use crate::automation::binding::{materialize_step, resolve_binding, BindingLimits};
use crate::automation::block::definition_for_action;
use crate::automation::context::{
    Binding, ContextOrigin, ContextPathSegment, ContextProvenance, ContextScope, ContextStore,
    ContextType, ContextValue, FieldRef, ResolvedSchemaOwned, Sensitivity, TemplatePart,
};
use crate::automation::expression::{
    check_rule, ExpressionLimits, ExpressionV1, ReferenceV1, RuleEvaluation, RuleExprV1,
};
use crate::automation::graph::{
    ActionNode, EdgePort, ForEachNode, GraphNode, IfNode, JoinMode, JoinNode, LoopFailurePolicy,
    SwitchNode, WorkflowGraph,
};
use crate::automation::package_registry;
use crate::automation::task::{
    Action, AppBundleIdentity, AppStoreOperation, ArchiveFormat, AuthPolicy, ElevationPolicy,
    GithubContextInput, GithubRepositoryInput, IndeterminatePolicy, InspectPathAction,
    LicenseMethod, LicenseProvider, PathExpectation, PathKind, ReleaseChannel, ScriptInterpreter,
    ShellMode, Step, StepCondition, Task, WriteConflictPolicy,
};
use crate::github::get_account_repositories;
use crate::ppstore::{self, InstallOutcome};
use crate::rules::expand_path_template;
use crate::safety::{is_safe_rule_root, stays_under_root};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::IsTerminal;
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Error)]
pub enum AutomationError {
    #[error("{0}")]
    Message(String),
}

/// Runtime-only consent for one exact protected Git destination.
///
/// Values can only be created from a [`ProtectedPathApprovalRequest`]. They
/// deliberately do not implement serde traits: callers must keep consent in
/// memory for the current plan/apply cycle and must never persist it in a task
/// or project file.
#[derive(Debug, Clone)]
pub struct ProtectedPathApproval {
    task_id: String,
    step_id: String,
    operation: ProtectedPathOperation,
    repository: String,
    branch: Option<String>,
    snapshot: ProtectedPathSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtectedPathOperation {
    GitCloneOrUpdate,
    GitCloneIfMissing,
    GitFetch,
    GitFastForward,
}

impl ProtectedPathOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitCloneOrUpdate => "git-clone-or-update",
            Self::GitCloneIfMissing => "git-clone-if-missing",
            Self::GitFetch => "git-fetch",
            Self::GitFastForward => "git-fast-forward",
        }
    }
}

impl std::fmt::Display for ProtectedPathOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtectedPathRisk {
    UserDocuments,
}

impl ProtectedPathRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserDocuments => "user-documents",
        }
    }
}

#[derive(Debug, Clone)]
struct ProtectedPathSnapshot {
    requested_path: PathBuf,
    resolved_path: PathBuf,
    anchor_path: PathBuf,
    anchor_identity: Arc<same_file::Handle>,
}

/// A typed, exact consent request surfaced through
/// [`ProtectedPathApprovalRequired`].
///
/// The request owns an open identity handle for the deepest existing ancestor
/// observed during validation. Calling [`Self::approve`] rechecks that both the
/// resolved destination and that ancestor are unchanged before issuing an
/// opaque runtime token.
#[derive(Debug, Clone)]
pub struct ProtectedPathApprovalRequest {
    task_id: String,
    step_id: String,
    operation: ProtectedPathOperation,
    repository: String,
    branch: Option<String>,
    risk: ProtectedPathRisk,
    snapshot: ProtectedPathSnapshot,
}

impl ProtectedPathApprovalRequest {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn step_id(&self) -> &str {
        &self.step_id
    }

    pub fn operation(&self) -> ProtectedPathOperation {
        self.operation
    }

    pub fn expected_repository(&self) -> &str {
        &self.repository
    }

    pub fn expected_branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub fn requested_path(&self) -> &Path {
        &self.snapshot.requested_path
    }

    pub fn resolved_path(&self) -> &Path {
        &self.snapshot.resolved_path
    }

    pub fn risk(&self) -> ProtectedPathRisk {
        self.risk
    }

    pub fn approve(&self) -> Result<ProtectedPathApproval> {
        revalidate_protected_path_snapshot(&self.snapshot).with_context(|| {
            format!(
                "protected destination changed before approval: {}",
                self.snapshot.requested_path.display()
            )
        })?;
        Ok(ProtectedPathApproval {
            task_id: self.task_id.clone(),
            step_id: self.step_id.clone(),
            operation: self.operation,
            repository: self.repository.clone(),
            branch: self.branch.clone(),
            snapshot: self.snapshot.clone(),
        })
    }
}

#[derive(Debug, Clone, Error)]
#[error(
    "explicit approval required for {operation} destination {path} under Documents",
    operation = .request.operation,
    path = .request.snapshot.resolved_path.display()
)]
pub struct ProtectedPathApprovalRequired {
    request: ProtectedPathApprovalRequest,
}

impl ProtectedPathApprovalRequired {
    pub fn request(&self) -> &ProtectedPathApprovalRequest {
        &self.request
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub apply: bool,
    pub allow_shell: bool,
    pub allow_elevation: bool,
    pub release_channel: Option<ReleaseChannel>,
    /// Ephemeral, exact path approvals for the current plan/apply cycle.
    /// Callers are responsible for dropping these when the project or plan is
    /// invalidated.
    pub protected_path_approvals: Vec<ProtectedPathApproval>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionPlan {
    pub step_id: String,
    pub step_name: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prerequisites: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    Pending,
    Running,
    WaitingForAttention,
    Skipped,
    Satisfied,
    Applied,
    Failed,
}

impl StepStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::WaitingForAttention => "waiting-for-attention",
            Self::Skipped => "skipped",
            Self::Satisfied => "satisfied",
            Self::Applied => "applied",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StepLogEntry {
    pub step_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub step_id: String,
    pub step_name: String,
    pub summary: String,
    pub status: StepStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prerequisites: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<StepLogEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<StepOutput>,
}

impl StepReport {
    /// Stable runtime envelope exposed as `steps.<id>`. Action output is
    /// nested below `output`; status metadata remains available even when an
    /// action was skipped or failed before producing an action-specific value.
    pub fn context_value(&self) -> serde_json::Value {
        let output = self
            .output
            .as_ref()
            .and_then(|output| output.context_value().ok())
            .unwrap_or(serde_json::Value::Null);
        let error = matches!(self.status, StepStatus::Failed).then(|| {
            let message = self
                .logs
                .last()
                .map(|entry| entry.message.as_str())
                .unwrap_or(self.summary.as_str());
            serde_json::json!({
                "code": "step-failed",
                "message": message,
                "step_id": self.step_id,
                "retryable": false,
            })
        });
        serde_json::json!({
            "status": self.status.as_str(),
            "changed": matches!(self.status, StepStatus::Applied),
            "summary": self.summary,
            "output": output,
            "error": error,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum StepOutput {
    GithubRepositories(GithubRepositoriesOutput),
    PathMetadata(PathMetadataOutput),
    ProcessExit(ProcessExitOutput),
    /// Schema-addressed output for actions whose result is represented by the
    /// shared context contract rather than a bespoke Rust structure.
    ///
    /// `schema_id` is stable and versioned. `value` is validated against the
    /// corresponding block output schema before it is exposed to bindings.
    Structured(StructuredStepOutput),
}

impl StepOutput {
    /// Return the action-owned value without the enum's serde transport
    /// envelope. Context paths always start at this value.
    pub fn context_value(&self) -> Result<serde_json::Value> {
        match self {
            Self::Structured(output) => Ok(output.value.clone()),
            other => {
                let serialized = serde_json::to_value(other).context("serialize step output")?;
                serialized
                    .get("value")
                    .cloned()
                    .context("step output has no value")
            }
        }
    }

    pub fn schema_id(&self) -> &str {
        match self {
            Self::GithubRepositories(_) => "ppduster.github.repositories@1",
            Self::PathMetadata(_) => "ppduster.path.metadata@1",
            Self::ProcessExit(_) => "ppduster.process.exit@1",
            Self::Structured(output) => &output.schema_id,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StructuredStepOutput {
    pub schema_id: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct GithubRepositoriesOutput {
    pub github: GithubContextOutput,
}

#[derive(Debug, Clone, Serialize)]
pub struct GithubContextOutput {
    pub account: GithubAccountOutput,
    pub repositories: Vec<GithubRepositoryOutput>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GithubAccountOutput {
    pub login: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GithubRepositoryOutput {
    /// Opaque GitHub GraphQL node ID; it is not a workflow identifier.
    pub id: String,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub https_url: String,
    pub ssh_url: String,
    pub default_branch: Option<String>,
    pub private: bool,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathMetadataOutput {
    pub path: PathBuf,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<PathKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessExitOutput {
    /// The interpreter's normal process exit code. On Windows this preserves
    /// the full unsigned DWORD value exposed by ExitStatus as a signed i32.
    pub exit_code: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_signal: Option<i32>,
    pub accepted: bool,
    pub success_exit_codes: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionOutcome {
    Planned { action: ActionPlan },
    AlreadySatisfied { reason: String },
    Observed { summary: String },
    Applied { summary: String },
    Skipped { reason: String },
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub task_id: String,
    pub task_name: String,
    pub task_description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scenarios: Vec<String>,
    pub plans: Vec<ActionPlan>,
    pub outcomes: Vec<ActionOutcome>,
    pub steps: Vec<StepReport>,
    /// Validated action outputs keyed by stable step IDs. Transport metadata
    /// from [`StepOutput`] is deliberately absent: bindings start directly at
    /// the output contract (`github.repositories`, `repository.path`, ...).
    pub context: ContextStore,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Compile completed step outputs into the runtime context used by bindings
/// and rules. Every value is checked against the same block contract that the
/// visual editor exposes; a producer cannot publish a differently shaped
/// value under a trusted schema ID.
pub fn context_store_from_reports(task: &Task, reports: &[StepReport]) -> Result<ContextStore> {
    let graph = task
        .workflow_graph()
        .map_err(|error| anyhow!("read task {} workflow graph: {error}", task.id))?;
    let mut steps_by_id = BTreeMap::new();
    collect_graph_action_steps(graph, &mut steps_by_id);
    context_store_from_step_map(&steps_by_id, reports)
}

fn collect_graph_action_steps<'a>(
    graph: &'a WorkflowGraph,
    steps: &mut BTreeMap<&'a str, &'a Step>,
) {
    for node in &graph.nodes {
        match node {
            GraphNode::Action(node) => {
                steps.insert(node.step.id.as_str(), &node.step);
            }
            GraphNode::ForEach(node) => collect_graph_action_steps(&node.body, steps),
            GraphNode::If(node) => {
                collect_graph_action_steps(&node.then_graph, steps);
                if let Some(graph) = node.else_graph.as_deref() {
                    collect_graph_action_steps(graph, steps);
                }
            }
            GraphNode::Switch(node) => {
                for case in &node.cases {
                    collect_graph_action_steps(&case.graph, steps);
                }
                if let Some(graph) = node.default.as_deref() {
                    collect_graph_action_steps(graph, steps);
                }
            }
            GraphNode::Join(_) => {}
        }
    }
}

fn context_store_from_steps(steps: &[Step], reports: &[StepReport]) -> Result<ContextStore> {
    let steps_by_id = steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect::<BTreeMap<_, _>>();
    context_store_from_step_map(&steps_by_id, reports)
}

fn context_store_from_step_map(
    steps_by_id: &BTreeMap<&str, &Step>,
    reports: &[StepReport],
) -> Result<ContextStore> {
    let mut store = ContextStore::default();

    for report in reports {
        let Some(output) = &report.output else {
            continue;
        };
        // Nested legacy foreach reports have instance IDs such as `clone[1]`.
        // They are intentionally not promoted to the parent scope; Graph v3
        // gives every nested instance an explicit scope path.
        let Some(step) = steps_by_id.get(report.step_id.as_str()) else {
            continue;
        };
        let definition = definition_for_action(&step.action);
        let expected_schema_id = definition
            .output_schema_id()
            .with_context(|| format!("block {} has no output schema id", definition.kind.id()))?;
        if output.schema_id() != expected_schema_id {
            bail!(
                "step {} published schema {}, expected {}",
                report.step_id,
                output.schema_id(),
                expected_schema_id
            );
        }
        let value = output
            .context_value()
            .with_context(|| format!("read output context for step {}", report.step_id))?;
        definition
            .output_schema
            .validate_value(&value)
            .with_context(|| {
                format!(
                    "step {} output violates schema {}",
                    report.step_id, expected_schema_id
                )
            })?;
        store.insert(
            ContextScope::Step {
                step_id: report.step_id.clone(),
            },
            ContextValue::new(value, ContextProvenance::step(&report.step_id))
                .with_schema(definition.output_schema),
        );
    }

    Ok(store)
}

fn context_schema_store_before(steps: &[Step], consumer_step_id: &str) -> Result<ContextStore> {
    let mut store = ContextStore::default();
    let mut found_consumer = false;
    for step in steps {
        if step.id == consumer_step_id {
            found_consumer = true;
            break;
        }
        let definition = definition_for_action(&step.action);
        store.insert(
            ContextScope::Step {
                step_id: step.id.clone(),
            },
            // The checker reads only the attached schema. Null makes an
            // accidental value lookup fail closed instead of inventing data.
            ContextValue::new(serde_json::Value::Null, ContextProvenance::step(&step.id))
                .with_schema(definition.output_schema),
        );
    }
    if !found_consumer {
        bail!("condition consumer step {consumer_step_id} is not in task")
    }
    Ok(store)
}

#[derive(Debug, Default)]
struct AuthState {
    git_authenticated: bool,
    sudo_authenticated: bool,
}

#[derive(Debug)]
enum ApplyStepResult {
    Applied(String),
    AppliedWithOutput {
        summary: String,
        output: StepOutput,
    },
    AlreadySatisfied(String),
    AlreadySatisfiedWithOutput {
        summary: String,
        output: StepOutput,
    },
    Failed {
        summary: String,
        error: String,
        output: Option<StepOutput>,
    },
}

fn structured_step_output(schema_id: impl Into<String>, value: serde_json::Value) -> StepOutput {
    StepOutput::Structured(StructuredStepOutput {
        schema_id: schema_id.into(),
        value,
    })
}

fn context_path(raw: &str) -> String {
    expand_path_template(raw)
        .unwrap_or_else(|| PathBuf::from(raw))
        .to_string_lossy()
        .into_owned()
}

/// Build the stable, action-specific result for actions that predate typed
/// outputs. This function is deliberately infallible: publishing context must
/// never turn an already completed mutation into a failed step.
fn legacy_action_output(step: &Step, changed: bool) -> Option<StepOutput> {
    let output = match &step.action {
        Action::GithubListRepositories
        | Action::GithubSelectRepositories { .. }
        | Action::InspectPath(_)
        | Action::RunCommand { .. }
        | Action::RunScript { .. } => return None,
        Action::ForEach { .. } | Action::ForEachGitCloneIfMissing { .. } => return None,
        Action::CreateDirectory(action) => structured_step_output(
            "ppduster.filesystem.create-directory@1",
            serde_json::json!({
                "path": {
                    "value": context_path(&action.path),
                    "exists": true,
                    "kind": "directory",
                    "created": changed,
                    "changed": changed,
                }
            }),
        ),
        Action::CopyPath(action) => structured_step_output(
            "ppduster.filesystem.copy-path@1",
            serde_json::json!({
                "path": {
                    "source": context_path(&action.src),
                    "destination": context_path(&action.dest),
                    "copied": true,
                    "changed": changed,
                }
            }),
        ),
        Action::WriteFile(action) => {
            let sha256 = hex::encode(Sha256::digest(action.content.as_bytes()));
            structured_step_output(
                "ppduster.filesystem.write-file@1",
                serde_json::json!({
                    "file": {
                        "path": context_path(&action.path),
                        "bytes": action.content.len(),
                        "sha256": sha256,
                        "created": changed,
                        "changed": changed,
                    }
                }),
            )
        }
        Action::RemovePath(action) => structured_step_output(
            "ppduster.filesystem.remove-path@1",
            serde_json::json!({
                "path": {
                    "value": context_path(&action.path),
                    "exists": false,
                    "removed": changed,
                    "changed": changed,
                }
            }),
        ),
        Action::GitClone { repo, dest, branch } => structured_step_output(
            "ppduster.git.clone@1",
            repository_context_value(repo, dest, branch.as_deref(), changed, "sync"),
        ),
        // Git inspection publishes an observation produced by
        // `apply_git_inspect`. Deriving it here from `Path::exists` would
        // incorrectly classify an empty directory as a repository.
        Action::GitInspect { .. } => return None,
        Action::GitCloneIfMissing { repo, dest, branch } => structured_step_output(
            "ppduster.git.clone-if-missing@1",
            repository_context_value(repo, dest, branch.as_deref(), changed, "clone-if-missing"),
        ),
        Action::GitFetch { repo, dest, branch } => structured_step_output(
            "ppduster.git.fetch@1",
            repository_context_value(repo, dest, Some(branch), changed, "fetch"),
        ),
        Action::GitFastForward { repo, dest, branch } => structured_step_output(
            "ppduster.git.fast-forward@1",
            repository_context_value(repo, dest, Some(branch), changed, "fast-forward"),
        ),
        Action::BrewInstall { package, cask } => structured_step_output(
            "ppduster.package.brew-install@1",
            serde_json::json!({
                "package": {
                    "name": package,
                    "cask": cask,
                    "installed": true,
                    "changed": changed,
                }
            }),
        ),
        Action::ConfigurePackageRegistryFiles { npm, nuget, .. } => structured_step_output(
            "ppduster.configuration.package-registries@1",
            serde_json::json!({
                "configuration": {
                    "npm_scope": npm.scope,
                    "npm_registry": npm.registry,
                    "nuget_public_source": nuget.public_source,
                    "nuget_private_source": nuget.source,
                    "changed": changed,
                    "secrets_redacted": true,
                }
            }),
        ),
        Action::DownloadFile {
            url,
            dest,
            checksum,
        } => structured_step_output(
            "ppduster.artifact.download@1",
            serde_json::json!({
                "artifact": {
                    "url": url,
                    "path": context_path(dest),
                    "sha256": checksum.sha256,
                    "downloaded": changed,
                    "verified": true,
                    "changed": changed,
                }
            }),
        ),
        Action::ExtractArchive {
            src, dest, format, ..
        } => structured_step_output(
            "ppduster.artifact.extract@1",
            serde_json::json!({
                "archive": {
                    "source": context_path(src),
                    "destination": context_path(dest),
                    "format": archive_format_name(*format),
                    "extracted": true,
                    "changed": changed,
                }
            }),
        ),
        Action::InstallDmg {
            dmg,
            app_name,
            target,
            identity,
        } => structured_step_output(
            "ppduster.installation.dmg@1",
            serde_json::json!({
                "installation": {
                    "source": context_path(dmg),
                    "target": context_path(target.as_deref().unwrap_or("$HOME/Applications")),
                    "app_name": app_name,
                    "bundle_identifier": identity.as_ref().map(|value| value.bundle_identifier.as_str()),
                    "version": identity.as_ref().map(|value| value.version.as_str()),
                    "installed": true,
                    "changed": changed,
                }
            }),
        ),
        Action::InstallPkg { pkg, target } => structured_step_output(
            "ppduster.installation.pkg@1",
            serde_json::json!({
                "installation": {
                    "source": context_path(pkg),
                    "target": target.as_deref().unwrap_or("/"),
                    "installed": true,
                    "changed": changed,
                }
            }),
        ),
        Action::MacosRequirements {
            minimum_version,
            require_rosetta_on_apple_silicon,
        } => structured_step_output(
            "ppduster.system.macos-requirements@1",
            serde_json::json!({
                "system": {
                    "platform": "macos",
                    "minimum_version": minimum_version,
                    "rosetta_required": require_rosetta_on_apple_silicon,
                    "satisfied": true,
                    "changed": changed,
                }
            }),
        ),
        Action::AppStoreInstall(action) => structured_step_output(
            "ppduster.installation.app-store@1",
            serde_json::json!({
                "application": {
                    "id": action.app_id,
                    "operation": app_store_operation_name(action.operation),
                    "installed": true,
                    "changed": changed,
                }
            }),
        ),
        Action::BambuStudioRelease(action) => structured_step_output(
            "ppduster.installation.bambu-studio@1",
            serde_json::json!({
                "application": {
                    "name": "Bambu Studio",
                    "channel": release_channel_name(action.channel),
                    "installed": true,
                    "changed": changed,
                }
            }),
        ),
        Action::ActivateLicense(action) => structured_step_output(
            "ppduster.license.activation@1",
            serde_json::json!({
                "license": {
                    "provider": match action.provider { LicenseProvider::LightBurn => "lightburn" },
                    "method": match action.method { LicenseMethod::VendorUi => "vendor-ui" },
                    "activated": true,
                    "changed": changed,
                    "secret_exposed": false,
                }
            }),
        ),
    };
    Some(output)
}

fn repository_context_value(
    repo: &str,
    dest: &str,
    branch: Option<&str>,
    changed: bool,
    operation: &str,
) -> serde_json::Value {
    let path = context_path(dest);
    let exists = Path::new(&path).exists();
    serde_json::json!({
        "repository": {
            "path": path,
            "remote_url": repo,
            "branch": branch,
            "exists": exists,
            "operation": operation,
            "cloned": operation == "clone-if-missing" && changed,
            "fetched": operation == "fetch",
            "updated": (operation == "sync" || operation == "fast-forward") && changed,
            "changed": changed,
        }
    })
}

fn git_inspection_output(
    repo: &str,
    dest: &str,
    branch: Option<&str>,
    repository_exists: bool,
) -> StepOutput {
    let mut value = repository_context_value(repo, dest, branch, false, "inspect");
    value["repository"]["exists"] = serde_json::Value::Bool(repository_exists);
    structured_step_output("ppduster.git.inspect@1", value)
}

pub fn run_task(task: &Task, opts: &RunOptions) -> Result<RunReport> {
    run_task_with_interactivity(task, opts, terminal_is_interactive())
}

fn run_task_with_interactivity(
    task: &Task,
    opts: &RunOptions,
    terminal_interactive: bool,
) -> Result<RunReport> {
    // Deserialization and TaskPack resolution are the compatibility boundary.
    // The runtime accepts only an already canonical graph and never infers
    // control flow from Task.steps.
    task.workflow_graph().map_err(|error| {
        anyhow!(
            "task {} is not a canonical workflow graph: {error}",
            task.id
        )
    })?;
    run_graph_task_with_interactivity(task, opts, terminal_interactive)
}

/// Execute an already materialized sequence of atomic actions.
///
/// Graph execution uses this with a one-element slice. Keeping the action
/// machinery independent from `Task.steps` prevents a graph action from
/// constructing a synthetic legacy task merely to reuse the platform
/// adapters. The sequence form remains useful for evaluating the legacy
/// condition/action behavior in focused regression tests.
fn run_step_sequence_with_interactivity(
    task: &Task,
    task_steps: &[Step],
    opts: &RunOptions,
    terminal_interactive: bool,
) -> Result<RunReport> {
    if task_steps.is_empty() {
        bail!("task {} has no executable actions", task.id);
    }
    for step in task_steps {
        step.validate().map_err(AutomationError::Message)?;
    }
    if opts.release_channel.is_some()
        && !task_steps
            .iter()
            .any(|step| matches!(step.action, Action::BambuStudioRelease(_)))
    {
        bail!("--channel is only supported by tasks with a bambu-studio-release step");
    }
    // Validate every policy gate before the first applied step so a missing
    // acknowledgement cannot leave a task partially applied.
    for step in task_steps {
        enforce_step_policy(&task.id, step, opts, terminal_interactive)?;
    }

    let mut plans = Vec::new();
    let mut outcomes = Vec::new();
    let mut steps = Vec::new();
    let mut errors = Vec::new();
    let mut auth_state = AuthState::default();
    let mut loop_contexts: BTreeMap<String, (String, Vec<serde_json::Value>)> = BTreeMap::new();
    let mut halted = false;

    for step in task_steps {
        if halted {
            let plan = plan_step(step, opts)?;
            plans.push(plan.clone());
            outcomes.push(ActionOutcome::Blocked);
            steps.push(StepReport {
                step_id: step.id.clone(),
                step_name: step_name(step),
                summary: plan.summary.clone(),
                status: StepStatus::Skipped,
                prerequisites: plan.prerequisites.clone(),
                logs: vec![StepLogEntry {
                    step_id: step.id.clone(),
                    message: "skipped after earlier failure".into(),
                }],
                output: None,
            });
            continue;
        }
        if opts.apply {
            if let Some(condition) = &step.when {
                match evaluate_condition(condition, &steps, task_steps, &step.id) {
                    Ok(ConditionEvaluation::Matched(_)) => {}
                    Ok(ConditionEvaluation::NotMatched(reason))
                    | Ok(ConditionEvaluation::Unavailable(reason)) => {
                        let plan = plan_step(step, opts)?;
                        plans.push(plan.clone());
                        outcomes.push(ActionOutcome::Skipped {
                            reason: reason.clone(),
                        });
                        steps.push(StepReport {
                            step_id: step.id.clone(),
                            step_name: step_name(step),
                            summary: plan.summary,
                            status: StepStatus::Skipped,
                            prerequisites: plan.prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: step.id.clone(),
                                message: format!("when condition not met: {reason}"),
                            }],
                            output: None,
                        });
                        continue;
                    }
                    Err(error) => {
                        let plan = plan_step(step, opts)?;
                        let message = format!(
                            "step {} when condition failed to evaluate: {error:#}",
                            step.id
                        );
                        plans.push(plan.clone());
                        outcomes.push(ActionOutcome::Blocked);
                        steps.push(StepReport {
                            step_id: step.id.clone(),
                            step_name: step_name(step),
                            summary: plan.summary,
                            status: StepStatus::Failed,
                            prerequisites: plan.prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: step.id.clone(),
                                message: format!("failed: {message}"),
                            }],
                            output: None,
                        });
                        errors.push(message);
                        halted = true;
                        continue;
                    }
                }
            }
            if let Some(condition) = &step.require {
                match evaluate_condition(condition, &steps, task_steps, &step.id) {
                    Ok(ConditionEvaluation::Matched(_)) => {}
                    Ok(ConditionEvaluation::NotMatched(reason))
                    | Ok(ConditionEvaluation::Unavailable(reason)) => {
                        let plan = plan_step(step, opts)?;
                        let message =
                            format!("step {} required condition was not met: {reason}", step.id);
                        plans.push(plan.clone());
                        outcomes.push(ActionOutcome::Blocked);
                        steps.push(StepReport {
                            step_id: step.id.clone(),
                            step_name: step_name(step),
                            summary: plan.summary,
                            status: StepStatus::Failed,
                            prerequisites: plan.prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: step.id.clone(),
                                message: format!("failed: {message}"),
                            }],
                            output: None,
                        });
                        errors.push(message);
                        halted = true;
                        continue;
                    }
                    Err(error) => {
                        let plan = plan_step(step, opts)?;
                        let message = format!(
                            "step {} required condition failed to evaluate: {error:#}",
                            step.id
                        );
                        plans.push(plan.clone());
                        outcomes.push(ActionOutcome::Blocked);
                        steps.push(StepReport {
                            step_id: step.id.clone(),
                            step_name: step_name(step),
                            summary: plan.summary,
                            status: StepStatus::Failed,
                            prerequisites: plan.prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: step.id.clone(),
                                message: format!("failed: {message}"),
                            }],
                            output: None,
                        });
                        errors.push(message);
                        halted = true;
                        continue;
                    }
                }
            }
        }
        if opts.apply {
            if let Action::ForEach {
                source_step,
                array_path,
                item,
                fields,
            } = &step.action
            {
                let plan = plan_step(step, opts)?;
                plans.push(plan.clone());
                match resolve_for_each_items(source_step, array_path, fields, &steps) {
                    Ok(items) => {
                        let count = items.len();
                        let output_items = items.clone();
                        loop_contexts.insert(step.id.clone(), (item.clone(), items));
                        let summary = format!(
                            "prepared {count} iteration(s) from {source_step}.{array_path} as {item}"
                        );
                        steps.push(StepReport {
                            step_id: step.id.clone(),
                            step_name: step_name(step),
                            summary: summary.clone(),
                            status: StepStatus::Applied,
                            prerequisites: plan.prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: step.id.clone(),
                                message: summary.clone(),
                            }],
                            output: Some(structured_step_output(
                                "ppduster.control.for-each@1",
                                serde_json::json!({
                                    "loop": {
                                        "source_step": source_step,
                                        "array_path": array_path,
                                        "item_alias": item,
                                        "count": count,
                                        "items": output_items,
                                    }
                                }),
                            )),
                        });
                        outcomes.push(ActionOutcome::Applied { summary });
                    }
                    Err(error) => {
                        let message = format!("step {} failed to prepare loop: {error:#}", step.id);
                        steps.push(StepReport {
                            step_id: step.id.clone(),
                            step_name: step_name(step),
                            summary: plan.summary,
                            status: StepStatus::Failed,
                            prerequisites: plan.prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: step.id.clone(),
                                message: format!("failed: {message}"),
                            }],
                            output: None,
                        });
                        errors.push(message);
                        outcomes.push(ActionOutcome::Blocked);
                        halted = true;
                    }
                }
                continue;
            }
            if let Action::ForEachGitCloneIfMissing {
                loop_step,
                repo,
                dest,
                branch,
            } = &step.action
            {
                let plan = plan_step(step, opts)?;
                plans.push(plan.clone());
                let Some((item_alias, items)) = loop_contexts.get(loop_step) else {
                    let message = format!(
                        "step {} cannot find executed for-each step {}",
                        step.id, loop_step
                    );
                    steps.push(failed_step_report(step, &plan, &message));
                    errors.push(message);
                    outcomes.push(ActionOutcome::Blocked);
                    halted = true;
                    continue;
                };
                let mut logs = vec![StepLogEntry {
                    step_id: step.id.clone(),
                    message: format!("running {} iteration(s)", items.len()),
                }];
                let mut applied = 0usize;
                let mut satisfied = 0usize;
                let mut failure = None;
                let mut iteration_steps = Vec::with_capacity(items.len());
                for (index, value) in items.iter().enumerate() {
                    let iteration = index + 1;
                    let rendered = (|| -> Result<Step> {
                        let repo = render_item_template(repo, item_alias, value)?;
                        let dest = render_item_template(dest, item_alias, value)?;
                        let branch = branch
                            .as_deref()
                            .map(|value_template| {
                                render_optional_item_template(value_template, item_alias, value)
                            })
                            .transpose()?
                            .flatten();
                        Ok(Step {
                            id: format!("{}[{iteration}]", step.id),
                            name: format!("{} · {iteration}", step_name(step)),
                            bindings: BTreeMap::new(),
                            auth: step.auth,
                            check: step.check.clone(),
                            dangerous: step.dangerous,
                            allow_elevation: step.allow_elevation,
                            when: None,
                            require: None,
                            action: Action::GitCloneIfMissing { repo, dest, branch },
                        })
                    })();
                    let iteration_step = match rendered {
                        Ok(iteration_step) => iteration_step,
                        Err(error) => {
                            failure = Some(format!("iteration {iteration}: {error:#}"));
                            break;
                        }
                    };
                    if let Err(error) = iteration_step.validate() {
                        failure = Some(format!("iteration {iteration}: {error}"));
                        break;
                    }
                    if let Err(error) =
                        enforce_step_policy(&task.id, &iteration_step, opts, terminal_interactive)
                    {
                        if error
                            .downcast_ref::<ProtectedPathApprovalRequired>()
                            .is_some()
                        {
                            return Err(error).with_context(|| {
                                format!("iteration {iteration} requires protected path approval")
                            });
                        }
                        failure = Some(format!("iteration {iteration}: {error:#}"));
                        break;
                    }
                    iteration_steps.push(iteration_step);
                }
                if failure.is_none() {
                    for (index, iteration_step) in iteration_steps.iter().enumerate() {
                        let iteration = index + 1;
                        if step_requires_auth_prompt(iteration_step, &auth_state)? {
                            if let Err(error) = ensure_auth(iteration_step, &mut auth_state) {
                                failure = Some(format!("iteration {iteration}: {error}"));
                                break;
                            }
                        }
                        match is_satisfied(iteration_step, true)? {
                            Some(reason) => {
                                satisfied += 1;
                                logs.push(StepLogEntry {
                                    step_id: iteration_step.id.clone(),
                                    message: reason,
                                });
                            }
                            None => match apply_step(&task.id, iteration_step, opts) {
                                Ok(ApplyStepResult::Applied(summary))
                                | Ok(ApplyStepResult::AppliedWithOutput { summary, .. }) => {
                                    applied += 1;
                                    logs.push(StepLogEntry {
                                        step_id: iteration_step.id.clone(),
                                        message: summary,
                                    });
                                }
                                Ok(ApplyStepResult::AlreadySatisfied(summary)) => {
                                    satisfied += 1;
                                    logs.push(StepLogEntry {
                                        step_id: iteration_step.id.clone(),
                                        message: summary,
                                    });
                                }
                                Ok(ApplyStepResult::AlreadySatisfiedWithOutput {
                                    summary, ..
                                }) => {
                                    satisfied += 1;
                                    logs.push(StepLogEntry {
                                        step_id: iteration_step.id.clone(),
                                        message: summary,
                                    });
                                }
                                Ok(ApplyStepResult::Failed { error, .. }) => {
                                    failure = Some(format!("iteration {iteration}: {error}"));
                                    break;
                                }
                                Err(error) => {
                                    if error
                                        .downcast_ref::<ProtectedPathApprovalRequired>()
                                        .is_some()
                                    {
                                        return Err(error).with_context(|| {
                                            format!(
                                                "iteration {iteration} requires renewed protected path approval"
                                            )
                                        });
                                    }
                                    failure = Some(format!("iteration {iteration}: {error:#}"));
                                    break;
                                }
                            },
                        }
                    }
                }
                if let Some(message) = failure {
                    logs.push(StepLogEntry {
                        step_id: step.id.clone(),
                        message: format!("failed: {message}"),
                    });
                    steps.push(StepReport {
                        step_id: step.id.clone(),
                        step_name: step_name(step),
                        summary: plan.summary,
                        status: StepStatus::Failed,
                        prerequisites: plan.prerequisites,
                        logs,
                        output: Some(structured_step_output(
                            "ppduster.control.for-each-results@1",
                            serde_json::json!({
                                "loop": {
                                    "source_step": loop_step,
                                    "count": items.len(),
                                    "applied": applied,
                                    "satisfied": satisfied,
                                    "failed": true,
                                    "error": message,
                                }
                            }),
                        )),
                    });
                    errors.push(format!("step {} {message}", step.id));
                    outcomes.push(ActionOutcome::Blocked);
                    halted = true;
                } else {
                    let summary = format!(
                        "completed {} iteration(s): {applied} cloned, {satisfied} already present",
                        items.len()
                    );
                    logs.push(StepLogEntry {
                        step_id: step.id.clone(),
                        message: summary.clone(),
                    });
                    steps.push(StepReport {
                        step_id: step.id.clone(),
                        step_name: step_name(step),
                        summary: summary.clone(),
                        status: if applied > 0 {
                            StepStatus::Applied
                        } else {
                            StepStatus::Satisfied
                        },
                        prerequisites: plan.prerequisites,
                        logs,
                        output: Some(structured_step_output(
                            "ppduster.control.for-each-results@1",
                            serde_json::json!({
                                "loop": {
                                    "source_step": loop_step,
                                    "count": items.len(),
                                    "applied": applied,
                                    "satisfied": satisfied,
                                    "failed": false,
                                }
                            }),
                        )),
                    });
                    outcomes.push(if applied > 0 {
                        ActionOutcome::Applied { summary }
                    } else {
                        ActionOutcome::AlreadySatisfied { reason: summary }
                    });
                }
                continue;
            }
        }
        // Unconditional typed inspections are deliberately read-only, so they
        // run during both a normal dry-run and an applied run. Conditional
        // inspections remain planned during dry-run because their prerequisite
        // process output does not exist until apply time.
        let should_observe_inspect = opts.apply || (step.when.is_none() && step.require.is_none());
        if should_observe_inspect {
            if let Action::InspectPath(action) = &step.action {
                let prerequisites = prerequisites_for_step(step);
                let planned_summary = describe_step(step, opts)?;
                match inspect_path(action) {
                    Ok(metadata) => {
                        let summary = summarize_path_metadata(&metadata);
                        let output = Some(StepOutput::PathMetadata(metadata.clone()));
                        let expectation = action
                            .expect
                            .as_ref()
                            .map(|expectation| verify_path_expectation(expectation, &metadata))
                            .transpose();
                        match expectation {
                            Ok(_) => {
                                steps.push(StepReport {
                                    step_id: step.id.clone(),
                                    step_name: step_name(step),
                                    summary: summary.clone(),
                                    status: StepStatus::Satisfied,
                                    prerequisites,
                                    logs: vec![StepLogEntry {
                                        step_id: step.id.clone(),
                                        message: summary.clone(),
                                    }],
                                    output,
                                });
                                outcomes.push(ActionOutcome::Observed { summary });
                            }
                            Err(error) => {
                                let message =
                                    format!("step {} path expectation failed: {error}", step.id);
                                steps.push(StepReport {
                                    step_id: step.id.clone(),
                                    step_name: step_name(step),
                                    summary,
                                    status: StepStatus::Failed,
                                    prerequisites,
                                    logs: vec![StepLogEntry {
                                        step_id: step.id.clone(),
                                        message: format!("failed: {message}"),
                                    }],
                                    output,
                                });
                                errors.push(message);
                                outcomes.push(ActionOutcome::Blocked);
                                halted = true;
                            }
                        }
                    }
                    Err(error) => {
                        let message = format!("step {} path inspection failed: {error:#}", step.id);
                        steps.push(StepReport {
                            step_id: step.id.clone(),
                            step_name: step_name(step),
                            summary: planned_summary,
                            status: StepStatus::Failed,
                            prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: step.id.clone(),
                                message: format!("failed: {message}"),
                            }],
                            output: None,
                        });
                        errors.push(message);
                        outcomes.push(ActionOutcome::Blocked);
                        halted = true;
                    }
                }
                continue;
            }
            if let Action::GitInspect { repo, dest } = &step.action {
                let prerequisites = prerequisites_for_step(step);
                let planned_summary = describe_step(step, opts)?;
                match apply_git_inspect(repo, dest) {
                    Ok(ApplyStepResult::AlreadySatisfiedWithOutput { summary, output }) => {
                        steps.push(StepReport {
                            step_id: step.id.clone(),
                            step_name: step_name(step),
                            summary: summary.clone(),
                            status: StepStatus::Satisfied,
                            prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: step.id.clone(),
                                message: summary.clone(),
                            }],
                            output: Some(output),
                        });
                        outcomes.push(ActionOutcome::Observed { summary });
                    }
                    Ok(_) => unreachable!("git inspection is read-only"),
                    Err(error) => {
                        let message = format!("step {} git inspection failed: {error:#}", step.id);
                        steps.push(StepReport {
                            step_id: step.id.clone(),
                            step_name: step_name(step),
                            summary: planned_summary,
                            status: StepStatus::Failed,
                            prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: step.id.clone(),
                                message: format!("failed: {message}"),
                            }],
                            output: None,
                        });
                        errors.push(message);
                        outcomes.push(ActionOutcome::Blocked);
                        halted = true;
                    }
                }
                continue;
            }
        }
        let satisfaction = is_satisfied(step, opts.apply)?;
        if let Some(reason) = satisfaction {
            let output = legacy_action_output(step, false);
            steps.push(StepReport {
                step_id: step.id.clone(),
                step_name: step_name(step),
                summary: describe_step(step, opts)?,
                status: StepStatus::Satisfied,
                prerequisites: prerequisites_for_step(step),
                logs: vec![StepLogEntry {
                    step_id: step.id.clone(),
                    message: reason.clone(),
                }],
                output,
            });
            outcomes.push(ActionOutcome::AlreadySatisfied { reason });
            continue;
        }
        let plan = plan_step(step, opts)?;
        plans.push(plan.clone());
        let step_idx = steps.len();
        steps.push(StepReport {
            step_id: step.id.clone(),
            step_name: step_name(step),
            summary: plan.summary.clone(),
            status: StepStatus::Pending,
            prerequisites: plan.prerequisites.clone(),
            logs: vec![StepLogEntry {
                step_id: step.id.clone(),
                message: "queued".into(),
            }],
            output: None,
        });
        if opts.apply {
            if step_requires_auth_prompt(step, &auth_state)? {
                steps[step_idx].status = StepStatus::WaitingForAttention;
                steps[step_idx].logs.push(StepLogEntry {
                    step_id: step.id.clone(),
                    message: "waiting for authorization".into(),
                });
                if let Err(err) = ensure_auth(step, &mut auth_state) {
                    let message = err.to_string();
                    steps[step_idx].status = StepStatus::Failed;
                    steps[step_idx].logs.push(StepLogEntry {
                        step_id: step.id.clone(),
                        message: format!("failed: {message}"),
                    });
                    errors.push(message.clone());
                    outcomes.push(ActionOutcome::Blocked);
                    halted = true;
                    continue;
                }
                steps[step_idx].logs.push(StepLogEntry {
                    step_id: step.id.clone(),
                    message: "authorization granted; resuming".into(),
                });
            }
            if matches!(
                &step.action,
                Action::ActivateLicense(_) | Action::AppStoreInstall(_)
            ) {
                steps[step_idx].status = StepStatus::WaitingForAttention;
                steps[step_idx].logs.push(StepLogEntry {
                    step_id: step.id.clone(),
                    message: match &step.action {
                        Action::ActivateLicense(_) => {
                            "waiting for license activation in the vendor UI".into()
                        }
                        Action::AppStoreInstall(_) => {
                            "waiting for any required App Store authentication".into()
                        }
                        _ => unreachable!(),
                    },
                });
            } else {
                steps[step_idx].status = StepStatus::Running;
            }
            steps[step_idx].logs.push(StepLogEntry {
                step_id: step.id.clone(),
                message: "running".into(),
            });
            let result = match apply_step(&task.id, step, opts) {
                Ok(result) => result,
                Err(err) => {
                    if err
                        .downcast_ref::<ProtectedPathApprovalRequired>()
                        .is_some()
                    {
                        return Err(err).with_context(|| {
                            format!(
                                "step {} requires renewed protected path approval during apply",
                                step.id
                            )
                        });
                    }
                    let message = err.to_string();
                    steps[step_idx].status = StepStatus::Failed;
                    steps[step_idx].logs.push(StepLogEntry {
                        step_id: step.id.clone(),
                        message: format!("failed: {message}"),
                    });
                    errors.push(message.clone());
                    outcomes.push(ActionOutcome::Blocked);
                    halted = true;
                    continue;
                }
            };
            let result = match result {
                ApplyStepResult::Failed {
                    summary,
                    error,
                    output,
                } => {
                    steps[step_idx].status = StepStatus::Failed;
                    steps[step_idx].summary = summary;
                    steps[step_idx].output = output;
                    steps[step_idx].logs.push(StepLogEntry {
                        step_id: step.id.clone(),
                        message: format!("failed: {error}"),
                    });
                    errors.push(error);
                    outcomes.push(ActionOutcome::Blocked);
                    halted = true;
                    continue;
                }
                result => result,
            };
            let (status, summary, applied, output) = match result {
                ApplyStepResult::Applied(summary) => (
                    StepStatus::Applied,
                    summary,
                    true,
                    legacy_action_output(step, true),
                ),
                ApplyStepResult::AppliedWithOutput { summary, output } => {
                    (StepStatus::Applied, summary, true, Some(output))
                }
                ApplyStepResult::AlreadySatisfied(summary) => (
                    StepStatus::Satisfied,
                    summary,
                    false,
                    legacy_action_output(step, false),
                ),
                ApplyStepResult::AlreadySatisfiedWithOutput { summary, output } => {
                    (StepStatus::Satisfied, summary, false, Some(output))
                }
                ApplyStepResult::Failed { .. } => unreachable!(),
            };
            steps[step_idx].status = status;
            steps[step_idx].summary = summary.clone();
            steps[step_idx].output = output;
            steps[step_idx].logs.push(StepLogEntry {
                step_id: step.id.clone(),
                message: summary.clone(),
            });
            outcomes.push(if applied {
                ActionOutcome::Applied { summary }
            } else {
                ActionOutcome::AlreadySatisfied { reason: summary }
            });
            continue;
        }
        outcomes.push(ActionOutcome::Planned { action: plan });
    }

    let context = context_store_from_steps(task_steps, &steps)?;

    Ok(RunReport {
        task_id: task.id.clone(),
        task_name: task.name.clone(),
        task_description: task.description.clone(),
        scenarios: task.included_scenarios().to_vec(),
        plans,
        outcomes,
        steps,
        context,
        errors,
    })
}

const GRAPH_MAX_DEPTH: usize = 32;
const GRAPH_MAX_NODE_ACTIVATIONS: usize = 4_096;
const GRAPH_MAX_LOOP_ITERATIONS: usize = 10_000;
const GRAPH_MAX_REPORTS: usize = 16_384;

#[derive(Debug, Clone, Default)]
struct GraphScopeState {
    values: ContextStore,
    schemas: ContextStore,
    aliases: BTreeMap<String, FieldRef>,
}

#[derive(Debug, Default)]
struct GraphRunAccumulator {
    plans: Vec<ActionPlan>,
    outcomes: Vec<ActionOutcome>,
    steps: Vec<StepReport>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct GraphExecutionBudget {
    node_activations: usize,
    loop_iterations: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphSignal {
    Successful,
    Failed,
    /// A structurally incoming path that was not selected at runtime. Join
    /// nodes treat it as a neutral completion, so an `all` barrier does not
    /// wait forever for the inactive half of a success/failure diamond.
    SkippedByControl,
}

impl GraphSignal {
    fn is_successful(self) -> bool {
        !matches!(self, Self::Failed)
    }
}

#[derive(Debug, Default)]
struct GraphInvocation {
    failed: bool,
}

#[derive(Debug)]
struct GraphNodeResult {
    ports: BTreeSet<EdgePort>,
    successful: bool,
    failed: bool,
}

impl GraphNodeResult {
    fn action_success() -> Self {
        Self {
            ports: BTreeSet::from([EdgePort::Success, EdgePort::Always]),
            successful: true,
            failed: false,
        }
    }

    fn action_skipped() -> Self {
        Self {
            // A false `when` is a successful no-op, matching v1 linear
            // semantics and allowing migrated `Success` chains to continue.
            ports: BTreeSet::from([EdgePort::Success, EdgePort::Always]),
            successful: true,
            failed: false,
        }
    }

    fn action_failure() -> Self {
        Self {
            ports: BTreeSet::from([EdgePort::Failure, EdgePort::Always]),
            successful: false,
            failed: true,
        }
    }

    fn action_planned() -> Self {
        Self {
            // Planning has no action outcome yet. Activate both conditional
            // routes so the report contains every statically reachable plan.
            ports: BTreeSet::from([EdgePort::Success, EdgePort::Failure, EdgePort::Always]),
            successful: true,
            failed: false,
        }
    }

    fn control_success() -> Self {
        Self {
            ports: BTreeSet::from([EdgePort::Completed]),
            successful: true,
            failed: false,
        }
    }

    fn control_failure() -> Self {
        Self {
            ports: BTreeSet::from([EdgePort::Failure]),
            successful: false,
            failed: true,
        }
    }

    fn control_planned(include_empty: bool) -> Self {
        let mut ports = BTreeSet::from([EdgePort::Completed, EdgePort::Failure]);
        if include_empty {
            ports.insert(EdgePort::Empty);
        }
        Self {
            ports,
            successful: true,
            failed: false,
        }
    }
}

struct GraphRuntime<'a> {
    task: &'a Task,
    opts: &'a RunOptions,
    terminal_interactive: bool,
    accumulator: GraphRunAccumulator,
    budget: GraphExecutionBudget,
}

fn run_graph_task_with_interactivity(
    task: &Task,
    opts: &RunOptions,
    terminal_interactive: bool,
) -> Result<RunReport> {
    let graph = task.workflow_graph().map_err(|error| {
        anyhow!(
            "task {} is not a canonical workflow graph: {error}",
            task.id
        )
    })?;
    if opts.release_channel.is_some() && !graph_contains_bambu(graph) {
        bail!("--channel is only supported by tasks with a bambu-studio-release action node");
    }
    preflight_graph_capabilities(&task.id, graph, opts, terminal_interactive)?;

    let mut runtime = GraphRuntime {
        task,
        opts,
        terminal_interactive,
        accumulator: GraphRunAccumulator::default(),
        budget: GraphExecutionBudget::default(),
    };
    let mut scope = GraphScopeState::default();
    let invocation = runtime.execute_graph(graph, &mut scope, "", 1)?;
    if invocation.failed && runtime.accumulator.errors.is_empty() {
        runtime
            .accumulator
            .errors
            .push("workflow graph finished through an unhandled failure path".into());
    }

    Ok(RunReport {
        task_id: task.id.clone(),
        task_name: task.name.clone(),
        task_description: task.description.clone(),
        scenarios: task.included_scenarios().to_vec(),
        plans: runtime.accumulator.plans,
        outcomes: runtime.accumulator.outcomes,
        steps: runtime.accumulator.steps,
        // Nested graph executions receive cloned stores. Only action outputs
        // produced directly in the root graph are therefore published here.
        context: scope.values,
        errors: runtime.accumulator.errors,
    })
}

fn graph_contains_bambu(graph: &WorkflowGraph) -> bool {
    graph.nodes.iter().any(|node| match node {
        GraphNode::Action(node) => matches!(node.step.action, Action::BambuStudioRelease(_)),
        GraphNode::ForEach(node) => graph_contains_bambu(&node.body),
        GraphNode::If(node) => {
            graph_contains_bambu(&node.then_graph)
                || node.else_graph.as_deref().is_some_and(graph_contains_bambu)
        }
        GraphNode::Switch(node) => {
            node.cases
                .iter()
                .any(|case| graph_contains_bambu(&case.graph))
                || node.default.as_deref().is_some_and(graph_contains_bambu)
        }
        GraphNode::Join(_) => false,
    })
}

fn preflight_graph_capabilities(
    task_id: &str,
    graph: &WorkflowGraph,
    opts: &RunOptions,
    terminal_interactive: bool,
) -> Result<()> {
    preflight_graph_policy_order(task_id, graph, opts, terminal_interactive, false).map(|_| ())
}

/// Prove that inputs which can fail late or change safety policy are checked
/// before the first possible mutation on their structural path. Literal
/// bindings are applied and validated immediately. A dynamic binding is
/// allowed on the first mutating action (its materialized policy is checked by
/// the one-step runner), and inside a `for-each` whose own two-phase preflight
/// runs before any iteration. Once a previous mutation is possible, an
/// unresolved safety input or positional lookup fails closed. Other dynamic
/// values retain guard-before-binding and loop preflight semantics.
fn preflight_graph_policy_order(
    task_id: &str,
    graph: &WorkflowGraph,
    opts: &RunOptions,
    terminal_interactive: bool,
    mut mutation_possible: bool,
) -> Result<bool> {
    for index in deterministic_graph_order(graph)? {
        let node = &graph.nodes[index];
        match node {
            GraphNode::Action(node) => {
                let static_bindings = node
                    .bindings
                    .iter()
                    .filter(|(_, binding)| binding_is_statically_resolvable(binding))
                    .map(|(target, binding)| (target.clone(), binding.clone()))
                    .collect::<BTreeMap<_, _>>();
                let step = materialize_step(
                    &node.step,
                    &static_bindings,
                    &ContextStore::default(),
                    BindingLimits::default(),
                )
                .with_context(|| {
                    format!(
                        "preflight static bindings for graph action {}",
                        node.step.id
                    )
                })?;
                // Keep the graph-wide safety barrier equivalent to the linear
                // runner's all-step preflight. In particular, destination and
                // existing-DMG identity checks must fail before an earlier
                // graph action can mutate the machine.
                enforce_step_policy(task_id, &step, opts, terminal_interactive)?;
                if mutation_possible {
                    if let Some(target) = node.bindings.iter().find_map(|(target, binding)| {
                        (!binding_is_statically_resolvable(binding)
                            && (binding_affects_preflight_policy(&node.step.action, target)
                                || binding_contains_positional_index(binding)))
                        .then_some(target)
                    }) {
                        bail!(
                            "step {} has runtime-bound input {target:?} after a possible earlier mutation; move it before mutating actions or place the mutations in one preflighted for-each",
                            node.step.id
                        );
                    }
                }
                mutation_possible |= !definition_for_action(&node.step.action).read_only;
            }
            GraphNode::ForEach(node) => {
                mutation_possible = preflight_graph_policy_order(
                    task_id,
                    &node.body,
                    opts,
                    terminal_interactive,
                    mutation_possible,
                )?;
            }
            GraphNode::If(node) => {
                let then_mutation = preflight_graph_policy_order(
                    task_id,
                    &node.then_graph,
                    opts,
                    terminal_interactive,
                    mutation_possible,
                )?;
                let else_mutation = if let Some(graph) = node.else_graph.as_deref() {
                    preflight_graph_policy_order(
                        task_id,
                        graph,
                        opts,
                        terminal_interactive,
                        mutation_possible,
                    )?
                } else {
                    mutation_possible
                };
                mutation_possible = then_mutation || else_mutation;
            }
            GraphNode::Switch(node) => {
                let mut branch_mutation = mutation_possible;
                for case in &node.cases {
                    branch_mutation |= preflight_graph_policy_order(
                        task_id,
                        &case.graph,
                        opts,
                        terminal_interactive,
                        mutation_possible,
                    )?;
                }
                if let Some(graph) = node.default.as_deref() {
                    branch_mutation |= preflight_graph_policy_order(
                        task_id,
                        graph,
                        opts,
                        terminal_interactive,
                        mutation_possible,
                    )?;
                }
                mutation_possible = branch_mutation;
            }
            GraphNode::Join(_) => {}
        }
    }
    Ok(mutation_possible)
}

fn binding_is_statically_resolvable(binding: &Binding) -> bool {
    match binding {
        Binding::Literal { .. } | Binding::Template { .. } => true,
        Binding::Field { .. } => false,
        Binding::Interpolated { parts } => parts
            .iter()
            .all(|part| matches!(part, TemplatePart::Literal { .. })),
    }
}

fn binding_contains_positional_index(binding: &Binding) -> bool {
    let field_has_index = |field: &FieldRef| {
        field
            .segments
            .iter()
            .any(|segment| matches!(segment, ContextPathSegment::Index { .. }))
    };
    match binding {
        Binding::Field { field } => field_has_index(field),
        Binding::Interpolated { parts } => parts
            .iter()
            .any(|part| matches!(part, TemplatePart::Field { field } if field_has_index(field))),
        Binding::Literal { .. } | Binding::Template { .. } => false,
    }
}

fn binding_affects_preflight_policy(action: &Action, target: &str) -> bool {
    let root = target
        .strip_prefix('/')
        .unwrap_or(target)
        .split(['/', '.'])
        .next()
        .unwrap_or_default();
    match action {
        Action::CreateDirectory(_) => root == "path",
        Action::InspectPath(_) => root == "path",
        Action::CopyPath(_) => matches!(root, "src" | "dest"),
        Action::WriteFile(_) | Action::RemovePath(_) => root == "path",
        Action::GitClone { .. }
        | Action::GitInspect { .. }
        | Action::GitCloneIfMissing { .. }
        | Action::GitFetch { .. }
        | Action::GitFastForward { .. } => matches!(root, "repo" | "dest"),
        Action::RunCommand { .. } => matches!(root, "program" | "cwd" | "shell"),
        Action::RunScript { .. } => matches!(root, "interpreter" | "script" | "cwd"),
        Action::ConfigurePackageRegistryFiles { .. }
        | Action::DownloadFile { .. }
        | Action::ExtractArchive { .. }
        | Action::InstallDmg { .. }
        | Action::InstallPkg { .. } => true,
        Action::GithubListRepositories
        | Action::GithubSelectRepositories { .. }
        | Action::ForEach { .. }
        | Action::ForEachGitCloneIfMissing { .. }
        | Action::BrewInstall { .. }
        | Action::MacosRequirements { .. }
        | Action::AppStoreInstall(_)
        | Action::BambuStudioRelease(_)
        | Action::ActivateLicense(_) => false,
    }
}

impl GraphRuntime<'_> {
    fn execute_graph(
        &mut self,
        graph: &WorkflowGraph,
        scope: &mut GraphScopeState,
        instance_prefix: &str,
        depth: usize,
    ) -> Result<GraphInvocation> {
        if depth > GRAPH_MAX_DEPTH {
            bail!("workflow graph nesting exceeds {GRAPH_MAX_DEPTH}");
        }
        let order = deterministic_graph_order(graph)?;
        let entries = graph.entries.iter().cloned().collect::<BTreeSet<_>>();
        let mut signals = BTreeMap::<String, Vec<GraphSignal>>::new();
        let mut emitted = BTreeSet::<(String, EdgePort)>::new();
        let mut invocation = GraphInvocation::default();

        for index in order {
            let node = &graph.nodes[index];
            let node_id = node.id();
            let incoming = signals.remove(node_id).unwrap_or_default();
            let is_entry = entries.contains(node_id);
            if !matches!(node, GraphNode::Join(_)) && !is_entry && incoming.is_empty() {
                continue;
            }
            if self.budget.node_activations >= GRAPH_MAX_NODE_ACTIVATIONS {
                bail!("workflow graph exceeds {GRAPH_MAX_NODE_ACTIVATIONS} node activations");
            }
            if self.accumulator.steps.len() >= GRAPH_MAX_REPORTS {
                bail!("workflow graph exceeds {GRAPH_MAX_REPORTS} step reports");
            }
            let error_checkpoint = self.accumulator.errors.len();
            let result = match node {
                GraphNode::Join(join) => {
                    let expected = graph
                        .edges
                        .iter()
                        .filter(|edge| edge.to.node == node_id)
                        .count();
                    self.execute_join(join, &incoming, expected, instance_prefix)
                }
                _ => Some(self.execute_node(node, scope, instance_prefix, depth)?),
            };
            let Some(result) = result else {
                continue;
            };

            self.budget.node_activations = self.budget.node_activations.saturating_add(1);

            let mut failure_routed = false;
            for port in &result.ports {
                emitted.insert((node_id.to_owned(), port.clone()));
                for edge in graph
                    .edges
                    .iter()
                    .filter(|edge| edge.from.node == node_id && edge.from.port == *port)
                {
                    if matches!(port, EdgePort::Failure) {
                        failure_routed = true;
                    }
                    signals
                        .entry(edge.to.node.clone())
                        .or_default()
                        .push(match port {
                            EdgePort::Failure => GraphSignal::Failed,
                            EdgePort::Success | EdgePort::Completed | EdgePort::Empty => {
                                GraphSignal::Successful
                            }
                            EdgePort::Always | EdgePort::Input if result.successful => {
                                GraphSignal::Successful
                            }
                            EdgePort::Always | EdgePort::Input => GraphSignal::Failed,
                        });
                }
            }
            if result.failed {
                if failure_routed {
                    // The failure remains visible in the step report, but an
                    // explicit failure route owns recovery. Do not also leave
                    // it in `RunReport.errors`, which is the CLI's final task
                    // failure signal.
                    self.accumulator.errors.truncate(error_checkpoint);
                } else {
                    invocation.failed = true;
                }
            }
        }

        if !graph.exits.is_empty() {
            let active_exits = graph
                .exits
                .iter()
                .filter(|exit| emitted.contains(&(exit.from.node.clone(), exit.from.port.clone())))
                .collect::<Vec<_>>();
            if active_exits.is_empty() {
                invocation.failed = true;
                self.accumulator.errors.push(format!(
                    "graph {} did not activate any declared exit",
                    graph.id.as_deref().unwrap_or("<anonymous>")
                ));
            }
            if self.opts.apply
                && active_exits
                    .iter()
                    .any(|exit| matches!(exit.from.port, EdgePort::Failure))
            {
                invocation.failed = true;
            }
        }
        Ok(invocation)
    }

    fn execute_node(
        &mut self,
        node: &GraphNode,
        scope: &mut GraphScopeState,
        instance_prefix: &str,
        depth: usize,
    ) -> Result<GraphNodeResult> {
        match node {
            GraphNode::Action(node) => self.execute_action(node, scope, instance_prefix),
            GraphNode::ForEach(node) => {
                self.execute_for_each(node, scope, instance_prefix, depth + 1)
            }
            GraphNode::If(node) => self.execute_if(node, scope, instance_prefix, depth + 1),
            GraphNode::Switch(node) => self.execute_switch(node, scope, instance_prefix, depth + 1),
            GraphNode::Join(_) => unreachable!("joins are dispatched with their input signals"),
        }
    }

    fn execute_join(
        &mut self,
        node: &JoinNode,
        incoming: &[GraphSignal],
        expected: usize,
        instance_prefix: &str,
    ) -> Option<GraphNodeResult> {
        if !self.opts.apply {
            if incoming.is_empty() {
                return None;
            }
            self.push_control_report(
                &node.id,
                "Join",
                StepStatus::Pending,
                format!(
                    "join {:?} is deferred until {} incoming path(s) have runtime outcomes",
                    node.mode,
                    incoming.len()
                ),
                instance_prefix,
            );
            return Some(GraphNodeResult::control_planned(false));
        }
        if incoming.is_empty() {
            // A join contained wholly inside an inactive route must remain
            // inactive; only a partially activated fan-in receives neutral
            // skipped tokens for its unselected paths.
            return None;
        }
        let skipped = expected.saturating_sub(incoming.len());
        let mut effective = incoming.to_vec();
        if matches!(node.mode, JoinMode::All) {
            effective.extend(std::iter::repeat_n(GraphSignal::SkippedByControl, skipped));
        }
        let result = match node.mode {
            JoinMode::All if effective.iter().all(|signal| signal.is_successful()) => {
                GraphNodeResult::control_success()
            }
            JoinMode::All => GraphNodeResult::control_failure(),
            JoinMode::Any => GraphNodeResult::control_success(),
            JoinMode::FirstSuccessful if incoming.iter().any(|signal| signal.is_successful()) => {
                GraphNodeResult::control_success()
            }
            JoinMode::FirstSuccessful => GraphNodeResult::control_failure(),
        };
        let summary = match node.mode {
            JoinMode::All => {
                format!("joined all {expected} incoming paths ({skipped} skipped by control flow)")
            }
            JoinMode::Any => format!("joined {} activated incoming path(s)", incoming.len()),
            JoinMode::FirstSuccessful => {
                if result.failed {
                    "no incoming path completed successfully".into()
                } else {
                    "joined the first successful incoming path".into()
                }
            }
        };
        self.push_control_report(
            &node.id,
            "Join",
            if result.failed {
                StepStatus::Failed
            } else if self.opts.apply {
                StepStatus::Satisfied
            } else {
                StepStatus::Pending
            },
            summary,
            instance_prefix,
        );
        Some(result)
    }

    fn execute_action(
        &mut self,
        node: &ActionNode,
        scope: &mut GraphScopeState,
        instance_prefix: &str,
    ) -> Result<GraphNodeResult> {
        if self.opts.apply {
            match evaluate_graph_step_gate(&node.step, scope) {
                GraphStepGate::Run => {}
                GraphStepGate::Skip(reason) => {
                    let plan = plan_step(&node.step, self.opts)?;
                    self.push_plan(plan.clone(), instance_prefix);
                    self.accumulator.outcomes.push(ActionOutcome::Skipped {
                        reason: reason.clone(),
                    });
                    self.push_step_report(
                        StepReport {
                            step_id: node.step.id.clone(),
                            step_name: step_name(&node.step),
                            summary: plan.summary,
                            status: StepStatus::Skipped,
                            prerequisites: plan.prerequisites,
                            logs: vec![StepLogEntry {
                                step_id: node.step.id.clone(),
                                message: format!("when condition not met: {reason}"),
                            }],
                            output: None,
                        },
                        instance_prefix,
                    );
                    insert_action_schema(&mut scope.schemas, &node.step);
                    return Ok(GraphNodeResult::action_skipped());
                }
                GraphStepGate::Fail(message) => {
                    self.push_action_failure(&node.step, &message, instance_prefix)?;
                    insert_action_schema(&mut scope.schemas, &node.step);
                    return Ok(GraphNodeResult::action_failure());
                }
            }
        }
        let mut materialized = match materialize_step(
            &node.step,
            &node.bindings,
            &scope.values,
            BindingLimits::default(),
        ) {
            Ok(step) => step,
            Err(_error) if !self.opts.apply => {
                return self.defer_graph_action(
                    node,
                    scope,
                    instance_prefix,
                    "deferred until runtime context values are available during apply",
                    "typed bindings will be materialized during apply",
                );
            }
            Err(error) => {
                let message = format!("step {} binding failed: {error}", node.step.id);
                self.push_action_failure(&node.step, &message, instance_prefix)?;
                insert_action_schema(&mut scope.schemas, &node.step);
                return Ok(GraphNodeResult::action_failure());
            }
        };
        // Apply-mode gates were already evaluated against graph scope above.
        // In planning mode retain them so the atomic runner keeps guarded
        // read-only observations pending instead of executing them early.
        if self.opts.apply {
            materialized.when = None;
            materialized.require = None;
        }

        let mut one_step_opts = self.opts.clone();
        if !matches!(materialized.action, Action::BambuStudioRelease(_)) {
            one_step_opts.release_channel = None;
        }
        let report = match run_step_sequence_with_interactivity(
            self.task,
            std::slice::from_ref(&materialized),
            &one_step_opts,
            self.terminal_interactive,
        ) {
            Ok(report) => report,
            Err(error) => {
                if error
                    .downcast_ref::<ProtectedPathApprovalRequired>()
                    .is_some()
                {
                    return Err(error).with_context(|| {
                        format!(
                            "graph action {} requires protected path approval",
                            materialized.id
                        )
                    });
                }
                let message = format!("step {} execution failed: {error:#}", materialized.id);
                self.push_action_failure(&materialized, &message, instance_prefix)?;
                return Ok(GraphNodeResult::action_failure());
            }
        };
        for entry in report.context.entries() {
            scope
                .values
                .insert(entry.scope.clone(), entry.context.clone());
        }
        insert_action_schema(&mut scope.schemas, &materialized);

        let status = report.steps.first().map(|report| &report.status);
        let result = match status {
            Some(StepStatus::Failed) => GraphNodeResult::action_failure(),
            Some(StepStatus::Skipped) => GraphNodeResult::action_skipped(),
            Some(StepStatus::Pending) => GraphNodeResult::action_planned(),
            Some(
                StepStatus::Running
                | StepStatus::WaitingForAttention
                | StepStatus::Satisfied
                | StepStatus::Applied,
            ) => GraphNodeResult::action_success(),
            None => {
                bail!(
                    "one-step graph action {} produced no report",
                    materialized.id
                )
            }
        };
        self.append_linear_report(report, instance_prefix);
        Ok(result)
    }

    fn defer_graph_action(
        &mut self,
        node: &ActionNode,
        scope: &mut GraphScopeState,
        instance_prefix: &str,
        reason: &str,
        log: &str,
    ) -> Result<GraphNodeResult> {
        let mut plan = plan_step(&node.step, self.opts)?;
        plan.summary = format!("{reason}; {}", plan.summary);
        let report = StepReport {
            step_id: node.step.id.clone(),
            step_name: step_name(&node.step),
            summary: plan.summary.clone(),
            status: StepStatus::Pending,
            prerequisites: plan.prerequisites.clone(),
            logs: vec![StepLogEntry {
                step_id: node.step.id.clone(),
                message: log.into(),
            }],
            output: None,
        };
        self.push_plan(plan.clone(), instance_prefix);
        self.accumulator.outcomes.push(ActionOutcome::Planned {
            action: self.prefixed_plan(plan, instance_prefix),
        });
        self.push_step_report(report, instance_prefix);
        insert_action_schema(&mut scope.schemas, &node.step);
        Ok(GraphNodeResult::action_planned())
    }

    fn push_action_failure(
        &mut self,
        step: &Step,
        message: &str,
        instance_prefix: &str,
    ) -> Result<()> {
        let plan = plan_step(step, self.opts)?;
        self.push_plan(plan.clone(), instance_prefix);
        self.accumulator.outcomes.push(ActionOutcome::Blocked);
        self.push_step_report(failed_step_report(step, &plan, message), instance_prefix);
        self.accumulator.errors.push(format!(
            "{}{}",
            display_instance_prefix(instance_prefix),
            message
        ));
        Ok(())
    }

    fn append_linear_report(&mut self, report: RunReport, instance_prefix: &str) {
        for plan in report.plans {
            self.push_plan(plan, instance_prefix);
        }
        let outcomes = report
            .outcomes
            .into_iter()
            .map(|outcome| self.prefixed_outcome(outcome, instance_prefix))
            .collect::<Vec<_>>();
        self.accumulator.outcomes.extend(outcomes);
        for step in report.steps {
            self.push_step_report(step, instance_prefix);
        }
        self.accumulator.errors.extend(
            report
                .errors
                .into_iter()
                .map(|error| format!("{}{}", display_instance_prefix(instance_prefix), error)),
        );
    }

    fn prefixed_plan(&self, mut plan: ActionPlan, instance_prefix: &str) -> ActionPlan {
        if !instance_prefix.is_empty() {
            plan.step_id = format!("{instance_prefix}/{}", plan.step_id);
            plan.step_name = format!("{} · {}", plan.step_name, instance_prefix);
        }
        plan
    }

    fn prefixed_outcome(&self, outcome: ActionOutcome, instance_prefix: &str) -> ActionOutcome {
        match outcome {
            ActionOutcome::Planned { action } => ActionOutcome::Planned {
                action: self.prefixed_plan(action, instance_prefix),
            },
            other => other,
        }
    }

    fn push_plan(&mut self, plan: ActionPlan, instance_prefix: &str) {
        let plan = self.prefixed_plan(plan, instance_prefix);
        self.accumulator.plans.push(plan);
    }

    fn push_step_report(&mut self, mut report: StepReport, instance_prefix: &str) {
        if !instance_prefix.is_empty() {
            report.step_id = format!("{instance_prefix}/{}", report.step_id);
            report.step_name = format!("{} · {}", report.step_name, instance_prefix);
            for log in &mut report.logs {
                log.step_id = format!("{instance_prefix}/{}", log.step_id);
            }
        }
        self.accumulator.steps.push(report);
    }

    fn push_control_report(
        &mut self,
        id: &str,
        name: &str,
        status: StepStatus,
        summary: String,
        instance_prefix: &str,
    ) {
        if matches!(status, StepStatus::Failed) {
            self.accumulator.outcomes.push(ActionOutcome::Blocked);
            self.accumulator.errors.push(format!(
                "{}control node {id} failed: {summary}",
                display_instance_prefix(instance_prefix)
            ));
        } else if self.opts.apply {
            self.accumulator.outcomes.push(ActionOutcome::Observed {
                summary: summary.clone(),
            });
        }
        self.push_step_report(
            StepReport {
                step_id: id.into(),
                step_name: name.into(),
                summary: summary.clone(),
                status,
                prerequisites: Vec::new(),
                logs: vec![StepLogEntry {
                    step_id: id.into(),
                    message: summary,
                }],
                output: None,
            },
            instance_prefix,
        );
    }
}

impl GraphRuntime<'_> {
    fn execute_if(
        &mut self,
        node: &IfNode,
        scope: &GraphScopeState,
        instance_prefix: &str,
        depth: usize,
    ) -> Result<GraphNodeResult> {
        if !self.opts.apply {
            let mut failed = false;
            let mut then_scope = scope.clone();
            let then_prefix =
                nested_instance_prefix(instance_prefix, &format!("{}[then]", node.id));
            failed |= self
                .execute_graph(&node.then_graph, &mut then_scope, &then_prefix, depth)?
                .failed;
            if let Some(else_graph) = node.else_graph.as_deref() {
                let mut else_scope = scope.clone();
                let else_prefix =
                    nested_instance_prefix(instance_prefix, &format!("{}[else]", node.id));
                failed |= self
                    .execute_graph(else_graph, &mut else_scope, &else_prefix, depth)?
                    .failed;
            }
            let summary = "planned all conditional branches; selection is deferred until apply";
            self.push_control_report(
                &node.id,
                "If",
                if failed {
                    StepStatus::Failed
                } else {
                    StepStatus::Pending
                },
                summary.into(),
                instance_prefix,
            );
            return Ok(if failed {
                GraphNodeResult::control_failure()
            } else {
                GraphNodeResult::control_planned(false)
            });
        }

        let branch = match evaluate_graph_rule(&node.condition, scope) {
            Ok(RuleEvaluation::True) => Some(("then", node.then_graph.as_ref())),
            Ok(RuleEvaluation::False) => node.else_graph.as_deref().map(|graph| ("else", graph)),
            Ok(RuleEvaluation::Null) => {
                return Ok(self.control_evaluation_failure(
                    &node.id,
                    "If",
                    "condition evaluated to null",
                    instance_prefix,
                ));
            }
            Ok(RuleEvaluation::Missing(issue)) => {
                return Ok(self.control_evaluation_failure(
                    &node.id,
                    "If",
                    &format!("condition input is missing: {}", issue.message),
                    instance_prefix,
                ));
            }
            Ok(RuleEvaluation::Unknown(issue)) => {
                return Ok(self.control_evaluation_failure(
                    &node.id,
                    "If",
                    &format!("condition input is unavailable: {}", issue.message),
                    instance_prefix,
                ));
            }
            Ok(RuleEvaluation::Error(error)) => {
                return Ok(self.control_evaluation_failure(
                    &node.id,
                    "If",
                    &format!("condition evaluation failed: {}", error.message),
                    instance_prefix,
                ));
            }
            Err(error) => {
                return Ok(self.control_evaluation_failure(
                    &node.id,
                    "If",
                    &format!("condition type-check failed: {error:#}"),
                    instance_prefix,
                ));
            }
        };

        let Some((branch_name, branch_graph)) = branch else {
            self.push_control_report(
                &node.id,
                "If",
                StepStatus::Satisfied,
                "condition was false; no else branch was declared".into(),
                instance_prefix,
            );
            return Ok(GraphNodeResult::control_success());
        };
        let mut child_scope = scope.clone();
        let child_prefix =
            nested_instance_prefix(instance_prefix, &format!("{}[{branch_name}]", node.id));
        let child = self.execute_graph(branch_graph, &mut child_scope, &child_prefix, depth)?;
        self.push_control_report(
            &node.id,
            "If",
            if child.failed {
                StepStatus::Failed
            } else {
                StepStatus::Satisfied
            },
            format!("selected {branch_name} branch"),
            instance_prefix,
        );
        Ok(if child.failed {
            GraphNodeResult::control_failure()
        } else {
            GraphNodeResult::control_success()
        })
    }

    fn execute_switch(
        &mut self,
        node: &SwitchNode,
        scope: &GraphScopeState,
        instance_prefix: &str,
        depth: usize,
    ) -> Result<GraphNodeResult> {
        if !self.opts.apply {
            let mut failed = false;
            for case in &node.cases {
                let mut child_scope = scope.clone();
                let prefix = nested_instance_prefix(
                    instance_prefix,
                    &format!("{}[case:{}]", node.id, case.id),
                );
                failed |= self
                    .execute_graph(&case.graph, &mut child_scope, &prefix, depth)?
                    .failed;
            }
            if let Some(default) = node.default.as_deref() {
                let mut child_scope = scope.clone();
                let prefix =
                    nested_instance_prefix(instance_prefix, &format!("{}[default]", node.id));
                failed |= self
                    .execute_graph(default, &mut child_scope, &prefix, depth)?
                    .failed;
            }
            self.push_control_report(
                &node.id,
                "Switch",
                if failed {
                    StepStatus::Failed
                } else {
                    StepStatus::Pending
                },
                "planned every switch case; selector is deferred until apply".into(),
                instance_prefix,
            );
            return Ok(if failed {
                GraphNodeResult::control_failure()
            } else {
                GraphNodeResult::control_planned(false)
            });
        }

        let expected = ResolvedSchemaOwned {
            value_type: ContextType::Any,
            required: true,
            nullable: true,
            sensitivity: Sensitivity::Public,
            allowed_values: Vec::new(),
        };
        let selector = match resolve_binding(
            &node.selector,
            &expected,
            &scope.values,
            BindingLimits::default(),
        ) {
            Ok(selector) => selector.value,
            Err(error) => {
                return Ok(self.control_evaluation_failure(
                    &node.id,
                    "Switch",
                    &format!("selector binding failed: {error}"),
                    instance_prefix,
                ));
            }
        };
        let selected = node
            .cases
            .iter()
            .find(|case| case.values.iter().any(|value| value == &selector));
        let (selected_name, selected_graph) = if let Some(case) = selected {
            (format!("case:{}", case.id), case.graph.as_ref())
        } else if let Some(default) = node.default.as_deref() {
            ("default".into(), default)
        } else {
            return Ok(self.control_evaluation_failure(
                &node.id,
                "Switch",
                "selector matched no case and no default was declared",
                instance_prefix,
            ));
        };
        let mut child_scope = scope.clone();
        let prefix =
            nested_instance_prefix(instance_prefix, &format!("{}[{selected_name}]", node.id));
        let child = self.execute_graph(selected_graph, &mut child_scope, &prefix, depth)?;
        self.push_control_report(
            &node.id,
            "Switch",
            if child.failed {
                StepStatus::Failed
            } else {
                StepStatus::Satisfied
            },
            format!("selected {selected_name}"),
            instance_prefix,
        );
        Ok(if child.failed {
            GraphNodeResult::control_failure()
        } else {
            GraphNodeResult::control_success()
        })
    }

    fn execute_for_each(
        &mut self,
        node: &ForEachNode,
        scope: &GraphScopeState,
        instance_prefix: &str,
        depth: usize,
    ) -> Result<GraphNodeResult> {
        let item_type =
            collection_item_type(&node.collection, &scope.schemas).unwrap_or(ContextType::Any);
        if !self.opts.apply {
            let mut symbolic = scope.clone();
            insert_loop_value(
                &mut symbolic,
                &node.id,
                0,
                serde_json::Value::Null,
                item_type,
                Sensitivity::Public,
            );
            symbolic
                .aliases
                .insert(node.item_alias.clone(), FieldRef::loop_item(&node.id));
            if let Some(alias) = &node.index_alias {
                let index_scope = loop_index_scope(&node.id);
                insert_loop_value(
                    &mut symbolic,
                    &index_scope,
                    0,
                    serde_json::Value::Null,
                    ContextType::Integer,
                    Sensitivity::Public,
                );
                symbolic
                    .aliases
                    .insert(alias.clone(), FieldRef::loop_item(index_scope));
            }
            let prefix = nested_instance_prefix(instance_prefix, &format!("{}[*]", node.id));
            let body = self.execute_graph(&node.body, &mut symbolic, &prefix, depth)?;
            self.push_control_report(
                &node.id,
                "For each",
                if body.failed {
                    StepStatus::Failed
                } else {
                    StepStatus::Pending
                },
                "planned one symbolic loop body; collection values are deferred until apply".into(),
                instance_prefix,
            );
            return Ok(if body.failed {
                GraphNodeResult::control_failure()
            } else {
                GraphNodeResult::control_planned(true)
            });
        }

        let expected = ResolvedSchemaOwned {
            value_type: ContextType::array(ContextType::Any),
            required: true,
            nullable: false,
            sensitivity: Sensitivity::Secret,
            allowed_values: Vec::new(),
        };
        let collection = match resolve_binding(
            &node.collection,
            &expected,
            &scope.values,
            BindingLimits::default(),
        ) {
            Ok(collection) => collection,
            Err(error) => {
                return Ok(self.control_evaluation_failure(
                    &node.id,
                    "For each",
                    &format!("collection binding failed: {error}"),
                    instance_prefix,
                ));
            }
        };
        let Some(items) = collection.value.as_array() else {
            return Ok(self.control_evaluation_failure(
                &node.id,
                "For each",
                "collection binding did not resolve to an array",
                instance_prefix,
            ));
        };
        self.budget.loop_iterations = self.budget.loop_iterations.saturating_add(items.len());
        if self.budget.loop_iterations > GRAPH_MAX_LOOP_ITERATIONS {
            bail!("workflow graph exceeds {GRAPH_MAX_LOOP_ITERATIONS} total loop iterations");
        }

        if items.is_empty() {
            self.push_control_report(
                &node.id,
                "For each",
                StepStatus::Satisfied,
                "collection was empty".into(),
                instance_prefix,
            );
            return Ok(GraphNodeResult {
                ports: BTreeSet::from([EdgePort::Empty, EdgePort::Completed]),
                successful: true,
                failed: false,
            });
        }

        // Two-phase execution: resolve and policy-check every iteration before
        // the first body action can mutate the machine. A body action whose
        // safety-critical binding depends on output produced by an earlier
        // body action has no value during this phase and is rejected rather
        // than allowing a later iteration to fail after partial application.
        let mut preflight_budget = self.budget.clone();
        preflight_budget.node_activations = preflight_budget.node_activations.saturating_add(1);
        if preflight_budget.node_activations > GRAPH_MAX_NODE_ACTIVATIONS {
            bail!("workflow graph preflight exceeds {GRAPH_MAX_NODE_ACTIVATIONS} node activations");
        }
        for (index, item) in items.iter().enumerate() {
            let mut child_scope = scope.clone();
            insert_loop_value(
                &mut child_scope,
                &node.id,
                index,
                item.clone(),
                item_type.clone(),
                collection.sensitivity,
            );
            child_scope
                .aliases
                .insert(node.item_alias.clone(), FieldRef::loop_item(&node.id));
            if let Some(alias) = &node.index_alias {
                let index_scope = loop_index_scope(&node.id);
                insert_loop_value(
                    &mut child_scope,
                    &index_scope,
                    index,
                    serde_json::json!(index),
                    ContextType::Integer,
                    Sensitivity::Public,
                );
                child_scope
                    .aliases
                    .insert(alias.clone(), FieldRef::loop_item(index_scope));
            }
            self.preflight_iteration_graph(
                &node.body,
                &mut child_scope,
                depth,
                &mut preflight_budget,
            )
            .with_context(|| {
                format!(
                    "for-each {} iteration {} failed preflight before any iteration ran",
                    node.id,
                    index + 1
                )
            })?;
        }

        let mut failures = 0usize;
        let mut completed = 0usize;
        for (index, item) in items.iter().enumerate() {
            let mut child_scope = scope.clone();
            insert_loop_value(
                &mut child_scope,
                &node.id,
                index,
                item.clone(),
                item_type.clone(),
                collection.sensitivity,
            );
            child_scope
                .aliases
                .insert(node.item_alias.clone(), FieldRef::loop_item(&node.id));
            if let Some(alias) = &node.index_alias {
                let index_scope = loop_index_scope(&node.id);
                insert_loop_value(
                    &mut child_scope,
                    &index_scope,
                    index,
                    serde_json::json!(index),
                    ContextType::Integer,
                    Sensitivity::Public,
                );
                child_scope
                    .aliases
                    .insert(alias.clone(), FieldRef::loop_item(index_scope));
            }
            let prefix =
                nested_instance_prefix(instance_prefix, &format!("{}[{}]", node.id, index + 1));
            let body = self.execute_graph(&node.body, &mut child_scope, &prefix, depth)?;
            completed += 1;
            if body.failed {
                failures += 1;
                if matches!(node.on_error, LoopFailurePolicy::Stop) {
                    break;
                }
            }
        }
        let failed = failures > 0;
        self.push_control_report(
            &node.id,
            "For each",
            if failed {
                StepStatus::Failed
            } else {
                StepStatus::Satisfied
            },
            format!(
                "completed {completed} of {} iteration(s); {failures} failed; concurrency {} executes deterministically in sequence",
                items.len(),
                node.concurrency
            ),
            instance_prefix,
        );
        Ok(if failed {
            GraphNodeResult::control_failure()
        } else {
            GraphNodeResult::control_success()
        })
    }

    fn preflight_iteration_graph(
        &self,
        graph: &WorkflowGraph,
        scope: &mut GraphScopeState,
        depth: usize,
        budget: &mut GraphExecutionBudget,
    ) -> Result<()> {
        if depth > GRAPH_MAX_DEPTH {
            bail!("workflow graph nesting exceeds {GRAPH_MAX_DEPTH}");
        }
        for index in deterministic_graph_order(graph)? {
            let node = &graph.nodes[index];
            budget.node_activations = budget.node_activations.saturating_add(1);
            if budget.node_activations > GRAPH_MAX_NODE_ACTIVATIONS {
                bail!(
                    "workflow graph preflight exceeds {GRAPH_MAX_NODE_ACTIVATIONS} node activations"
                );
            }
            match node {
                GraphNode::Action(node) => {
                    match evaluate_graph_step_gate(&node.step, scope) {
                        GraphStepGate::Run => {}
                        GraphStepGate::Skip(_) => {
                            insert_action_schema(&mut scope.schemas, &node.step);
                            continue;
                        }
                        GraphStepGate::Fail(message) => bail!(message),
                    }
                    let materialized = materialize_step(
                        &node.step,
                        &node.bindings,
                        &scope.values,
                        BindingLimits::default(),
                    )
                    .with_context(|| {
                        format!(
                            "materialize action {}; bindings that depend on body-produced values are unsupported in loop preflight",
                            node.step.id
                        )
                    })?;
                    enforce_step_policy(
                        &self.task.id,
                        &materialized,
                        self.opts,
                        self.terminal_interactive,
                    )?;
                    insert_action_schema(&mut scope.schemas, &materialized);
                }
                GraphNode::ForEach(node) => {
                    let expected = ResolvedSchemaOwned {
                        value_type: ContextType::array(ContextType::Any),
                        required: true,
                        nullable: false,
                        sensitivity: Sensitivity::Secret,
                        allowed_values: Vec::new(),
                    };
                    let collection = resolve_binding(
                        &node.collection,
                        &expected,
                        &scope.values,
                        BindingLimits::default(),
                    )
                    .with_context(|| {
                        format!("resolve nested for-each {} during preflight", node.id)
                    })?;
                    let items = collection.value.as_array().with_context(|| {
                        format!("nested for-each {} collection is not an array", node.id)
                    })?;
                    if items.len() > GRAPH_MAX_LOOP_ITERATIONS {
                        bail!(
                            "nested for-each {} exceeds {GRAPH_MAX_LOOP_ITERATIONS} iterations",
                            node.id
                        );
                    }
                    budget.loop_iterations = budget.loop_iterations.saturating_add(items.len());
                    if budget.loop_iterations > GRAPH_MAX_LOOP_ITERATIONS {
                        bail!(
                            "workflow graph preflight exceeds {GRAPH_MAX_LOOP_ITERATIONS} total loop iterations"
                        );
                    }
                    let item_type = collection_item_type(&node.collection, &scope.schemas)
                        .unwrap_or(ContextType::Any);
                    for (index, item) in items.iter().enumerate() {
                        let mut child_scope = scope.clone();
                        insert_loop_value(
                            &mut child_scope,
                            &node.id,
                            index,
                            item.clone(),
                            item_type.clone(),
                            collection.sensitivity,
                        );
                        child_scope
                            .aliases
                            .insert(node.item_alias.clone(), FieldRef::loop_item(&node.id));
                        if let Some(alias) = &node.index_alias {
                            let index_scope = loop_index_scope(&node.id);
                            insert_loop_value(
                                &mut child_scope,
                                &index_scope,
                                index,
                                serde_json::json!(index),
                                ContextType::Integer,
                                Sensitivity::Public,
                            );
                            child_scope
                                .aliases
                                .insert(alias.clone(), FieldRef::loop_item(index_scope));
                        }
                        self.preflight_iteration_graph(
                            &node.body,
                            &mut child_scope,
                            depth + 1,
                            budget,
                        )?;
                    }
                }
                GraphNode::If(node) => match evaluate_graph_rule(&node.condition, scope)? {
                    RuleEvaluation::True => self.preflight_iteration_graph(
                        &node.then_graph,
                        &mut scope.clone(),
                        depth + 1,
                        budget,
                    )?,
                    RuleEvaluation::False => {
                        if let Some(graph) = node.else_graph.as_deref() {
                            self.preflight_iteration_graph(
                                graph,
                                &mut scope.clone(),
                                depth + 1,
                                budget,
                            )?;
                        }
                    }
                    other => bail!(
                        "if {} is indeterminate during loop preflight: {other:?}",
                        node.id
                    ),
                },
                GraphNode::Switch(node) => {
                    let expected = ResolvedSchemaOwned {
                        value_type: ContextType::Any,
                        required: true,
                        nullable: true,
                        sensitivity: Sensitivity::Public,
                        allowed_values: Vec::new(),
                    };
                    let selector = resolve_binding(
                        &node.selector,
                        &expected,
                        &scope.values,
                        BindingLimits::default(),
                    )?;
                    let graph = node
                        .cases
                        .iter()
                        .find(|case| case.values.contains(&selector.value))
                        .map(|case| case.graph.as_ref())
                        .or(node.default.as_deref())
                        .with_context(|| {
                            format!("switch {} matched no case during loop preflight", node.id)
                        })?;
                    self.preflight_iteration_graph(graph, &mut scope.clone(), depth + 1, budget)?;
                }
                GraphNode::Join(_) => {}
            }
        }
        Ok(())
    }

    fn control_evaluation_failure(
        &mut self,
        id: &str,
        name: &str,
        message: &str,
        instance_prefix: &str,
    ) -> GraphNodeResult {
        self.push_control_report(
            id,
            name,
            StepStatus::Failed,
            message.into(),
            instance_prefix,
        );
        GraphNodeResult::control_failure()
    }
}

fn collection_item_type(binding: &Binding, schemas: &ContextStore) -> Option<ContextType> {
    let collection_type = match binding {
        Binding::Literal { value } => ContextType::infer(value),
        Binding::Field { field } => schemas.resolve_type_owned(field)?.value_type,
        Binding::Template { .. } | Binding::Interpolated { .. } => ContextType::STRING,
    };
    match collection_type {
        ContextType::Array { items } => Some(*items),
        _ => None,
    }
}

fn insert_loop_value(
    scope: &mut GraphScopeState,
    loop_id: &str,
    index: usize,
    value: serde_json::Value,
    value_type: ContextType,
    sensitivity: Sensitivity,
) {
    let context = ContextValue::new(
        value,
        ContextProvenance {
            origin: ContextOrigin::LoopItem {
                step_id: loop_id.into(),
                index,
            },
            inputs: Vec::new(),
            operation: Some("for-each-item".into()),
        },
    )
    .with_type(value_type)
    .sensitive(sensitivity);
    let context_scope = ContextScope::LoopItem {
        step_id: loop_id.into(),
    };
    scope.values.insert(context_scope.clone(), context.clone());
    scope.schemas.insert(context_scope, context);
}

fn loop_index_scope(loop_id: &str) -> String {
    format!("{loop_id}::index")
}

fn nested_instance_prefix(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.into()
    } else {
        format!("{parent}/{child}")
    }
}

#[derive(Debug)]
enum GraphStepGate {
    Run,
    Skip(String),
    Fail(String),
}

fn evaluate_graph_step_gate(step: &Step, scope: &GraphScopeState) -> GraphStepGate {
    if let Some(condition) = &step.when {
        match evaluate_graph_condition(condition, scope) {
            Ok(ConditionEvaluation::Matched(_)) => {}
            Ok(ConditionEvaluation::NotMatched(reason))
            | Ok(ConditionEvaluation::Unavailable(reason)) => {
                return GraphStepGate::Skip(reason);
            }
            Err(error) => {
                return GraphStepGate::Fail(format!(
                    "step {} when condition failed to evaluate: {error:#}",
                    step.id
                ));
            }
        }
    }
    if let Some(condition) = &step.require {
        match evaluate_graph_condition(condition, scope) {
            Ok(ConditionEvaluation::Matched(_)) => {}
            Ok(ConditionEvaluation::NotMatched(reason))
            | Ok(ConditionEvaluation::Unavailable(reason)) => {
                return GraphStepGate::Fail(format!(
                    "step {} required condition was not met: {reason}",
                    step.id
                ));
            }
            Err(error) => {
                return GraphStepGate::Fail(format!(
                    "step {} required condition failed to evaluate: {error:#}",
                    step.id
                ));
            }
        }
    }
    GraphStepGate::Run
}

fn evaluate_graph_condition(
    condition: &StepCondition,
    scope: &GraphScopeState,
) -> Result<ConditionEvaluation> {
    match condition {
        StepCondition::ExitCode { step, codes } => {
            let reference = FieldRef::step(step).field("exit_code");
            let exit_code = match scope.values.resolve(&reference) {
                Ok(value) => value
                    .value
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok()),
                Err(_) => None,
            };
            let Some(exit_code) = exit_code else {
                return Ok(ConditionEvaluation::Unavailable(format!(
                    "step {step} produced no normal exit code"
                )));
            };
            if codes.contains(&exit_code) {
                Ok(ConditionEvaluation::Matched(format!(
                    "step {step} returned accepted branch code {exit_code}"
                )))
            } else {
                Ok(ConditionEvaluation::NotMatched(format!(
                    "step {step} returned exit code {exit_code}; expected one of [{}]",
                    format_exit_codes(codes)
                )))
            }
        }
        StepCondition::Path { path, expect } => {
            let action = InspectPathAction {
                path: path.clone(),
                recursive_size: expect.min_size_bytes.is_some() || expect.max_size_bytes.is_some(),
                sha256: expect.sha256.is_some(),
                expect: Some(expect.clone()),
            };
            let metadata = inspect_path(&action)?;
            match verify_path_expectation(expect, &metadata) {
                Ok(()) => Ok(ConditionEvaluation::Matched(summarize_path_metadata(
                    &metadata,
                ))),
                Err(error) => Ok(ConditionEvaluation::NotMatched(error.to_string())),
            }
        }
        StepCondition::All { conditions } => {
            let mut matched = Vec::new();
            let mut unavailable = Vec::new();
            for child in conditions {
                match evaluate_graph_condition(child, scope)? {
                    ConditionEvaluation::Matched(reason) => matched.push(reason),
                    ConditionEvaluation::NotMatched(reason) => {
                        return Ok(ConditionEvaluation::NotMatched(format!(
                            "all condition failed: {reason}"
                        )));
                    }
                    ConditionEvaluation::Unavailable(reason) => unavailable.push(reason),
                }
            }
            if unavailable.is_empty() {
                Ok(ConditionEvaluation::Matched(format!(
                    "all conditions matched: {}",
                    matched.join("; ")
                )))
            } else {
                Ok(ConditionEvaluation::Unavailable(format!(
                    "all condition is unavailable: {}",
                    unavailable.join("; ")
                )))
            }
        }
        StepCondition::Any { conditions } => {
            let mut unmatched = Vec::new();
            let mut unavailable = Vec::new();
            for child in conditions {
                match evaluate_graph_condition(child, scope)? {
                    ConditionEvaluation::Matched(reason) => {
                        return Ok(ConditionEvaluation::Matched(format!(
                            "any condition matched: {reason}"
                        )));
                    }
                    ConditionEvaluation::NotMatched(reason) => unmatched.push(reason),
                    ConditionEvaluation::Unavailable(reason) => unavailable.push(reason),
                }
            }
            if unavailable.is_empty() {
                Ok(ConditionEvaluation::NotMatched(format!(
                    "no any branch matched: {}",
                    unmatched.join("; ")
                )))
            } else {
                Ok(ConditionEvaluation::Unavailable(format!(
                    "any condition is unavailable: {}",
                    unavailable.join("; ")
                )))
            }
        }
        StepCondition::Not { condition } => match evaluate_graph_condition(condition, scope)? {
            ConditionEvaluation::Matched(reason) => Ok(ConditionEvaluation::NotMatched(format!(
                "negated condition matched: {reason}"
            ))),
            ConditionEvaluation::NotMatched(reason) => Ok(ConditionEvaluation::Matched(format!(
                "negated condition did not match: {reason}"
            ))),
            ConditionEvaluation::Unavailable(reason) => Ok(ConditionEvaluation::Unavailable(
                format!("negated condition is unavailable: {reason}"),
            )),
        },
        StepCondition::Expression { rule, policy } => match evaluate_graph_rule(rule, scope)? {
            RuleEvaluation::True => Ok(ConditionEvaluation::Matched(
                "context rule evaluated to true".into(),
            )),
            RuleEvaluation::False => Ok(ConditionEvaluation::NotMatched(
                "context rule evaluated to false".into(),
            )),
            RuleEvaluation::Null => {
                apply_indeterminate_policy(policy.on_null, "context rule evaluated to null")
            }
            RuleEvaluation::Missing(issue) => apply_indeterminate_policy(
                policy.on_missing,
                &format!("context rule input is missing: {}", issue.message),
            ),
            RuleEvaluation::Unknown(issue) => apply_indeterminate_policy(
                policy.on_unknown,
                &format!("context rule input is unavailable: {}", issue.message),
            ),
            RuleEvaluation::Error(error) => bail!(
                "context rule evaluation failed ({:?}): {}",
                error.kind,
                error.message
            ),
        },
    }
}

fn evaluate_graph_rule(rule: &RuleExprV1, scope: &GraphScopeState) -> Result<RuleEvaluation> {
    let mut rewritten = rule.clone();
    rewrite_graph_locals(
        &mut rewritten,
        &scope.aliases,
        &mut BTreeSet::<String>::new(),
    );
    let checked = check_rule(rewritten, &scope.schemas, ExpressionLimits::default()).map_err(
        |diagnostics| {
            anyhow!(
                "context rule failed type-check: {}",
                diagnostics
                    .into_iter()
                    .map(|diagnostic| format!(
                        "{:?} at {}: {}",
                        diagnostic.code, diagnostic.location, diagnostic.message
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        },
    )?;
    Ok(checked.evaluate_rule(&scope.values))
}

fn rewrite_graph_locals(
    expression: &mut ExpressionV1,
    aliases: &BTreeMap<String, FieldRef>,
    quantifier_bindings: &mut BTreeSet<String>,
) {
    match expression {
        ExpressionV1::Literal { .. } => {}
        ExpressionV1::Ref { reference } | ExpressionV1::Exists { reference } => {
            rewrite_graph_reference(reference, aliases, quantifier_bindings);
        }
        ExpressionV1::All { expressions } | ExpressionV1::Any { expressions } => {
            for expression in expressions {
                rewrite_graph_locals(expression, aliases, quantifier_bindings);
            }
        }
        ExpressionV1::Not { expression }
        | ExpressionV1::IsNull { expression }
        | ExpressionV1::IsEmpty { expression } => {
            rewrite_graph_locals(expression, aliases, quantifier_bindings);
        }
        ExpressionV1::Compare { left, right, .. } => {
            rewrite_graph_locals(left, aliases, quantifier_bindings);
            rewrite_graph_locals(right, aliases, quantifier_bindings);
        }
        ExpressionV1::Contains { value, needle } => {
            rewrite_graph_locals(value, aliases, quantifier_bindings);
            rewrite_graph_locals(needle, aliases, quantifier_bindings);
        }
        ExpressionV1::StartsWith { value, prefix } => {
            rewrite_graph_locals(value, aliases, quantifier_bindings);
            rewrite_graph_locals(prefix, aliases, quantifier_bindings);
        }
        ExpressionV1::EndsWith { value, suffix } => {
            rewrite_graph_locals(value, aliases, quantifier_bindings);
            rewrite_graph_locals(suffix, aliases, quantifier_bindings);
        }
        ExpressionV1::Matches { value, .. } => {
            rewrite_graph_locals(value, aliases, quantifier_bindings);
        }
        ExpressionV1::In { needle, collection } => {
            rewrite_graph_locals(needle, aliases, quantifier_bindings);
            rewrite_graph_locals(collection, aliases, quantifier_bindings);
        }
        ExpressionV1::Quantifier {
            collection,
            binding,
            predicate,
            ..
        } => {
            rewrite_graph_locals(collection, aliases, quantifier_bindings);
            let was_present = !quantifier_bindings.insert(binding.clone());
            rewrite_graph_locals(predicate, aliases, quantifier_bindings);
            if !was_present {
                quantifier_bindings.remove(binding);
            }
        }
    }
}

fn rewrite_graph_reference(
    reference: &mut ReferenceV1,
    aliases: &BTreeMap<String, FieldRef>,
    quantifier_bindings: &BTreeSet<String>,
) {
    let ReferenceV1::Local { binding, path } = reference else {
        return;
    };
    if quantifier_bindings.contains(binding) {
        return;
    }
    let Some(mut field) = aliases.get(binding).cloned() else {
        return;
    };
    field
        .segments
        .extend(path.iter().cloned().map(ContextPathSegment::field));
    *reference = ReferenceV1::Context { field };
}

fn deterministic_graph_order(graph: &WorkflowGraph) -> Result<Vec<usize>> {
    let mut processed = BTreeSet::<String>::new();
    let mut order = Vec::with_capacity(graph.nodes.len());
    while order.len() < graph.nodes.len() {
        let next = graph.nodes.iter().enumerate().find(|(_, node)| {
            !processed.contains(node.id())
                && graph
                    .edges
                    .iter()
                    .filter(|edge| edge.to.node == node.id())
                    .all(|edge| processed.contains(&edge.from.node))
        });
        let Some((index, node)) = next else {
            bail!("workflow graph is cyclic or has an unresolved predecessor");
        };
        processed.insert(node.id().to_owned());
        order.push(index);
    }
    Ok(order)
}

fn insert_action_schema(store: &mut ContextStore, step: &Step) {
    let definition = definition_for_action(&step.action);
    store.insert(
        ContextScope::Step {
            step_id: step.id.clone(),
        },
        ContextValue::new(serde_json::Value::Null, ContextProvenance::step(&step.id))
            .with_schema(definition.output_schema),
    );
}

fn display_instance_prefix(prefix: &str) -> String {
    if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}: ")
    }
}

fn enforce_step_policy(
    task_id: &str,
    step: &Step,
    opts: &RunOptions,
    terminal_interactive: bool,
) -> Result<()> {
    if matches!(step.allow_elevation, ElevationPolicy::Allow) && !opts.allow_elevation {
        return Err(AutomationError::Message(format!(
            "step {} requires --allow-elevation",
            step.id
        ))
        .into());
    }
    let requires_shell_permission = matches!(
        &step.action,
        Action::RunCommand {
            shell: ShellMode::Allow,
            ..
        } | Action::RunScript { .. }
    );
    if requires_shell_permission && !opts.allow_shell {
        return Err(
            AutomationError::Message(format!("step {} requires --allow-shell", step.id)).into(),
        );
    }
    if opts.apply && matches!(&step.action, Action::ActivateLicense(_)) && !terminal_interactive {
        return Err(AutomationError::Message(format!(
            "step {} requires an interactive terminal for vendor UI license activation",
            step.id
        ))
        .into());
    }
    validate_destinations(task_id, step, opts)?;
    if opts.apply {
        validate_existing_dmg_install(step)?;
    }
    Ok(())
}

fn failed_step_report(step: &Step, plan: &ActionPlan, message: &str) -> StepReport {
    StepReport {
        step_id: step.id.clone(),
        step_name: step_name(step),
        summary: plan.summary.clone(),
        status: StepStatus::Failed,
        prerequisites: plan.prerequisites.clone(),
        logs: vec![StepLogEntry {
            step_id: step.id.clone(),
            message: format!("failed: {message}"),
        }],
        output: None,
    }
}

fn resolve_for_each_items(
    source_step: &str,
    array_path: &str,
    fields: &[String],
    reports: &[StepReport],
) -> Result<Vec<serde_json::Value>> {
    let report = reports
        .iter()
        .find(|report| report.step_id == source_step)
        .with_context(|| format!("source step {source_step} has not produced a report"))?;
    let output = report
        .output
        .as_ref()
        .with_context(|| format!("source step {source_step} has no output context"))?;
    let serialized = output.context_value()?;
    let mut current = &serialized;
    for segment in array_path.split('.').filter(|segment| !segment.is_empty()) {
        current = current
            .get(segment)
            .with_context(|| format!("context path {array_path} is missing segment {segment}"))?;
    }
    let items = current
        .as_array()
        .cloned()
        .with_context(|| format!("context path {array_path} is not an array"))?;
    if fields.is_empty() {
        return Ok(items);
    }
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let object = item.as_object().with_context(|| {
                format!(
                    "context item {} at {array_path} is not an object",
                    index + 1
                )
            })?;
            let mut projected = serde_json::Map::new();
            for field in fields {
                let value = object.get(field).with_context(|| {
                    format!("context item {} has no selected field {field}", index + 1)
                })?;
                projected.insert(field.clone(), value.clone());
            }
            Ok(serde_json::Value::Object(projected))
        })
        .collect()
}

fn item_value_at_path<'a>(
    expression: &str,
    item_alias: &str,
    item: &'a serde_json::Value,
) -> Result<&'a serde_json::Value> {
    let mut segments = expression.split('.');
    let alias = segments.next().unwrap_or_default();
    if alias != item_alias {
        bail!("template {expression} must start with loop item {item_alias}");
    }
    let mut current = item;
    for segment in segments {
        current = current
            .get(segment)
            .with_context(|| format!("template field {expression} does not exist"))?;
    }
    Ok(current)
}

fn scalar_template_value(value: &serde_json::Value, expression: &str) -> Result<String> {
    match value {
        serde_json::Value::String(value) => Ok(value.clone()),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        serde_json::Value::Null => bail!("template field {expression} is null"),
        _ => bail!("template field {expression} is not a scalar value"),
    }
}

fn render_optional_item_template(
    template: &str,
    item_alias: &str,
    item: &serde_json::Value,
) -> Result<Option<String>> {
    let trimmed = template.trim();
    if let Some(expression) = trimmed
        .strip_prefix("{{")
        .and_then(|value| value.strip_suffix("}}"))
    {
        let expression = expression.trim();
        let value = item_value_at_path(expression, item_alias, item)?;
        if value.is_null() {
            return Ok(None);
        }
    }
    render_item_template(template, item_alias, item).map(Some)
}

fn render_item_template(
    template: &str,
    item_alias: &str,
    item: &serde_json::Value,
) -> Result<String> {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        rendered.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let end = after_start
            .find("}}")
            .with_context(|| format!("unclosed template in {template}"))?;
        let expression = after_start[..end].trim();
        let value = item_value_at_path(expression, item_alias, item)?;
        rendered.push_str(&scalar_template_value(value, expression)?);
        rest = &after_start[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

fn validate_existing_dmg_install(step: &Step) -> Result<()> {
    let Action::InstallDmg {
        app_name: Some(app_name),
        target,
        identity: Some(identity),
        ..
    } = &step.action
    else {
        return Ok(());
    };
    let destination = dmg_install_destination(app_name, target.as_deref())?;
    if path_entry_exists(&destination)? {
        verify_app_identity(&destination, identity).with_context(|| {
            format!(
                "existing application does not match the pinned identity and version: {}",
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn validate_destinations(task_id: &str, step: &Step, opts: &RunOptions) -> Result<()> {
    match &step.action {
        Action::CreateDirectory(action) => {
            validate_create_directory_path(&action.path).with_context(|| {
                format!(
                    "step {} directory destination {} blocked by safety",
                    step.id, action.path
                )
            })?;
        }
        Action::InspectPath(action) => {
            validate_declared_path(&action.path).with_context(|| {
                format!("step {} inspect path {} is invalid", step.id, action.path)
            })?;
        }
        Action::CopyPath(action) => {
            validate_declared_path(&action.src).with_context(|| {
                format!("step {} copy source {} is invalid", step.id, action.src)
            })?;
            validate_safe_mutation_path(&action.dest, "copy-path").with_context(|| {
                format!(
                    "step {} copy destination {} blocked by safety",
                    step.id, action.dest
                )
            })?;
        }
        Action::WriteFile(action) => {
            validate_safe_mutation_path(&action.path, "write-file").with_context(|| {
                format!(
                    "step {} write destination {} blocked by safety",
                    step.id, action.path
                )
            })?;
        }
        Action::RemovePath(action) => {
            validate_safe_mutation_path(&action.path, "remove-path").with_context(|| {
                format!(
                    "step {} removal target {} blocked by safety",
                    step.id, action.path
                )
            })?;
        }
        Action::GitInspect { dest, .. } => {
            validate_git_inspection_path(dest).with_context(|| {
                format!("step {} git inspection path {} is invalid", step.id, dest)
            })?;
        }
        Action::GitClone {
            repo, dest, branch, ..
        } => {
            let Some(path) = expand_path_template(dest) else {
                bail!("step {} has unexpanded destination {}", step.id, dest);
            };
            validate_git_destination_with_approval(
                &path,
                GitDestinationApprovalContext {
                    task_id,
                    step_id: &step.id,
                    operation: ProtectedPathOperation::GitCloneOrUpdate,
                    repository: repo,
                    branch: branch.as_deref(),
                    approvals: &opts.protected_path_approvals,
                },
            )
            .with_context(|| {
                format!(
                    "step {} git destination {} blocked by safety",
                    step.id,
                    path.display()
                )
            })?;
        }
        Action::GitCloneIfMissing {
            repo, dest, branch, ..
        } => {
            let Some(path) = expand_path_template(dest) else {
                bail!("step {} has unexpanded destination {}", step.id, dest);
            };
            validate_git_destination_with_approval(
                &path,
                GitDestinationApprovalContext {
                    task_id,
                    step_id: &step.id,
                    operation: ProtectedPathOperation::GitCloneIfMissing,
                    repository: repo,
                    branch: branch.as_deref(),
                    approvals: &opts.protected_path_approvals,
                },
            )
            .with_context(|| {
                format!(
                    "step {} git destination {} blocked by safety",
                    step.id,
                    path.display()
                )
            })?;
        }
        Action::GitFetch {
            repo, dest, branch, ..
        } => {
            let Some(path) = expand_path_template(dest) else {
                bail!("step {} has unexpanded destination {}", step.id, dest);
            };
            validate_git_destination_with_approval(
                &path,
                GitDestinationApprovalContext {
                    task_id,
                    step_id: &step.id,
                    operation: ProtectedPathOperation::GitFetch,
                    repository: repo,
                    branch: Some(branch),
                    approvals: &opts.protected_path_approvals,
                },
            )
            .with_context(|| {
                format!(
                    "step {} git destination {} blocked by safety",
                    step.id,
                    path.display()
                )
            })?;
        }
        Action::GitFastForward {
            repo, dest, branch, ..
        } => {
            let Some(path) = expand_path_template(dest) else {
                bail!("step {} has unexpanded destination {}", step.id, dest);
            };
            validate_git_destination_with_approval(
                &path,
                GitDestinationApprovalContext {
                    task_id,
                    step_id: &step.id,
                    operation: ProtectedPathOperation::GitFastForward,
                    repository: repo,
                    branch: Some(branch),
                    approvals: &opts.protected_path_approvals,
                },
            )
            .with_context(|| {
                format!(
                    "step {} git destination {} blocked by safety",
                    step.id,
                    path.display()
                )
            })?;
        }
        Action::DownloadFile { dest, .. } | Action::ExtractArchive { dest, .. } => {
            let Some(path) = expand_path_template(dest) else {
                bail!("step {} has unexpanded destination {}", step.id, dest);
            };
            if !is_safe_rule_root(parent_or_self(&path)) {
                bail!(
                    "step {} destination {} blocked by safety",
                    step.id,
                    path.display()
                );
            }
        }
        Action::InstallDmg { target, .. } => validate_dmg_target(step, target.as_deref())?,
        _ => {}
    }
    for condition in [step.when.as_ref(), step.require.as_ref()]
        .into_iter()
        .flatten()
    {
        validate_condition_paths(condition)
            .with_context(|| format!("step {} condition contains an invalid path", step.id))?;
    }
    Ok(())
}

fn validate_condition_paths(condition: &StepCondition) -> Result<()> {
    match condition {
        StepCondition::Path { path, .. } => {
            validate_declared_path(path)?;
        }
        StepCondition::All { conditions } | StepCondition::Any { conditions } => {
            for child in conditions {
                validate_condition_paths(child)?;
            }
        }
        StepCondition::Not { condition } => validate_condition_paths(condition)?,
        StepCondition::ExitCode { .. } | StepCondition::Expression { .. } => {}
    }
    Ok(())
}

fn validate_declared_path(raw: &str) -> Result<PathBuf> {
    let path = expand_required_path(raw)?;
    if !path.is_absolute() {
        bail!(
            "path must be absolute after template expansion: {}",
            path.display()
        );
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("path must not contain '..': {}", path.display());
    }
    lexical_absolute_path(&path)
}

fn validate_git_inspection_path(raw: &str) -> Result<PathBuf> {
    let path = validate_declared_path(raw)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "git inspection path must not be a symlink: {}",
                path.display()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect git inspection path {}", path.display()));
        }
    }
    Ok(path)
}

fn validate_create_directory_path(raw: &str) -> Result<PathBuf> {
    let path = validate_declared_path(raw)?;
    let resolved = resolve_through_existing_ancestor(&path)?;
    if !is_safe_rule_root(&resolved) {
        bail!("resolved directory {} is not safe", resolved.display());
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "directory destination must not be a symlink: {}",
                path.display()
            )
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!(
                "directory destination is not a directory: {}",
                path.display()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect directory destination {}", path.display()));
        }
    }
    Ok(path)
}

/// Resolve the longest existing prefix before evaluating the safety policy.
/// This catches destinations redirected through an existing symlink without
/// rejecting normal platform aliases such as macOS `/var` -> `/private/var`.
fn resolve_through_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut deepest_existing = path.to_path_buf();
    let metadata = loop {
        match fs::symlink_metadata(&deepest_existing) {
            Ok(metadata) => break metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !deepest_existing.pop() {
                    return Err(error)
                        .with_context(|| format!("resolve destination {}", path.display()));
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect destination ancestor {}",
                        deepest_existing.display()
                    )
                });
            }
        }
    };
    let suffix = path
        .strip_prefix(&deepest_existing)
        .with_context(|| format!("resolve destination suffix for {}", path.display()))?;
    if !suffix.as_os_str().is_empty() && !metadata.is_dir() && !metadata.file_type().is_symlink() {
        bail!(
            "destination ancestor is not a directory: {}",
            deepest_existing.display()
        );
    }
    let canonical_base = deepest_existing.canonicalize().with_context(|| {
        format!(
            "canonicalize destination ancestor {}",
            deepest_existing.display()
        )
    })?;
    if !suffix.as_os_str().is_empty() && !canonical_base.is_dir() {
        bail!(
            "resolved destination ancestor is not a directory: {}",
            canonical_base.display()
        );
    }
    Ok(canonical_base.join(suffix))
}

#[derive(Debug)]
struct ValidatedGitDestination {
    resolved_path: PathBuf,
    protected: bool,
}

#[derive(Clone, Copy)]
struct GitDestinationApprovalContext<'a> {
    task_id: &'a str,
    step_id: &'a str,
    operation: ProtectedPathOperation,
    repository: &'a str,
    branch: Option<&'a str>,
    approvals: &'a [ProtectedPathApproval],
}

fn capture_git_destination_snapshot(path: &Path) -> Result<ProtectedPathSnapshot> {
    if !path.is_absolute() {
        bail!(
            "git destination must be absolute after template expansion: {}",
            path.display()
        );
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("git destination must not contain '..': {}", path.display());
    }
    let absolute = lexical_absolute_path(path)?;
    let mut deepest_existing = absolute.clone();
    let metadata = loop {
        match fs::symlink_metadata(&deepest_existing) {
            Ok(metadata) => break metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !deepest_existing.pop() {
                    return Err(error)
                        .with_context(|| format!("resolve git destination {}", path.display()));
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect git destination ancestor {}",
                        deepest_existing.display()
                    )
                });
            }
        }
    };
    let suffix = absolute
        .strip_prefix(&deepest_existing)
        .with_context(|| format!("resolve git destination suffix for {}", absolute.display()))?;
    if !suffix.as_os_str().is_empty() && !metadata.is_dir() && !metadata.file_type().is_symlink() {
        bail!(
            "git destination ancestor is not a directory: {}",
            deepest_existing.display()
        );
    }
    let canonical_base = deepest_existing.canonicalize().with_context(|| {
        format!(
            "canonicalize git destination ancestor {}",
            deepest_existing.display()
        )
    })?;
    if !suffix.as_os_str().is_empty() && !canonical_base.is_dir() {
        bail!(
            "resolved git destination ancestor is not a directory: {}",
            canonical_base.display()
        );
    }
    let resolved = canonical_base.join(suffix);
    let anchor_identity = same_file::Handle::from_path(&canonical_base).with_context(|| {
        format!(
            "open git destination ancestor identity {}",
            canonical_base.display()
        )
    })?;
    Ok(ProtectedPathSnapshot {
        requested_path: absolute,
        resolved_path: resolved,
        anchor_path: canonical_base,
        anchor_identity: Arc::new(anchor_identity),
    })
}

fn revalidate_protected_path_snapshot(snapshot: &ProtectedPathSnapshot) -> Result<()> {
    reject_documents_symlink_components(&snapshot.requested_path)?;
    revalidate_destination_snapshot_identity(snapshot)
}

fn revalidate_destination_snapshot_identity(snapshot: &ProtectedPathSnapshot) -> Result<()> {
    let current = capture_git_destination_snapshot(&snapshot.requested_path)?;
    if current.resolved_path != snapshot.resolved_path
        || current.anchor_path != snapshot.anchor_path
        || current.anchor_identity.as_ref() != snapshot.anchor_identity.as_ref()
    {
        bail!(
            "protected destination identity changed: {}",
            snapshot.requested_path.display()
        );
    }
    Ok(())
}

fn canonical_documents_root() -> Result<PathBuf> {
    let documents = dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory unavailable"))?
        .join("Documents");
    documents
        .canonicalize()
        .with_context(|| format!("canonicalize Documents root {}", documents.display()))
}

fn is_approvable_documents_destination(path: &Path) -> Result<bool> {
    let documents = canonical_documents_root()?;
    Ok(path != documents && path.starts_with(documents))
}

fn lexical_documents_root() -> Result<PathBuf> {
    let documents = dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory unavailable"))?
        .join("Documents");
    lexical_absolute_path(&documents)
}

fn reject_documents_symlink_components(path: &Path) -> Result<()> {
    let documents = lexical_documents_root()?;
    reject_symlink_components_below(&documents, path)
}

fn reject_symlink_components_below(root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "protected destination {} is not below {}",
            path.display(),
            root.display()
        )
    })?;
    if relative.as_os_str().is_empty() {
        bail!("the Documents root itself cannot be approved");
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "protected git destination must not contain symlink components: {}",
                    current.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect protected destination component {}",
                        current.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn protected_destination_parent_is_real(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        anyhow!(
            "protected git destination has no parent: {}",
            path.display()
        )
    })?;
    let metadata = fs::symlink_metadata(parent).with_context(|| {
        format!(
            "protected git destination requires its immediate parent to already exist: {}",
            parent.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "protected git destination requires a real immediate parent directory: {}",
            parent.display()
        );
    }
    Ok(())
}

fn approval_matches_destination(
    approval: &ProtectedPathApproval,
    context: GitDestinationApprovalContext<'_>,
    snapshot: &ProtectedPathSnapshot,
) -> bool {
    approval_key_matches_destination(approval, context, snapshot)
        && revalidate_protected_path_snapshot(&approval.snapshot).is_ok()
}

fn approval_key_matches_destination(
    approval: &ProtectedPathApproval,
    context: GitDestinationApprovalContext<'_>,
    snapshot: &ProtectedPathSnapshot,
) -> bool {
    approval.task_id == context.task_id
        && approval.step_id == context.step_id
        && approval.operation == context.operation
        && approval.repository == context.repository
        && approval.branch.as_deref() == context.branch
        && approval.snapshot.requested_path == snapshot.requested_path
        && approval.snapshot.resolved_path == snapshot.resolved_path
}

fn approval_anchor_identity_is_current(approval: &ProtectedPathApproval) -> bool {
    let Ok(canonical_anchor) = approval.snapshot.anchor_path.canonicalize() else {
        return false;
    };
    if canonical_anchor != approval.snapshot.anchor_path {
        return false;
    }
    let Ok(current_identity) = same_file::Handle::from_path(&approval.snapshot.anchor_path) else {
        return false;
    };
    &current_identity == approval.snapshot.anchor_identity.as_ref()
}

fn revalidate_git_destination_after_action(
    path: &Path,
    context: GitDestinationApprovalContext<'_>,
) -> Result<()> {
    let snapshot = capture_git_destination_snapshot(path)?;
    if is_safe_rule_root(parent_or_self(&snapshot.resolved_path)) {
        return Ok(());
    }
    if !is_approvable_documents_destination(&snapshot.resolved_path)? {
        bail!(
            "git destination escaped its approved protected path during execution: {}",
            snapshot.resolved_path.display()
        );
    }
    reject_documents_symlink_components(&snapshot.requested_path)?;
    protected_destination_parent_is_real(&snapshot.requested_path)?;
    if context.approvals.iter().any(|approval| {
        approval_key_matches_destination(approval, context, &snapshot)
            && approval_anchor_identity_is_current(approval)
    }) {
        return Ok(());
    }
    bail!(
        "git destination changed during protected operation: {}",
        snapshot.requested_path.display()
    )
}

fn validate_git_destination_with_approval(
    path: &Path,
    context: GitDestinationApprovalContext<'_>,
) -> Result<ValidatedGitDestination> {
    let snapshot = capture_git_destination_snapshot(path)?;
    if is_safe_rule_root(parent_or_self(&snapshot.resolved_path)) {
        return Ok(ValidatedGitDestination {
            resolved_path: snapshot.resolved_path,
            protected: false,
        });
    }
    if !is_approvable_documents_destination(&snapshot.resolved_path)? {
        bail!(
            "resolved destination {} is not safe",
            snapshot.resolved_path.display()
        );
    }
    reject_documents_symlink_components(&snapshot.requested_path)?;
    protected_destination_parent_is_real(&snapshot.requested_path)?;
    if context
        .approvals
        .iter()
        .any(|approval| approval_matches_destination(approval, context, &snapshot))
    {
        return Ok(ValidatedGitDestination {
            resolved_path: snapshot.resolved_path,
            protected: true,
        });
    }
    Err(ProtectedPathApprovalRequired {
        request: ProtectedPathApprovalRequest {
            task_id: context.task_id.to_owned(),
            step_id: context.step_id.to_owned(),
            operation: context.operation,
            repository: context.repository.to_owned(),
            branch: context.branch.map(str::to_owned),
            risk: ProtectedPathRisk::UserDocuments,
            snapshot,
        },
    }
    .into())
}

#[cfg(test)]
fn validate_resolved_git_destination(path: &Path) -> Result<PathBuf> {
    let snapshot = capture_git_destination_snapshot(path)?;
    let resolved = snapshot.resolved_path;
    if !is_safe_rule_root(parent_or_self(&resolved)) {
        bail!("resolved destination {} is not safe", resolved.display());
    }
    Ok(resolved)
}

fn lexical_absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory for git destination")?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn validate_dmg_target(step: &Step, target: Option<&str>) -> Result<()> {
    let raw_target = target.unwrap_or("$HOME/Applications");
    let target_path = expand_required_path(raw_target)?;
    let home_apps = dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory unavailable"))?
        .join("Applications");

    if target_path != home_apps {
        bail!(
            "step {} dmg target {} is not allowed; install-dmg is restricted to $HOME/Applications",
            step.id,
            target_path.display()
        );
    }
    if !matches!(step.auth, AuthPolicy::None)
        || !matches!(step.allow_elevation, ElevationPolicy::Forbidden)
    {
        bail!(
            "step {} install-dmg must not request authentication or elevation",
            step.id
        );
    }
    Ok(())
}

fn dmg_install_destination(app_name: &str, target: Option<&str>) -> Result<PathBuf> {
    Ok(expand_required_path(target.unwrap_or("$HOME/Applications"))?.join(app_name))
}

fn parent_or_self(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

fn plan_step(step: &Step, opts: &RunOptions) -> Result<ActionPlan> {
    let prerequisites = prerequisites_for_step(step);
    let summary = describe_step(step, opts)?;
    Ok(ActionPlan {
        step_id: step.id.clone(),
        step_name: step_name(step),
        summary,
        prerequisites,
    })
}

/// Return a detailed, side-effect-free explanation of what one step will do.
///
/// This is shared by plans, reports, and the scenario inspector so the user
/// sees the same technical description before and during execution.
pub fn describe_step(step: &Step, opts: &RunOptions) -> Result<String> {
    let summary = match &step.action {
        Action::GithubListRepositories => {
            "list repositories visible to the GitHub CLI account and return the account login plus typed GitHub repository metadata".into()
        }
        Action::GithubSelectRepositories {
            expected_account_login,
            repository_ids,
            ..
        } => format!(
            "select {} repository/repositories by exact GitHub node ID from the freshly listed account {:?}; fail if the account or any selected ID changed",
            repository_ids.len(),
            expected_account_login
        ),
        Action::ForEach {
            source_step,
            array_path,
            item,
            fields,
        } => format!(
            "iterate over array {array_path} from step {source_step}, exposing each element as {item}{}",
            if fields.is_empty() {
                String::new()
            } else {
                format!(" with selected fields {}", fields.join(", "))
            }
        ),
        Action::ForEachGitCloneIfMissing {
            loop_step,
            repo,
            dest,
            branch,
        } => format!(
            "for every item from loop {loop_step}, clone {repo} to {dest} when absent{}",
            branch
                .as_deref()
                .map(|branch| format!(" using branch {branch}"))
                .unwrap_or_default()
        ),
        Action::CreateDirectory(action) => format!(
            "create directory {} and missing parents; leave an existing real directory unchanged and never replace a file or symlink",
            action.path
        ),
        Action::InspectPath(action) => {
            let measurement = if action.recursive_size {
                ", recursively total regular-file bytes without following symlinks"
            } else {
                ""
            };
            let checksum = if action.sha256
                || action
                    .expect
                    .as_ref()
                    .is_some_and(|expectation| expectation.sha256.is_some())
            {
                ", compute SHA-256 for a regular file"
            } else {
                ""
            };
            let expectation = action
                .expect
                .as_ref()
                .map(describe_path_expectation)
                .unwrap_or_default();
            format!(
                "inspect {} for existence, type, emptiness, and timestamps{}{}{}",
                action.path, measurement, checksum, expectation
            )
        }
        Action::CopyPath(action) => format!(
            "copy {} to {} without following symlinks; leave an identical file or tree unchanged and never replace different content",
            action.src, action.dest
        ),
        Action::WriteFile(action) => format!(
            "atomically write {} exact UTF-8 bytes to {} with conflict policy {}",
            action.content.len(),
            action.path,
            match action.on_conflict {
                WriteConflictPolicy::Fail => "fail",
                WriteConflictPolicy::Replace => "replace",
            }
        ),
        Action::RemovePath(action) => format!(
            "move {} to the system Trash/Recycle Bin; never fall back to permanent deletion",
            action.path
        ),
        Action::GitClone { repo, dest, branch } => format!(
            "ensure git repository {} at {}: clone when absent; otherwise fetch origin and fast-forward {} when safe (hooks and submodules disabled)",
            repo,
            dest,
            branch
                .as_deref()
                .map(|branch| format!("branch {branch}"))
                .unwrap_or_else(|| "the checked-out branch".into())
        ),
        Action::GitInspect { repo, dest } => format!(
            "inspect whether {} contains the expected git repository {} without changing it",
            dest, repo
        ),
        Action::GitCloneIfMissing { repo, dest, branch } => format!(
            "clone git repository {} into {} only when it is absent{} (hooks and submodules disabled)",
            repo,
            dest,
            branch
                .as_deref()
                .map(|branch| format!("; branch {branch}"))
                .unwrap_or_default()
        ),
        Action::GitFetch {
            repo,
            dest,
            branch,
        } => format!(
            "fetch origin/{} for the verified repository {} at {} without changing local branches",
            branch, repo, dest
        ),
        Action::GitFastForward {
            repo,
            dest,
            branch,
        } => format!(
            "safely fast-forward local {} to the already fetched origin/{} for {} at {}; never reset, stash, merge, or overwrite local work",
            branch, branch, repo, dest
        ),
        Action::BrewInstall { package, cask } => {
            format!(
                "brew install {}{}",
                if *cask { "--cask " } else { "" },
                package
            )
        }
        Action::RunCommand {
            program,
            args,
            cwd,
            shell,
            ..
        } => format!(
            "run {} {:?}{}{}",
            program,
            args,
            cwd.as_ref()
                .map(|d| format!(" in {}", d))
                .unwrap_or_default(),
            if matches!(shell, ShellMode::Allow) {
                " with shell"
            } else {
                ""
            }
        ),
        Action::RunScript {
            interpreter,
            script,
            args,
            cwd,
            success_exit_codes,
            ..
        } => format!(
            "run {} script {}{}{}; treat exit codes [{}] as success",
            script_interpreter_name(*interpreter),
            script,
            if args.is_empty() {
                String::new()
            } else {
                format!(" with arguments {:?}", args)
            },
            cwd.as_ref()
                .map(|directory| format!(" in {}", directory))
                .unwrap_or_default(),
            format_exit_codes(success_exit_codes)
        ),
        Action::ConfigurePackageRegistryFiles {
            secrets,
            npm,
            nuget,
        } => package_registry::plan_summary(secrets, npm, nuget)?,
        Action::DownloadFile { url, dest, .. } => {
            format!("download {} to {} with sha256 verification", url, dest)
        }
        Action::ExtractArchive {
            src,
            dest,
            format,
            max_unpacked_bytes,
        } => {
            format!(
                "extract {} archive {} into {} atomically with traversal and link protection (maximum {} unpacked bytes)",
                archive_format_name(*format),
                src,
                dest,
                max_unpacked_bytes
            )
        }
        Action::InstallDmg {
            dmg,
            app_name,
            target,
            identity,
        } => {
            let identity_summary = identity
                .as_ref()
                .map(|identity| {
                    format!(
                        ", require bundle {} version {} signed by team {}",
                        identity.bundle_identifier, identity.version, identity.team_identifier
                    )
                })
                .unwrap_or_default();
            format!(
                "verify and mount {} read-only, validate signature{}, install {} into {}",
                dmg,
                identity_summary,
                app_name.as_deref().unwrap_or("the only .app bundle"),
                target.as_deref().unwrap_or("$HOME/Applications")
            )
        }
        Action::InstallPkg { pkg, target } => format!(
            "validate pkg signature for {} and install to {}",
            pkg,
            target.as_deref().unwrap_or("/")
        ),
        Action::MacosRequirements {
            minimum_version,
            require_rosetta_on_apple_silicon,
        } => format!(
            "require macOS {} or newer{}",
            minimum_version,
            if *require_rosetta_on_apple_silicon {
                " and Rosetta on Apple Silicon"
            } else {
                ""
            }
        ),
        Action::AppStoreInstall(action) => format!(
            "delegate an App Store {} request for application {} to standalone ppstore 0.1.x",
            app_store_operation_name(action.operation),
            action.app_id
        ),
        Action::BambuStudioRelease(action) => format!(
            "resolve the latest Bambu Studio {} from the official GitHub releases, compare its version with the signed installed app, and install only when newer",
            release_channel_name(opts.release_channel.unwrap_or(action.channel))
        ),
        Action::ActivateLicense(action) => match (&action.provider, &action.method) {
            (LicenseProvider::LightBurn, LicenseMethod::VendorUi) =>
                "launch LightBurn and wait for user-confirmed activation in its License Page; the license key is entered only in LightBurn"
                    .into(),
        },
    };
    Ok(match &step.when {
        Some(condition) => format!("{}: {}", describe_condition(condition), summary),
        None => summary,
    })
}

fn prerequisites_for_step(step: &Step) -> Vec<String> {
    let mut prerequisites = Vec::new();
    match step.auth {
        AuthPolicy::None => {}
        AuthPolicy::GitCredential => prerequisites.push(
            "authenticate with git once if credentials are not already available; reuse the existing credential helper or SSH agent afterwards"
                .into(),
        ),
        AuthPolicy::Sudo => prerequisites.push(
            "authenticate with sudo once if the session does not already have an active sudo timestamp; later elevated steps can reuse it until the sudo timeout expires"
                .into(),
        ),
    }
    if matches!(&step.action, Action::GithubListRepositories) {
        prerequisites.push(
            "GitHub CLI must be installed and authenticated for github.com; credentials remain owned by GitHub CLI"
                .into(),
        );
    }
    if matches!(&step.action, Action::ActivateLicense(_)) {
        prerequisites.push(
            "enter the license key only in the vendor application; ppduster does not read, store, or log it"
                .into(),
        );
    }
    if let Action::AppStoreInstall(action) = &step.action {
        prerequisites.push(
            "install a trusted ppstore 0.1.x executable separately; optionally select it with the absolute PPDUSTER_PPSTORE_PATH override"
                .into(),
        );
        prerequisites.push(
            "sign in to the Mac App Store with the Apple Account that owns the application; authentication stays in Apple's UI"
                .into(),
        );
        if matches!(action.operation, AppStoreOperation::Install) {
            prerequisites.push(
                "the application must already be obtained or purchased; use operation: get for a free app that is not yet associated with the account"
                    .into(),
            );
        }
    }
    if let Action::RunScript { interpreter, .. } = &step.action {
        prerequisites.push(format!(
            "{} must be installed and available to ppduster",
            script_interpreter_requirement(*interpreter)
        ));
    }
    if let Some(condition) = &step.when {
        prerequisites.push(format!("when: {}", describe_condition(condition)));
    }
    if let Some(condition) = &step.require {
        prerequisites.push(format!("require: {}", describe_condition(condition)));
    }
    prerequisites
}

fn describe_condition(condition: &StepCondition) -> String {
    match condition {
        StepCondition::ExitCode { step, codes } => format!(
            "step {} returns one of [{}]",
            step,
            format_exit_codes(codes)
        ),
        StepCondition::Path { path, expect } => {
            format!("path {} matches{}", path, describe_path_expectation(expect))
        }
        StepCondition::All { conditions } => format!(
            "all ({})",
            conditions
                .iter()
                .map(describe_condition)
                .collect::<Vec<_>>()
                .join("; ")
        ),
        StepCondition::Any { conditions } => format!(
            "any ({})",
            conditions
                .iter()
                .map(describe_condition)
                .collect::<Vec<_>>()
                .join("; ")
        ),
        StepCondition::Not { condition } => format!("not ({})", describe_condition(condition)),
        StepCondition::Expression { policy, .. } => format!(
            "typed context rule (null: {:?}, missing: {:?}, unknown: {:?})",
            policy.on_null, policy.on_missing, policy.on_unknown
        ),
    }
}

fn step_name(step: &Step) -> String {
    if step.name.trim().is_empty() {
        step.id.clone()
    } else {
        step.name.clone()
    }
}

#[derive(Debug)]
enum ConditionEvaluation {
    Matched(String),
    NotMatched(String),
    Unavailable(String),
}

fn evaluate_condition(
    condition: &StepCondition,
    completed_steps: &[StepReport],
    task_steps: &[Step],
    consumer_step_id: &str,
) -> Result<ConditionEvaluation> {
    match condition {
        StepCondition::ExitCode { step, codes } => {
            let source = completed_steps
                .iter()
                .find(|report| report.step_id == *step)
                .ok_or_else(|| anyhow!("condition source step {} has not run", step))?;
            let Some(output) = &source.output else {
                return Ok(ConditionEvaluation::Unavailable(format!(
                    "step {} produced no exit code",
                    step
                )));
            };
            let StepOutput::ProcessExit(process) = output else {
                bail!(
                    "condition source step {} did not produce process exit output",
                    step
                );
            };
            let Some(exit_code) = process.exit_code else {
                return Ok(ConditionEvaluation::Unavailable(format!(
                    "step {} did not exit normally",
                    step
                )));
            };
            if codes.contains(&exit_code) {
                Ok(ConditionEvaluation::Matched(format!(
                    "step {} returned accepted branch code {}",
                    step, exit_code
                )))
            } else {
                Ok(ConditionEvaluation::NotMatched(format!(
                    "step {} returned exit code {}; expected one of [{}]",
                    step,
                    exit_code,
                    format_exit_codes(codes)
                )))
            }
        }
        StepCondition::Path { path, expect } => {
            let action = InspectPathAction {
                path: path.clone(),
                recursive_size: expect.min_size_bytes.is_some() || expect.max_size_bytes.is_some(),
                sha256: expect.sha256.is_some(),
                expect: Some(expect.clone()),
            };
            let metadata = inspect_path(&action)?;
            match verify_path_expectation(expect, &metadata) {
                Ok(()) => Ok(ConditionEvaluation::Matched(summarize_path_metadata(
                    &metadata,
                ))),
                Err(error) => Ok(ConditionEvaluation::NotMatched(error.to_string())),
            }
        }
        StepCondition::All { conditions } => {
            let mut matched = Vec::with_capacity(conditions.len());
            let mut unavailable = Vec::new();
            for child in conditions {
                match evaluate_condition(child, completed_steps, task_steps, consumer_step_id)? {
                    ConditionEvaluation::Matched(reason) => matched.push(reason),
                    ConditionEvaluation::NotMatched(reason) => {
                        return Ok(ConditionEvaluation::NotMatched(format!(
                            "all condition failed: {reason}"
                        )));
                    }
                    ConditionEvaluation::Unavailable(reason) => unavailable.push(reason),
                }
            }
            if !unavailable.is_empty() {
                return Ok(ConditionEvaluation::Unavailable(format!(
                    "all condition is unavailable: {}",
                    unavailable.join("; ")
                )));
            }
            Ok(ConditionEvaluation::Matched(format!(
                "all conditions matched: {}",
                matched.join("; ")
            )))
        }
        StepCondition::Any { conditions } => {
            let mut unmatched = Vec::with_capacity(conditions.len());
            let mut unavailable = Vec::new();
            for child in conditions {
                match evaluate_condition(child, completed_steps, task_steps, consumer_step_id)? {
                    ConditionEvaluation::Matched(reason) => {
                        return Ok(ConditionEvaluation::Matched(format!(
                            "any condition matched: {reason}"
                        )));
                    }
                    ConditionEvaluation::NotMatched(reason) => unmatched.push(reason),
                    ConditionEvaluation::Unavailable(reason) => unavailable.push(reason),
                }
            }
            if !unavailable.is_empty() {
                return Ok(ConditionEvaluation::Unavailable(format!(
                    "any condition is unavailable: {}",
                    unavailable.join("; ")
                )));
            }
            Ok(ConditionEvaluation::NotMatched(format!(
                "no any branch matched: {}",
                unmatched.join("; ")
            )))
        }
        StepCondition::Not { condition } => {
            match evaluate_condition(condition, completed_steps, task_steps, consumer_step_id)? {
                ConditionEvaluation::Matched(reason) => Ok(ConditionEvaluation::NotMatched(
                    format!("negated condition matched: {reason}"),
                )),
                ConditionEvaluation::NotMatched(reason) => Ok(ConditionEvaluation::Matched(
                    format!("negated condition did not match: {reason}"),
                )),
                ConditionEvaluation::Unavailable(reason) => Ok(ConditionEvaluation::Unavailable(
                    format!("negated condition is unavailable: {reason}"),
                )),
            }
        }
        StepCondition::Expression { rule, policy } => {
            let schemas = context_schema_store_before(task_steps, consumer_step_id)?;
            let values = context_store_from_steps(task_steps, completed_steps)?;
            let checked = check_rule(rule.clone(), &schemas, ExpressionLimits::default()).map_err(
                |diagnostics| {
                    anyhow!(
                        "context rule failed type-check: {}",
                        diagnostics
                            .into_iter()
                            .map(|diagnostic| format!(
                                "{:?} at {}: {}",
                                diagnostic.code, diagnostic.location, diagnostic.message
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                },
            )?;
            match checked.evaluate_rule(&values) {
                RuleEvaluation::True => Ok(ConditionEvaluation::Matched(
                    "context rule evaluated to true".into(),
                )),
                RuleEvaluation::False => Ok(ConditionEvaluation::NotMatched(
                    "context rule evaluated to false".into(),
                )),
                RuleEvaluation::Null => {
                    apply_indeterminate_policy(policy.on_null, "context rule evaluated to null")
                }
                RuleEvaluation::Missing(issue) => apply_indeterminate_policy(
                    policy.on_missing,
                    &format!("context rule input is missing: {}", issue.message),
                ),
                RuleEvaluation::Unknown(issue) => apply_indeterminate_policy(
                    policy.on_unknown,
                    &format!("context rule input is unavailable: {}", issue.message),
                ),
                RuleEvaluation::Error(error) => bail!(
                    "context rule evaluation failed ({:?}): {}",
                    error.kind,
                    error.message
                ),
            }
        }
    }
}

fn apply_indeterminate_policy(
    policy: IndeterminatePolicy,
    reason: &str,
) -> Result<ConditionEvaluation> {
    match policy {
        IndeterminatePolicy::Fail => bail!("{reason}"),
        IndeterminatePolicy::TreatAsFalse => Ok(ConditionEvaluation::NotMatched(reason.into())),
        IndeterminatePolicy::TreatAsTrue => Ok(ConditionEvaluation::Matched(reason.into())),
    }
}

fn ensure_auth(step: &Step, state: &mut AuthState) -> Result<()> {
    match step.auth {
        AuthPolicy::None => Ok(()),
        AuthPolicy::GitCredential => {
            if state.git_authenticated || git_auth_ready() {
                state.git_authenticated = true;
                return Ok(());
            }
            prompt_once(
                "Git authentication is required. Press Enter to continue and complete the normal git credential prompt if it appears.",
            )?;
            state.git_authenticated = true;
            Ok(())
        }
        AuthPolicy::Sudo => {
            if state.sudo_authenticated || sudo_auth_ready()? {
                state.sudo_authenticated = true;
                return Ok(());
            }
            prompt_once(
                "sudo authentication is required. Press Enter to continue; you may be prompted for your password once.",
            )?;
            Command::new("/usr/bin/sudo")
                .arg("-v")
                .status()
                .context("refresh sudo credentials")?
                .exit_ok("refresh sudo credentials")?;
            state.sudo_authenticated = true;
            Ok(())
        }
    }
}

fn step_requires_auth_prompt(step: &Step, state: &AuthState) -> Result<bool> {
    match step.auth {
        AuthPolicy::None => Ok(false),
        AuthPolicy::GitCredential => Ok(!(state.git_authenticated || git_auth_ready())),
        AuthPolicy::Sudo => Ok(!(state.sudo_authenticated || sudo_auth_ready()?)),
    }
}

fn git_auth_ready() -> bool {
    std::env::var_os("SSH_AUTH_SOCK").is_some() || git_has_credential_helper()
}

fn git_has_credential_helper() -> bool {
    Command::new("git")
        .args(["config", "--get-all", "credential.helper"])
        .output()
        .map(|output| {
            output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        })
        .unwrap_or(false)
}

fn sudo_auth_ready() -> Result<bool> {
    Ok(Command::new("/usr/bin/sudo")
        .args(["-n", "true"])
        .status()
        .context("check sudo credential cache")?
        .success())
}

fn prompt_once(message: &str) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!(
            "interactive authorization is required, but stdin is not a TTY; rerun in an interactive terminal"
        );
    }
    eprint!("{message}\nPress Enter to continue: ");
    io::stderr().flush().context("flush auth prompt")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("read auth confirmation")?;
    Ok(())
}

fn apply_step(task_id: &str, step: &Step, opts: &RunOptions) -> Result<ApplyStepResult> {
    match &step.action {
        Action::GithubListRepositories => apply_github_list_repositories(),
        Action::GithubSelectRepositories {
            github,
            expected_account_login,
            repository_ids,
        } => apply_github_select_repositories(github, expected_account_login, repository_ids),
        Action::ForEach { .. } | Action::ForEachGitCloneIfMissing { .. } => {
            bail!("foreach actions must be executed by the scenario runner")
        }
        Action::CreateDirectory(action) => apply_create_directory(&action.path),
        Action::InspectPath(action) => {
            let metadata = inspect_path(action)?;
            if let Some(expectation) = &action.expect {
                verify_path_expectation(expectation, &metadata)?;
            }
            Ok(ApplyStepResult::AlreadySatisfied(summarize_path_metadata(
                &metadata,
            )))
        }
        Action::CopyPath(action) => apply_copy_path(&action.src, &action.dest),
        Action::WriteFile(action) => {
            apply_write_file(&action.path, &action.content, action.on_conflict)
        }
        Action::RemovePath(action) => apply_remove_path(&action.path),
        Action::GitClone { repo, dest, branch } => {
            let context = GitDestinationApprovalContext {
                task_id,
                step_id: &step.id,
                operation: ProtectedPathOperation::GitCloneOrUpdate,
                repository: repo,
                branch: branch.as_deref(),
                approvals: &opts.protected_path_approvals,
            };
            finish_git_action(
                dest,
                context,
                apply_git_clone_or_update(repo, dest, branch.as_deref(), context),
            )
        }
        Action::GitInspect { repo, dest } => apply_git_inspect(repo, dest),
        Action::GitCloneIfMissing { repo, dest, branch } => {
            let context = GitDestinationApprovalContext {
                task_id,
                step_id: &step.id,
                operation: ProtectedPathOperation::GitCloneIfMissing,
                repository: repo,
                branch: branch.as_deref(),
                approvals: &opts.protected_path_approvals,
            };
            finish_git_action(
                dest,
                context,
                apply_git_clone_if_missing(repo, dest, branch.as_deref(), context),
            )
        }
        Action::GitFetch { repo, dest, branch } => {
            let context = GitDestinationApprovalContext {
                task_id,
                step_id: &step.id,
                operation: ProtectedPathOperation::GitFetch,
                repository: repo,
                branch: Some(branch),
                approvals: &opts.protected_path_approvals,
            };
            finish_git_action(dest, context, apply_git_fetch(repo, dest, branch, context))
        }
        Action::GitFastForward { repo, dest, branch } => {
            let context = GitDestinationApprovalContext {
                task_id,
                step_id: &step.id,
                operation: ProtectedPathOperation::GitFastForward,
                repository: repo,
                branch: Some(branch),
                approvals: &opts.protected_path_approvals,
            };
            finish_git_action(
                dest,
                context,
                apply_git_fast_forward(repo, dest, branch, context),
            )
        }
        Action::BrewInstall { package, cask } => {
            apply_brew_install(package, *cask).map(ApplyStepResult::Applied)
        }
        Action::RunCommand {
            program,
            args,
            cwd,
            env,
            shell,
        } => apply_run_command(program, args, cwd.as_deref(), env, *shell),
        Action::RunScript {
            interpreter,
            script,
            args,
            cwd,
            env,
            success_exit_codes,
        } => apply_run_script(
            *interpreter,
            script,
            args,
            cwd.as_deref(),
            env,
            success_exit_codes,
        ),
        Action::ConfigurePackageRegistryFiles {
            secrets,
            npm,
            nuget,
        } => package_registry::apply(secrets, npm, nuget).map(ApplyStepResult::Applied),
        Action::DownloadFile {
            url,
            dest,
            checksum,
        } => apply_download_file(url, dest, checksum).map(ApplyStepResult::Applied),
        Action::ExtractArchive {
            src,
            dest,
            format,
            max_unpacked_bytes,
        } => apply_extract_archive(src, dest, *format, *max_unpacked_bytes)
            .map(ApplyStepResult::Applied),
        Action::InstallDmg {
            dmg,
            app_name,
            target,
            identity,
        } => apply_install_dmg(
            dmg,
            app_name.as_deref(),
            target.as_deref(),
            identity.as_ref(),
            false,
        )
        .map(ApplyStepResult::Applied),
        Action::InstallPkg { pkg, target } => {
            apply_install_pkg(pkg, target.as_deref()).map(ApplyStepResult::Applied)
        }
        Action::MacosRequirements {
            minimum_version,
            require_rosetta_on_apple_silicon,
        } => apply_macos_requirements(minimum_version, *require_rosetta_on_apple_silicon)
            .map(ApplyStepResult::Applied),
        Action::AppStoreInstall(action) => apply_app_store_install(action.app_id, action.operation),
        Action::BambuStudioRelease(action) => {
            apply_bambu_studio_release(opts.release_channel.unwrap_or(action.channel))
                .map(ApplyStepResult::Applied)
        }
        Action::ActivateLicense(action) => {
            apply_activate_license(action.provider, action.method).map(ApplyStepResult::Applied)
        }
    }
}

fn apply_github_list_repositories() -> Result<ApplyStepResult> {
    let account = get_account_repositories()?;
    let login = account.login;
    let repositories = account
        .repositories
        .into_iter()
        .map(|repository| GithubRepositoryOutput {
            id: repository.id,
            owner: repository.owner,
            name: repository.name,
            full_name: repository.name_with_owner,
            https_url: repository.url,
            ssh_url: repository.ssh_url,
            default_branch: repository.default_branch,
            private: repository.is_private,
            archived: repository.is_archived,
        })
        .collect::<Vec<_>>();
    let summary = format!("found {} GitHub repositories", repositories.len());
    Ok(ApplyStepResult::AppliedWithOutput {
        summary,
        output: StepOutput::GithubRepositories(GithubRepositoriesOutput {
            github: GithubContextOutput {
                account: GithubAccountOutput { login },
                repositories,
            },
        }),
    })
}

fn apply_github_select_repositories(
    github: &GithubContextInput,
    expected_account_login: &str,
    repository_ids: &[String],
) -> Result<ApplyStepResult> {
    if github.account.login != expected_account_login {
        bail!(
            "GitHub repository selection was authored for account {:?}, but the freshly listed account is {:?}; refresh the repository preview and confirm the selection again",
            expected_account_login,
            github.account.login
        );
    }

    let mut repositories_by_id = BTreeMap::new();
    for repository in &github.repositories {
        if repositories_by_id
            .insert(repository.id.as_str(), repository)
            .is_some()
        {
            bail!(
                "fresh GitHub repository output contains duplicate node ID {:?}",
                repository.id
            );
        }
    }

    let mut selected = Vec::with_capacity(repository_ids.len());
    for repository_id in repository_ids {
        let repository = repositories_by_id.get(repository_id.as_str()).ok_or_else(|| {
            anyhow!(
                "selected GitHub repository ID {:?} is no longer visible to account {:?}; refresh the repository preview and confirm the selection again",
                repository_id,
                expected_account_login
            )
        })?;
        selected.push(github_repository_input_output(repository));
    }

    let summary = format!(
        "selected {} GitHub repositories for account {}",
        selected.len(),
        expected_account_login
    );
    Ok(ApplyStepResult::AppliedWithOutput {
        summary,
        output: StepOutput::GithubRepositories(GithubRepositoriesOutput {
            github: GithubContextOutput {
                account: GithubAccountOutput {
                    login: github.account.login.clone(),
                },
                repositories: selected,
            },
        }),
    })
}

fn github_repository_input_output(repository: &GithubRepositoryInput) -> GithubRepositoryOutput {
    GithubRepositoryOutput {
        id: repository.id.clone(),
        owner: repository.owner.clone(),
        name: repository.name.clone(),
        full_name: repository.full_name.clone(),
        https_url: repository.https_url.clone(),
        ssh_url: repository.ssh_url.clone(),
        default_branch: repository.default_branch.clone(),
        private: repository.private,
        archived: repository.archived,
    }
}

fn apply_create_directory(raw_path: &str) -> Result<ApplyStepResult> {
    let path = validate_create_directory_path(raw_path)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            return Ok(ApplyStepResult::AlreadySatisfied(format!(
                "directory already exists: {}",
                path.display()
            )));
        }
        Ok(_) => {
            // `validate_create_directory_path` reports the more specific
            // conflict; this protects against a race between both checks.
            validate_create_directory_path(raw_path)?;
            bail!(
                "directory destination changed while applying: {}",
                path.display()
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect directory {}", path.display()));
        }
    }

    fs::create_dir_all(&path)
        .with_context(|| format!("create directory and parents {}", path.display()))?;
    let verified = validate_create_directory_path(raw_path)?;
    let metadata = fs::symlink_metadata(&verified)
        .with_context(|| format!("verify created directory {}", verified.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "created path is not a real directory: {}",
            verified.display()
        );
    }
    Ok(ApplyStepResult::Applied(format!(
        "created directory: {}",
        verified.display()
    )))
}

fn validate_safe_mutation_path(raw_path: &str, operation: &str) -> Result<PathBuf> {
    let path = validate_declared_path(raw_path)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} path has no parent: {}", operation, path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} must not target a filesystem root", operation))?;
    // Canonicalize only the parent. Canonicalizing the final component would
    // turn a symlink deletion into a safety decision about its target rather
    // than about the link's own location.
    let resolved_parent = resolve_through_existing_ancestor(parent)?;
    let effective_location = resolved_parent.join(file_name);
    if !is_safe_rule_root(&effective_location) {
        bail!(
            "{} path is protected or too broad: {}",
            operation,
            path.display()
        );
    }
    Ok(path)
}

fn ensure_destination_parent(path: &Path) -> Result<&Path> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent: {}", path.display()))?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "destination parent must not be a symlink: {}",
                parent.display()
            )
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => bail!(
            "destination parent is not a directory: {}",
            parent.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "destination parent does not exist; add a create-directory step first: {}",
            parent.display()
        ),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect destination parent {}", parent.display()));
        }
    }
    let metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("verify destination parent {}", parent.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "destination parent is not a real directory: {}",
            parent.display()
        );
    }
    Ok(parent)
}

fn apply_write_file(
    raw_path: &str,
    content: &str,
    on_conflict: WriteConflictPolicy,
) -> Result<ApplyStepResult> {
    let path = validate_safe_mutation_path(raw_path, "write-file")?;
    let existing_permissions = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "write-file destination is not a regular file: {}",
                    path.display()
                );
            }
            if file_matches_bytes(&path, content.as_bytes())? {
                return Ok(ApplyStepResult::AlreadySatisfied(format!(
                    "file already has the requested content: {}",
                    path.display()
                )));
            }
            if matches!(on_conflict, WriteConflictPolicy::Fail) {
                bail!(
                    "write-file destination has different content (set on_conflict: replace to replace it): {}",
                    path.display()
                );
            }
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect write destination {}", path.display()));
        }
    };

    let parent = ensure_destination_parent(&path)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create staged file in {}", parent.display()))?;
    staged
        .write_all(content.as_bytes())
        .with_context(|| format!("write staged content for {}", path.display()))?;
    if let Some(permissions) = existing_permissions {
        staged
            .as_file()
            .set_permissions(permissions)
            .with_context(|| format!("preserve permissions for {}", path.display()))?;
    }
    staged
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("sync staged content for {}", path.display()))?;

    match on_conflict {
        WriteConflictPolicy::Fail => staged.persist_noclobber(&path).map_err(|error| {
            anyhow!(error.error)
                .context(format!("commit file without replacing {}", path.display()))
        })?,
        WriteConflictPolicy::Replace => staged.persist(&path).map_err(|error| {
            anyhow!(error.error).context(format!("atomically replace {}", path.display()))
        })?,
    };
    sync_parent_directory(parent)?;
    if !file_matches_bytes(&path, content.as_bytes())? {
        bail!(
            "written file failed content verification: {}",
            path.display()
        );
    }
    Ok(ApplyStepResult::Applied(format!(
        "wrote {} bytes atomically to {}",
        content.len(),
        path.display()
    )))
}

fn apply_copy_path(raw_src: &str, raw_dest: &str) -> Result<ApplyStepResult> {
    let src = validate_copy_source(raw_src)?;
    let dest = validate_safe_mutation_path(raw_dest, "copy-path")?;
    validate_copy_relationship(&src, &dest)?;
    if path_entry_exists(&dest)? {
        if paths_have_equal_content(&src, &dest)? {
            return Ok(ApplyStepResult::AlreadySatisfied(format!(
                "destination already matches source: {}",
                dest.display()
            )));
        }
        bail!(
            "copy-path destination exists with different content: {}",
            dest.display()
        );
    }
    let parent = ensure_destination_parent(&dest)?;
    let source_metadata = fs::symlink_metadata(&src)
        .with_context(|| format!("inspect copy source {}", src.display()))?;
    if source_metadata.is_file() {
        copy_file_noclobber(&src, &dest, parent)?;
    } else {
        copy_directory_noclobber(&src, &dest, parent)?;
    }
    if !paths_have_equal_content(&src, &dest)? {
        bail!("copied path failed verification: {}", dest.display());
    }
    Ok(ApplyStepResult::Applied(format!(
        "copied {} to {}",
        src.display(),
        dest.display()
    )))
}

fn apply_remove_path(raw_path: &str) -> Result<ApplyStepResult> {
    apply_remove_path_with(raw_path, move_to_system_trash)
}

fn apply_remove_path_with<F>(raw_path: &str, move_to_trash: F) -> Result<ApplyStepResult>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let path = validate_safe_mutation_path(raw_path, "remove-path")?;
    if !path_entry_exists(&path)? {
        return Ok(ApplyStepResult::AlreadySatisfied(format!(
            "path is already absent: {}",
            path.display()
        )));
    }
    // `trash` canonicalizes the parent but deliberately retains the final file
    // name, so a final symlink is moved as a link rather than followed.
    move_to_trash(&path).with_context(|| format!("move {} to the system Trash", path.display()))?;
    if path_entry_exists(&path)? {
        bail!(
            "path still exists after moving it to Trash: {}",
            path.display()
        );
    }
    Ok(ApplyStepResult::Applied(format!(
        "moved path to the system Trash: {}",
        path.display()
    )))
}

#[cfg(target_os = "macos")]
fn move_to_system_trash(path: &Path) -> Result<()> {
    use trash::macos::{DeleteMethod, TrashContextExtMacos};

    let mut context = trash::TrashContext::new();
    context.set_delete_method(DeleteMethod::NsFileManager);
    context.delete(path).map_err(anyhow::Error::msg)
}

#[cfg(not(target_os = "macos"))]
fn move_to_system_trash(path: &Path) -> Result<()> {
    trash::delete(path).map_err(anyhow::Error::msg)
}

fn validate_copy_source(raw_src: &str) -> Result<PathBuf> {
    let src = validate_declared_path(raw_src)?;
    if src.parent().is_none() {
        bail!(
            "copy-path source must not be a filesystem root: {}",
            src.display()
        );
    }
    let metadata = fs::symlink_metadata(&src)
        .with_context(|| format!("inspect copy source {}", src.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("copy-path source must not be a symlink: {}", src.display());
    }
    if !metadata.is_file() && !metadata.is_dir() {
        bail!(
            "copy-path source must be a regular file or directory: {}",
            src.display()
        );
    }
    if metadata.is_dir() {
        validate_copy_tree(&src)?;
    }
    Ok(src)
}

fn validate_copy_tree(root: &Path) -> Result<()> {
    const MAX_COPY_ENTRIES: u64 = 100_000;
    const MAX_COPY_BYTES: u64 = 10 * 1024 * 1024 * 1024;
    const MAX_COPY_DEPTH: usize = 128;

    let mut entries = 0u64;
    let mut bytes = 0u64;
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.with_context(|| format!("walk copy source {}", root.display()))?;
        if entry.depth() > MAX_COPY_DEPTH {
            bail!(
                "copy-path source exceeds maximum depth {}: {}",
                MAX_COPY_DEPTH,
                entry.path().display()
            );
        }
        entries = entries
            .checked_add(1)
            .ok_or_else(|| anyhow!("copy-path entry count overflow for {}", root.display()))?;
        if entries > MAX_COPY_ENTRIES {
            bail!(
                "copy-path source exceeds maximum of {} entries: {}",
                MAX_COPY_ENTRIES,
                root.display()
            );
        }
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            bail!(
                "copy-path does not follow or copy symlinks: {}",
                entry.path().display()
            );
        }
        if !file_type.is_file() && !file_type.is_dir() {
            bail!(
                "copy-path supports only regular files and directories: {}",
                entry.path().display()
            );
        }
        if file_type.is_file() {
            let length = entry
                .metadata()
                .with_context(|| format!("read copy source metadata {}", entry.path().display()))?
                .len();
            bytes = bytes
                .checked_add(length)
                .ok_or_else(|| anyhow!("copy-path size overflow for {}", root.display()))?;
            if bytes > MAX_COPY_BYTES {
                bail!(
                    "copy-path source exceeds maximum of {} bytes: {}",
                    MAX_COPY_BYTES,
                    root.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_copy_relationship(src: &Path, dest: &Path) -> Result<()> {
    let source = src
        .canonicalize()
        .with_context(|| format!("resolve copy source {}", src.display()))?;
    let destination = resolve_through_existing_ancestor(dest)?;
    if destination == source || destination.starts_with(&source) || source.starts_with(&destination)
    {
        bail!(
            "copy source and destination must be distinct, non-nested paths: {} -> {}",
            src.display(),
            dest.display()
        );
    }
    Ok(())
}

fn copy_file_noclobber(src: &Path, dest: &Path, parent: &Path) -> Result<()> {
    let mut input =
        File::open(src).with_context(|| format!("open copy source {}", src.display()))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create staged copy in {}", parent.display()))?;
    io::copy(&mut input, &mut staged)
        .with_context(|| format!("copy {} into staging", src.display()))?;
    staged
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("sync staged copy for {}", dest.display()))?;
    let permissions = fs::symlink_metadata(src)
        .with_context(|| format!("read permissions for {}", src.display()))?
        .permissions();
    staged
        .as_file()
        .set_permissions(permissions)
        .with_context(|| format!("set staged permissions for {}", dest.display()))?;
    staged.persist_noclobber(dest).map_err(|error| {
        anyhow!(error.error).context(format!("commit copy without replacing {}", dest.display()))
    })?;
    sync_parent_directory(parent)?;
    Ok(())
}

fn copy_directory_noclobber(src: &Path, dest: &Path, parent: &Path) -> Result<()> {
    let staging = tempfile::Builder::new()
        .prefix(".ppduster-copy-")
        .tempdir_in(parent)
        .with_context(|| format!("create staged directory in {}", parent.display()))?;
    for entry in WalkDir::new(src).follow_links(false).min_depth(1) {
        let entry = entry.with_context(|| format!("walk copy source {}", src.display()))?;
        let relative = entry
            .path()
            .strip_prefix(src)
            .with_context(|| format!("derive relative copy path for {}", entry.path().display()))?;
        let target = staging.path().join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir(&target)
                .with_context(|| format!("create staged directory {}", target.display()))?;
        } else if entry.file_type().is_file() {
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "copy {} to staging {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        } else {
            bail!(
                "copy source changed to unsupported entry: {}",
                entry.path().display()
            );
        }
    }
    let staging_path = staging.keep();
    if let Err(error) = rename_directory_noreplace(&staging_path, dest) {
        let _ = fs::remove_dir_all(&staging_path);
        return Err(error).with_context(|| format!("commit directory copy {}", dest.display()));
    }
    sync_parent_directory(parent)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    File::open(parent)
        .with_context(|| format!("open destination parent {} for sync", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync destination parent {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn rename_directory_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both pointers refer to live, NUL-terminated C strings for the
    // duration of the call. RENAME_EXCL gives the required no-clobber commit.
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn rename_directory_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both pointers are valid C strings; AT_FDCWD selects absolute or
    // process-relative paths and RENAME_NOREPLACE forbids replacement.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_directory_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "copy destination already exists",
        ));
    }
    // The native Rust rename primitive is no-clobber on the primary supported
    // non-Unix target (Windows). The preceding check also gives conservative
    // behavior on less common targets.
    fs::rename(source, destination)
}

fn paths_have_equal_content(left: &Path, right: &Path) -> Result<bool> {
    let left_metadata =
        fs::symlink_metadata(left).with_context(|| format!("inspect {}", left.display()))?;
    let right_metadata =
        fs::symlink_metadata(right).with_context(|| format!("inspect {}", right.display()))?;
    if left_metadata.file_type().is_symlink() || right_metadata.file_type().is_symlink() {
        return Ok(false);
    }
    if left_metadata.is_file() && right_metadata.is_file() {
        if left_metadata.len() != right_metadata.len() {
            return Ok(false);
        }
        return Ok(sha256_file(left)? == sha256_file(right)?);
    }
    if !left_metadata.is_dir() || !right_metadata.is_dir() {
        return Ok(false);
    }
    directory_trees_equal(left, right)
}

fn directory_trees_equal(left: &Path, right: &Path) -> Result<bool> {
    validate_copy_tree(left)?;
    validate_copy_tree(right)?;
    let mut left_entries = WalkDir::new(left)
        .follow_links(false)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter();
    let mut right_entries = WalkDir::new(right)
        .follow_links(false)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter();
    loop {
        let left_entry = left_entries
            .next()
            .transpose()
            .with_context(|| format!("walk {}", left.display()))?;
        let right_entry = right_entries
            .next()
            .transpose()
            .with_context(|| format!("walk {}", right.display()))?;
        let (left_entry, right_entry) = match (left_entry, right_entry) {
            (None, None) => return Ok(true),
            (Some(left_entry), Some(right_entry)) => (left_entry, right_entry),
            _ => return Ok(false),
        };
        let left_relative = left_entry.path().strip_prefix(left)?;
        let right_relative = right_entry.path().strip_prefix(right)?;
        if left_relative != right_relative || left_entry.file_type() != right_entry.file_type() {
            return Ok(false);
        }
        if left_entry.file_type().is_file()
            && !paths_have_equal_content(left_entry.path(), right_entry.path())?
        {
            return Ok(false);
        }
    }
}

fn file_matches_bytes(path: &Path, expected: &[u8]) -> Result<bool> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(false);
    }
    if metadata.len() != expected.len() as u64 {
        return Ok(false);
    }
    let mut file = File::open(path).with_context(|| format!("open file {}", path.display()))?;
    let mut actual = Vec::with_capacity(expected.len());
    file.read_to_end(&mut actual)
        .with_context(|| format!("read file {}", path.display()))?;
    Ok(actual == expected)
}

fn inspect_path(action: &InspectPathAction) -> Result<PathMetadataOutput> {
    let path = validate_declared_path(&action.path)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PathMetadataOutput {
                path,
                exists: false,
                kind: None,
                size_bytes: None,
                empty: None,
                entry_count: None,
                modified_at: None,
                created_at: None,
                sha256: None,
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect path metadata {}", path.display()));
        }
    };

    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        PathKind::Symlink
    } else if metadata.is_file() {
        PathKind::File
    } else if metadata.is_dir() {
        PathKind::Directory
    } else {
        PathKind::Other
    };
    let modified_at = metadata
        .modified()
        .map(DateTime::<Utc>::from)
        .with_context(|| format!("read modified timestamp for {}", path.display()))?;
    let created_at = metadata.created().ok().map(DateTime::<Utc>::from);

    let (size_bytes, empty, entry_count) = match kind {
        PathKind::File => (Some(metadata.len()), Some(metadata.len() == 0), None),
        PathKind::Directory => {
            let empty = fs::read_dir(&path)
                .with_context(|| format!("read directory {}", path.display()))?
                .next()
                .transpose()
                .with_context(|| format!("inspect directory entry in {}", path.display()))?
                .is_none();
            let size_is_required = action.recursive_size
                || action.expect.as_ref().is_some_and(|expectation| {
                    expectation.min_size_bytes.is_some() || expectation.max_size_bytes.is_some()
                });
            if size_is_required {
                let (size, count) = measure_directory_tree(&path)?;
                (Some(size), Some(empty), Some(count))
            } else {
                (None, Some(empty), None)
            }
        }
        PathKind::Symlink => (None, None, None),
        PathKind::Other => (Some(metadata.len()), None, None),
    };
    let sha256_is_required = action.sha256
        || action
            .expect
            .as_ref()
            .is_some_and(|expectation| expectation.sha256.is_some());
    let sha256 = if sha256_is_required {
        if kind != PathKind::File {
            bail!(
                "SHA-256 is available only for regular files: {}",
                path.display()
            );
        }
        Some(sha256_file(&path)?)
    } else {
        None
    };

    Ok(PathMetadataOutput {
        path,
        exists: true,
        kind: Some(kind),
        size_bytes,
        empty,
        entry_count,
        modified_at: Some(modified_at),
        created_at,
        sha256,
    })
}

fn measure_directory_tree(path: &Path) -> Result<(u64, u64)> {
    let mut size_bytes = 0u64;
    let mut entry_count = 0u64;
    for entry in WalkDir::new(path).follow_links(false).into_iter().skip(1) {
        let entry = entry.with_context(|| format!("walk directory {}", path.display()))?;
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| anyhow!("directory entry count overflow for {}", path.display()))?;
        if entry.file_type().is_file() {
            let length = entry
                .metadata()
                .with_context(|| format!("read file metadata {}", entry.path().display()))?
                .len();
            size_bytes = size_bytes.checked_add(length).ok_or_else(|| {
                anyhow!("directory size overflow while reading {}", path.display())
            })?;
        }
    }
    Ok((size_bytes, entry_count))
}

fn verify_path_expectation(
    expectation: &PathExpectation,
    metadata: &PathMetadataOutput,
) -> Result<()> {
    if let Some(expected) = expectation.exists {
        if metadata.exists != expected {
            bail!(
                "expected exists to be {}, observed {} for {}",
                expected,
                metadata.exists,
                metadata.path.display()
            );
        }
    }
    if !metadata.exists {
        if expectation.kind.is_some()
            || expectation.empty.is_some()
            || expectation.min_size_bytes.is_some()
            || expectation.max_size_bytes.is_some()
            || expectation.modified_at_or_after.is_some()
            || expectation.modified_at_or_before.is_some()
            || expectation.sha256.is_some()
        {
            bail!(
                "path does not exist, so metadata assertions cannot be evaluated: {}",
                metadata.path.display()
            );
        }
        return Ok(());
    }

    if let Some(expected) = expectation.kind {
        let observed = metadata
            .kind
            .ok_or_else(|| anyhow!("path type is unavailable for {}", metadata.path.display()))?;
        if observed != expected {
            bail!(
                "expected kind {}, observed {} for {}",
                path_kind_name(expected),
                path_kind_name(observed),
                metadata.path.display()
            );
        }
    }
    if let Some(expected) = expectation.empty {
        let observed = metadata
            .empty
            .ok_or_else(|| anyhow!("emptiness is unavailable for {}", metadata.path.display()))?;
        if observed != expected {
            bail!(
                "expected empty to be {}, observed {} for {}",
                expected,
                observed,
                metadata.path.display()
            );
        }
    }
    if let Some(minimum) = expectation.min_size_bytes {
        let observed = metadata
            .size_bytes
            .ok_or_else(|| anyhow!("size is unavailable for {}", metadata.path.display()))?;
        if observed < minimum {
            bail!(
                "expected size at least {} bytes, observed {} bytes for {}",
                minimum,
                observed,
                metadata.path.display()
            );
        }
    }
    if let Some(maximum) = expectation.max_size_bytes {
        let observed = metadata
            .size_bytes
            .ok_or_else(|| anyhow!("size is unavailable for {}", metadata.path.display()))?;
        if observed > maximum {
            bail!(
                "expected size at most {} bytes, observed {} bytes for {}",
                maximum,
                observed,
                metadata.path.display()
            );
        }
    }
    if let Some(after) = expectation.modified_at_or_after.as_ref() {
        let observed = metadata.modified_at.as_ref().ok_or_else(|| {
            anyhow!(
                "modified timestamp is unavailable for {}",
                metadata.path.display()
            )
        })?;
        if observed < after {
            bail!(
                "expected modified_at at or after {}, observed {} for {}",
                format_timestamp(after),
                format_timestamp(observed),
                metadata.path.display()
            );
        }
    }
    if let Some(before) = expectation.modified_at_or_before.as_ref() {
        let observed = metadata.modified_at.as_ref().ok_or_else(|| {
            anyhow!(
                "modified timestamp is unavailable for {}",
                metadata.path.display()
            )
        })?;
        if observed > before {
            bail!(
                "expected modified_at at or before {}, observed {} for {}",
                format_timestamp(before),
                format_timestamp(observed),
                metadata.path.display()
            );
        }
    }
    if let Some(expected) = expectation.sha256.as_ref() {
        let observed = metadata
            .sha256
            .as_ref()
            .ok_or_else(|| anyhow!("SHA-256 is unavailable for {}", metadata.path.display()))?;
        if !observed.eq_ignore_ascii_case(expected) {
            bail!(
                "expected SHA-256 {}, observed {} for {}",
                expected,
                observed,
                metadata.path.display()
            );
        }
    }
    Ok(())
}

fn summarize_path_metadata(metadata: &PathMetadataOutput) -> String {
    if !metadata.exists {
        return format!("path does not exist: {}", metadata.path.display());
    }
    let mut fields = vec![
        format!("path exists: {}", metadata.path.display()),
        format!(
            "kind: {}",
            metadata.kind.map(path_kind_name).unwrap_or("unknown")
        ),
    ];
    if let Some(size) = metadata.size_bytes {
        fields.push(format!("size_bytes: {size}"));
    }
    if let Some(empty) = metadata.empty {
        fields.push(format!("empty: {empty}"));
    }
    if let Some(count) = metadata.entry_count {
        fields.push(format!("entries: {count}"));
    }
    if let Some(modified) = metadata.modified_at.as_ref() {
        fields.push(format!("modified_at: {}", format_timestamp(modified)));
    }
    if let Some(created) = metadata.created_at.as_ref() {
        fields.push(format!("created_at: {}", format_timestamp(created)));
    }
    if let Some(sha256) = metadata.sha256.as_ref() {
        fields.push(format!("sha256: {sha256}"));
    }
    fields.join("; ")
}

fn describe_path_expectation(expectation: &PathExpectation) -> String {
    let mut requirements = Vec::new();
    if let Some(exists) = expectation.exists {
        requirements.push(format!("exists = {exists}"));
    }
    if let Some(kind) = expectation.kind {
        requirements.push(format!("kind = {}", path_kind_name(kind)));
    }
    if let Some(empty) = expectation.empty {
        requirements.push(format!("empty = {empty}"));
    }
    if let Some(minimum) = expectation.min_size_bytes {
        requirements.push(format!("size >= {minimum} bytes"));
    }
    if let Some(maximum) = expectation.max_size_bytes {
        requirements.push(format!("size <= {maximum} bytes"));
    }
    if let Some(after) = expectation.modified_at_or_after.as_ref() {
        requirements.push(format!("modified_at >= {}", format_timestamp(after)));
    }
    if let Some(before) = expectation.modified_at_or_before.as_ref() {
        requirements.push(format!("modified_at <= {}", format_timestamp(before)));
    }
    if let Some(sha256) = expectation.sha256.as_ref() {
        requirements.push(format!("sha256 = {sha256}"));
    }
    if requirements.is_empty() {
        String::new()
    } else {
        format!("; require all of: {}", requirements.join(", "))
    }
}

fn path_kind_name(kind: PathKind) -> &'static str {
    match kind {
        PathKind::File => "file",
        PathKind::Directory => "directory",
        PathKind::Symlink => "symlink",
        PathKind::Other => "other",
    }
}

fn format_timestamp(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn finish_git_action(
    dest: &str,
    approval_context: GitDestinationApprovalContext<'_>,
    result: Result<ApplyStepResult>,
) -> Result<ApplyStepResult> {
    let result = result?;
    let dest_path = expand_required_path(dest)?;
    revalidate_git_destination_after_action(&dest_path, approval_context)?;
    Ok(result)
}

fn apply_git_clone_or_update(
    repo: &str,
    dest: &str,
    branch: Option<&str>,
    approval_context: GitDestinationApprovalContext<'_>,
) -> Result<ApplyStepResult> {
    let dest_path = expand_required_path(dest)?;
    let validated = validate_git_destination_with_approval(&dest_path, approval_context)?;
    if dest_path.exists() && fs::symlink_metadata(&dest_path)?.file_type().is_symlink() {
        bail!(
            "git destination must not be a symlink: {}",
            dest_path.display()
        );
    }

    let destination_is_empty = dest_path.is_dir()
        && fs::read_dir(&dest_path)
            .with_context(|| format!("inspect git destination {}", dest_path.display()))?
            .next()
            .is_none();
    if !dest_path.exists() || destination_is_empty {
        return clone_git_repository(repo, &dest_path, branch, approval_context);
    }

    validate_existing_git_repository(&dest_path, repo, validated.protected)?;
    let active_branch = current_git_branch(&dest_path)?;
    let target_branch = match branch {
        Some(branch) => branch.to_owned(),
        None => active_branch.clone().ok_or_else(|| {
            anyhow!(
                "git destination {} has a detached HEAD; set an explicit branch for synchronization",
                dest_path.display()
            )
        })?,
    };
    validate_git_branch_name(&target_branch)?;

    let remote_ref = format!("refs/remotes/origin/{target_branch}");
    let local_ref = format!("refs/heads/{target_branch}");
    let refspec = format!("+refs/heads/{target_branch}:{remote_ref}");
    git_stdout(
        &dest_path,
        &[
            "fetch",
            "--no-tags",
            "--recurse-submodules=no",
            "origin",
            &refspec,
        ],
        &format!("fetch origin/{target_branch}"),
    )?;

    let remote_sha = git_ref_sha(&dest_path, &remote_ref)?.ok_or_else(|| {
        anyhow!(
            "origin does not provide branch {} for repository at {}",
            target_branch,
            dest_path.display()
        )
    })?;
    let Some(local_sha) = git_ref_sha(&dest_path, &local_ref)? else {
        if active_branch.as_deref() == Some(target_branch.as_str()) {
            bail!(
                "local branch {} has no commit at {}; refusing to overwrite its working tree",
                target_branch,
                dest_path.display()
            );
        }
        update_inactive_git_branch(&dest_path, &local_ref, &remote_ref, &target_branch)?;
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), &target_branch)?;
        return Ok(ApplyStepResult::Applied(format!(
            "repository already existed at {}; local {} was missing and was created at {}{}",
            dest_path.display(),
            target_branch,
            short_git_sha(&remote_sha),
            preservation_note
        )));
    };

    if local_sha == remote_sha {
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), &target_branch)?;
        return Ok(ApplyStepResult::AlreadySatisfied(format!(
            "repository already existed at {}; {} branch ref was already up to date at {}{}",
            dest_path.display(),
            target_branch,
            short_git_sha(&local_sha),
            preservation_note
        )));
    }

    if git_is_ancestor(&dest_path, &local_ref, &remote_ref)? {
        let behind = git_commit_count(&dest_path, &format!("{local_ref}..{remote_ref}"))?;
        if active_branch.as_deref() == Some(target_branch.as_str()) {
            if let Some(blocker) = git_fast_forward_blocker(&dest_path, &local_ref, &remote_ref)? {
                bail!(
                    "repository already existed at {}; {} was outdated by {} commit(s), but {}; fetched origin/{} and left local files unchanged",
                    dest_path.display(),
                    target_branch,
                    behind,
                    blocker,
                    target_branch
                );
            }
            git_stdout(
                &dest_path,
                &["merge", "--ff-only", &remote_ref],
                &format!("fast-forward {target_branch}"),
            )?;
        } else {
            update_inactive_git_branch(&dest_path, &local_ref, &remote_ref, &target_branch)?;
        }
        let updated_sha = git_ref_sha(&dest_path, &local_ref)?.ok_or_else(|| {
            anyhow!(
                "local branch {} disappeared while updating {}",
                target_branch,
                dest_path.display()
            )
        })?;
        if updated_sha != remote_sha {
            bail!(
                "local branch {} changed concurrently while updating {}",
                target_branch,
                dest_path.display()
            );
        }
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), &target_branch)?;
        return Ok(ApplyStepResult::Applied(format!(
            "repository already existed at {}; {} was outdated by {} commit(s) and was updated {} -> {}{}",
            dest_path.display(),
            target_branch,
            behind,
            short_git_sha(&local_sha),
            short_git_sha(&updated_sha),
            preservation_note
        )));
    }

    if git_is_ancestor(&dest_path, &remote_ref, &local_ref)? {
        let ahead = git_commit_count(&dest_path, &format!("{remote_ref}..{local_ref}"))?;
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), &target_branch)?;
        return Ok(ApplyStepResult::AlreadySatisfied(format!(
            "repository already existed at {}; {} already contains origin/{} and is ahead by {} local commit(s); left unchanged at {}{}",
            dest_path.display(),
            target_branch,
            target_branch,
            ahead,
            short_git_sha(&local_sha),
            preservation_note
        )));
    }

    let ahead = git_commit_count(&dest_path, &format!("{remote_ref}..{local_ref}"))?;
    let behind = git_commit_count(&dest_path, &format!("{local_ref}..{remote_ref}"))?;
    bail!(
        "repository already existed at {}; {} diverged from origin/{} (ahead {}, behind {}); fetched the remote branch but left local history unchanged",
        dest_path.display(),
        target_branch,
        target_branch,
        ahead,
        behind
    )
}

fn apply_git_inspect(repo: &str, dest: &str) -> Result<ApplyStepResult> {
    let dest_path = validate_git_inspection_path(dest)?;
    if !dest_path.exists() {
        return Ok(ApplyStepResult::AlreadySatisfiedWithOutput {
            summary: format!("repository is absent at {}", dest_path.display()),
            output: git_inspection_output(repo, dest, None, false),
        });
    }
    if fs::symlink_metadata(&dest_path)?.file_type().is_symlink() {
        bail!(
            "git destination must not be a symlink: {}",
            dest_path.display()
        );
    }
    if dest_path.is_dir()
        && fs::read_dir(&dest_path)
            .with_context(|| format!("inspect git destination {}", dest_path.display()))?
            .next()
            .is_none()
    {
        return Ok(ApplyStepResult::AlreadySatisfiedWithOutput {
            summary: format!(
                "repository is absent; destination is an empty directory at {}",
                dest_path.display()
            ),
            output: git_inspection_output(repo, dest, None, false),
        });
    }
    validate_existing_git_repository(&dest_path, repo, false)?;
    let active_branch = current_git_branch(&dest_path)?;
    let active_checkout = active_branch.as_deref().unwrap_or("detached HEAD");
    Ok(ApplyStepResult::AlreadySatisfiedWithOutput {
        summary: format!(
            "expected repository exists at {}; active checkout: {}",
            dest_path.display(),
            active_checkout
        ),
        output: git_inspection_output(repo, dest, active_branch.as_deref(), true),
    })
}

fn apply_git_clone_if_missing(
    repo: &str,
    dest: &str,
    branch: Option<&str>,
    approval_context: GitDestinationApprovalContext<'_>,
) -> Result<ApplyStepResult> {
    let dest_path = expand_required_path(dest)?;
    let validated = validate_git_destination_with_approval(&dest_path, approval_context)?;
    if dest_path.exists() && fs::symlink_metadata(&dest_path)?.file_type().is_symlink() {
        bail!(
            "git destination must not be a symlink: {}",
            dest_path.display()
        );
    }
    let destination_is_empty = dest_path.is_dir()
        && fs::read_dir(&dest_path)
            .with_context(|| format!("inspect git destination {}", dest_path.display()))?
            .next()
            .is_none();
    if !dest_path.exists() || destination_is_empty {
        return clone_git_repository(repo, &dest_path, branch, approval_context);
    }
    validate_existing_git_repository(&dest_path, repo, validated.protected)?;
    Ok(ApplyStepResult::AlreadySatisfied(format!(
        "clone not needed; expected repository already exists at {}",
        dest_path.display()
    )))
}

fn apply_git_fetch(
    repo: &str,
    dest: &str,
    branch: &str,
    approval_context: GitDestinationApprovalContext<'_>,
) -> Result<ApplyStepResult> {
    validate_git_branch_name(branch)?;
    let dest_path = expand_required_path(dest)?;
    let validated = validate_git_destination_with_approval(&dest_path, approval_context)?;
    validate_existing_git_repository(&dest_path, repo, validated.protected)?;
    let remote_ref = format!("refs/remotes/origin/{branch}");
    let before = git_ref_sha(&dest_path, &remote_ref)?;
    let refspec = format!("+refs/heads/{branch}:{remote_ref}");
    git_stdout(
        &dest_path,
        &[
            "fetch",
            "--no-tags",
            "--recurse-submodules=no",
            "origin",
            &refspec,
        ],
        &format!("fetch origin/{branch}"),
    )?;
    let after = git_ref_sha(&dest_path, &remote_ref)?.ok_or_else(|| {
        anyhow!(
            "origin does not provide branch {} for repository at {}",
            branch,
            dest_path.display()
        )
    })?;
    if before.as_deref() == Some(after.as_str()) {
        Ok(ApplyStepResult::AlreadySatisfied(format!(
            "fetched origin/{}; remote ref was already current at {}",
            branch,
            short_git_sha(&after)
        )))
    } else {
        Ok(ApplyStepResult::Applied(format!(
            "fetched origin/{}; remote ref {} -> {}",
            branch,
            before.as_deref().map(short_git_sha).unwrap_or("absent"),
            short_git_sha(&after)
        )))
    }
}

fn apply_git_fast_forward(
    repo: &str,
    dest: &str,
    branch: &str,
    approval_context: GitDestinationApprovalContext<'_>,
) -> Result<ApplyStepResult> {
    validate_git_branch_name(branch)?;
    let dest_path = expand_required_path(dest)?;
    let validated = validate_git_destination_with_approval(&dest_path, approval_context)?;
    validate_existing_git_repository(&dest_path, repo, validated.protected)?;
    let active_branch = current_git_branch(&dest_path)?;
    let remote_ref = format!("refs/remotes/origin/{branch}");
    let local_ref = format!("refs/heads/{branch}");
    let remote_sha = git_ref_sha(&dest_path, &remote_ref)?.ok_or_else(|| {
        anyhow!(
            "origin/{} has not been fetched for repository at {}",
            branch,
            dest_path.display()
        )
    })?;
    let Some(local_sha) = git_ref_sha(&dest_path, &local_ref)? else {
        if active_branch.as_deref() == Some(branch) {
            bail!(
                "local branch {} has no commit at {}; refusing to overwrite its working tree",
                branch,
                dest_path.display()
            );
        }
        update_inactive_git_branch(&dest_path, &local_ref, &remote_ref, branch)?;
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), branch)?;
        return Ok(ApplyStepResult::Applied(format!(
            "local {} was missing and was created at {}{}",
            branch,
            short_git_sha(&remote_sha),
            preservation_note
        )));
    };

    if local_sha == remote_sha {
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), branch)?;
        return Ok(ApplyStepResult::AlreadySatisfied(format!(
            "{} branch ref was already up to date at {}{}",
            branch,
            short_git_sha(&local_sha),
            preservation_note
        )));
    }
    if git_is_ancestor(&dest_path, &local_ref, &remote_ref)? {
        let behind = git_commit_count(&dest_path, &format!("{local_ref}..{remote_ref}"))?;
        if active_branch.as_deref() == Some(branch) {
            if let Some(blocker) = git_fast_forward_blocker(&dest_path, &local_ref, &remote_ref)? {
                bail!(
                    "{} was outdated by {} commit(s), but {}; left local files unchanged",
                    branch,
                    behind,
                    blocker
                );
            }
            git_stdout(
                &dest_path,
                &["merge", "--ff-only", &remote_ref],
                &format!("fast-forward {branch}"),
            )?;
        } else {
            update_inactive_git_branch(&dest_path, &local_ref, &remote_ref, branch)?;
        }
        let updated_sha = git_ref_sha(&dest_path, &local_ref)?.ok_or_else(|| {
            anyhow!(
                "local branch {} disappeared while updating {}",
                branch,
                dest_path.display()
            )
        })?;
        if updated_sha != remote_sha {
            bail!(
                "local branch {} changed concurrently while updating {}",
                branch,
                dest_path.display()
            );
        }
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), branch)?;
        return Ok(ApplyStepResult::Applied(format!(
            "{} was outdated by {} commit(s) and was updated {} -> {}{}",
            branch,
            behind,
            short_git_sha(&local_sha),
            short_git_sha(&updated_sha),
            preservation_note
        )));
    }
    if git_is_ancestor(&dest_path, &remote_ref, &local_ref)? {
        let ahead = git_commit_count(&dest_path, &format!("{remote_ref}..{local_ref}"))?;
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), branch)?;
        return Ok(ApplyStepResult::AlreadySatisfied(format!(
            "{} already contains origin/{} and is ahead by {} local commit(s); left unchanged at {}{}",
            branch,
            branch,
            ahead,
            short_git_sha(&local_sha),
            preservation_note
        )));
    }
    let ahead = git_commit_count(&dest_path, &format!("{remote_ref}..{local_ref}"))?;
    let behind = git_commit_count(&dest_path, &format!("{local_ref}..{remote_ref}"))?;
    bail!(
        "{} diverged from origin/{} (ahead {}, behind {}); left local history unchanged",
        branch,
        branch,
        ahead,
        behind
    )
}

fn git_fast_forward_blocker(
    dest_path: &Path,
    local_ref: &str,
    remote_ref: &str,
) -> Result<Option<String>> {
    let tracked_status = git_stdout(
        dest_path,
        &["status", "--porcelain=v1", "--untracked-files=no"],
        "inspect tracked git working tree state",
    )?;
    if !tracked_status.is_empty() {
        return Ok(Some(
            "the working tree has tracked or staged local changes".into(),
        ));
    }

    let incoming_paths = git_nul_paths(
        dest_path,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--no-ext-diff",
            "-z",
            local_ref,
            remote_ref,
        ],
        "inspect incoming git paths",
    )?;
    let mut local_paths = git_nul_paths(
        dest_path,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        "inspect untracked git paths",
    )?;
    local_paths.extend(git_nul_paths(
        dest_path,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
        "inspect ignored git paths",
    )?);
    if let Some(local_path) = local_paths.iter().find(|local_path| {
        incoming_paths.iter().any(|incoming_path| {
            local_path == &incoming_path
                || local_path.starts_with(incoming_path)
                || incoming_path.starts_with(local_path)
        })
    }) {
        return Ok(Some(format!(
            "untracked or ignored path {} conflicts with the incoming fast-forward",
            local_path.display()
        )));
    }
    Ok(None)
}

fn git_nul_paths(dest_path: &Path, args: &[&str], action: &str) -> Result<Vec<PathBuf>> {
    let output = git_output(dest_path, args)?;
    if !output.status.success() {
        ensure_git_output_succeeded(output, action)?;
        unreachable!();
    }
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("{action} returned a non-UTF-8 path; refusing the update"))?;
    Ok(stdout
        .split_terminator('\0')
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn checkout_preservation_note(
    dest_path: &Path,
    active_branch: Option<&str>,
    target_branch: &str,
) -> Result<String> {
    match active_branch {
        Some(active) if active != target_branch => {
            Ok(format!("; active branch {active} was preserved"))
        }
        None => Ok("; detached checkout was preserved".into()),
        Some(_) => {
            let status = git_stdout(
                dest_path,
                &["status", "--porcelain=v1", "--untracked-files=normal"],
                "inspect git working tree",
            )?;
            Ok(if status.is_empty() {
                String::new()
            } else {
                "; working tree has local changes that were left untouched".into()
            })
        }
    }
}

fn update_inactive_git_branch(
    dest_path: &Path,
    local_ref: &str,
    remote_ref: &str,
    branch: &str,
) -> Result<()> {
    let refspec = format!("{remote_ref}:{local_ref}");
    git_stdout(
        dest_path,
        &[
            "fetch",
            "--no-tags",
            "--recurse-submodules=no",
            ".",
            &refspec,
        ],
        &format!("fast-forward inactive local branch {branch}"),
    )?;
    Ok(())
}

fn clone_git_repository(
    repo: &str,
    dest_path: &Path,
    branch: Option<&str>,
    approval_context: GitDestinationApprovalContext<'_>,
) -> Result<ApplyStepResult> {
    if let Some(branch) = branch {
        validate_git_branch_name(branch)?;
    }
    let validated_before = validate_git_destination_with_approval(dest_path, approval_context)?;
    if validated_before.protected {
        protected_destination_parent_is_real(dest_path)?;
    } else if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create clone parent {}", parent.display()))?;
    }
    let validated_after = validate_git_destination_with_approval(dest_path, approval_context)?;
    if validated_after.resolved_path != validated_before.resolved_path
        || validated_after.protected != validated_before.protected
    {
        bail!(
            "git destination changed while preparing clone: {}",
            dest_path.display()
        );
    }
    let mut command = Command::new("git");
    prepare_git_command(&mut command);
    command.arg("clone").arg("--no-recurse-submodules");
    if let Some(branch) = branch {
        command.arg("--branch").arg(branch);
    }
    command.arg(repo).arg(dest_path);
    let output = command
        .output()
        .with_context(|| format!("clone repository into {}", dest_path.display()))?;
    ensure_git_output_succeeded(output, "git clone")?;

    if validated_before.protected {
        revalidate_git_destination_after_action(dest_path, approval_context)?;
        validate_existing_git_repository(dest_path, repo, true).with_context(|| {
            format!(
                "verify protected repository layout after clone at {}",
                dest_path.display()
            )
        })?;
    }

    let cloned_branch = match branch {
        Some(branch) => branch.to_owned(),
        None => current_git_branch(dest_path)?.ok_or_else(|| {
            anyhow!(
                "cloned repository at {} has a detached HEAD",
                dest_path.display()
            )
        })?,
    };
    let sha = git_ref_sha(dest_path, &format!("refs/heads/{cloned_branch}"))?.ok_or_else(|| {
        anyhow!(
            "cloned repository at {} has no local branch {}",
            dest_path.display(),
            cloned_branch
        )
    })?;
    Ok(ApplyStepResult::Applied(format!(
        "repository was absent; cloned into {} with {} at {}",
        dest_path.display(),
        cloned_branch,
        short_git_sha(&sha)
    )))
}

fn validate_existing_git_repository(
    dest_path: &Path,
    expected_repo: &str,
    require_in_tree_git_dir: bool,
) -> Result<()> {
    if !dest_path.is_dir() {
        bail!(
            "git destination exists but is not a directory: {}",
            dest_path.display()
        );
    }
    let top_level = git_stdout(
        dest_path,
        &["rev-parse", "--show-toplevel"],
        "inspect git repository",
    )?;
    let canonical_top = Path::new(&top_level)
        .canonicalize()
        .with_context(|| format!("canonicalize git top-level {top_level}"))?;
    let canonical_dest = dest_path
        .canonicalize()
        .with_context(|| format!("canonicalize git destination {}", dest_path.display()))?;
    if canonical_top != canonical_dest {
        bail!(
            "destination {} is not a git repository root",
            dest_path.display()
        );
    }
    let bare = git_stdout(
        dest_path,
        &["rev-parse", "--is-bare-repository"],
        "inspect git repository type",
    )?;
    if bare != "false" {
        bail!(
            "destination {} is not a non-bare git working tree",
            dest_path.display()
        );
    }
    if require_in_tree_git_dir {
        validate_protected_git_repository_layout(dest_path, &canonical_dest)?;
    }
    let origin = git_stdout(
        dest_path,
        &["remote", "get-url", "origin"],
        "read git origin URL",
    )?;
    if normalize_git_remote(&origin) != normalize_git_remote(expected_repo) {
        bail!(
            "repository at {} has an origin that does not match the requested repository; left it unchanged",
            dest_path.display()
        );
    }
    Ok(())
}

fn validate_protected_git_repository_layout(dest_path: &Path, canonical_dest: &Path) -> Result<()> {
    let dot_git = dest_path.join(".git");
    let dot_git_metadata = fs::symlink_metadata(&dot_git).with_context(|| {
        format!(
            "protected repository requires a real in-tree .git directory: {}",
            dot_git.display()
        )
    })?;
    if dot_git_metadata.file_type().is_symlink() || !dot_git_metadata.is_dir() {
        bail!(
            "protected repository requires a real in-tree .git directory: {}",
            dot_git.display()
        );
    }
    let canonical_dot_git = dot_git
        .canonicalize()
        .with_context(|| format!("canonicalize protected git directory {}", dot_git.display()))?;
    for entry in WalkDir::new(&dot_git).follow_links(false) {
        let entry = entry.with_context(|| {
            format!("inspect protected git metadata tree {}", dot_git.display())
        })?;
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            bail!(
                "protected repository git metadata must not contain symlinks: {}",
                entry.path().display()
            );
        }
        if !file_type.is_file() && !file_type.is_dir() {
            bail!(
                "protected repository git metadata contains a special filesystem entry: {}",
                entry.path().display()
            );
        }
    }
    validate_protected_git_execution_config(dest_path)?;
    let absolute_git_dir = git_stdout(
        dest_path,
        &["rev-parse", "--absolute-git-dir"],
        "inspect protected git directory",
    )?;
    let common_git_dir = git_stdout(
        dest_path,
        &["rev-parse", "--git-common-dir"],
        "inspect protected common git directory",
    )?;
    let canonical_git_dir = canonicalize_git_reported_path(dest_path, &absolute_git_dir)
        .context("canonicalize protected absolute git directory")?;
    let canonical_common_dir = canonicalize_git_reported_path(dest_path, &common_git_dir)
        .context("canonicalize protected common git directory")?;
    if canonical_git_dir != canonical_dot_git
        || !canonical_git_dir.starts_with(canonical_dest)
        || !canonical_common_dir.starts_with(canonical_dest)
    {
        bail!(
            "protected repository at {} uses a linked or external git directory",
            dest_path.display()
        );
    }
    Ok(())
}

fn canonicalize_git_reported_path(work_tree: &Path, reported: &str) -> Result<PathBuf> {
    let path = Path::new(reported.trim());
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        work_tree.join(path)
    };
    absolute
        .canonicalize()
        .with_context(|| format!("canonicalize git path {}", absolute.display()))
}

fn validate_protected_git_execution_config(cwd: &Path) -> Result<()> {
    for scope in ["--local", "--worktree"] {
        let output = git_output(
            cwd,
            &[
                "config",
                scope,
                "--includes",
                "--null",
                "--name-only",
                "--list",
            ],
        )?;
        if !output.status.success() {
            ensure_git_output_succeeded(output, "inspect protected git configuration")?;
            unreachable!();
        }
        let keys = String::from_utf8(output.stdout)
            .context("protected git configuration contains a non-UTF-8 key")?;
        if let Some(key) = keys
            .split_terminator('\0')
            .find(|key| git_config_key_can_execute_process(key))
        {
            bail!(
                "protected repository has executable Git configuration {}; remove it before granting access",
                key
            );
        }
    }
    Ok(())
}

fn git_config_key_can_execute_process(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if matches!(
        key.as_str(),
        "core.askpass" | "core.fsmonitor" | "core.gitproxy" | "core.sshcommand"
    ) {
        return true;
    }
    let segments = key.split('.').collect::<Vec<_>>();
    matches!(
        segments.as_slice(),
        ["credential", .., "helper"]
            | ["filter", .., "clean" | "smudge" | "process"]
            | ["remote", .., "uploadpack"]
    )
}

fn validate_git_branch_name(branch: &str) -> Result<()> {
    if branch.trim().is_empty() {
        bail!("git branch must not be empty");
    }
    let mut command = Command::new("git");
    prepare_git_command(&mut command);
    let output = command
        .args(["check-ref-format", "--branch", branch])
        .output()
        .context("validate git branch")?;
    ensure_git_output_succeeded(output, "validate git branch")?;
    Ok(())
}

fn normalize_git_remote(remote: &str) -> String {
    github_git_remote_identity(remote)
        .unwrap_or_else(|| remote.trim().trim_end_matches('/').to_owned())
}

fn github_git_remote_identity(remote: &str) -> Option<String> {
    let remote = remote.trim();
    let lowercase = remote.to_ascii_lowercase();
    let marker = "github.com";
    let marker_start = lowercase.find(marker)?;
    if marker_start > 0 && !matches!(lowercase.as_bytes()[marker_start - 1], b'/' | b'@' | b':') {
        return None;
    }
    let mut remainder = remote.get(marker_start + marker.len()..)?;
    if let Some(after_colon) = remainder.strip_prefix(':') {
        if let Some((possible_port, path)) = after_colon.split_once('/') {
            remainder = if possible_port.chars().all(|ch| ch.is_ascii_digit()) {
                path
            } else {
                after_colon
            };
        } else {
            remainder = after_colon;
        }
    } else {
        remainder = remainder.strip_prefix('/')?;
    }
    let path = remainder.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut components = path.split('/');
    let owner = components.next()?;
    let repository = components.next()?;
    if owner.is_empty() || repository.is_empty() || components.next().is_some() {
        return None;
    }
    Some(format!(
        "github.com/{}/{}",
        owner.to_ascii_lowercase(),
        repository.to_ascii_lowercase()
    ))
}

fn current_git_branch(dest_path: &Path) -> Result<Option<String>> {
    let output = git_output(dest_path, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    ensure_git_output_succeeded(output, "read current git branch")?;
    unreachable!()
}

fn git_ref_sha(dest_path: &Path, reference: &str) -> Result<Option<String>> {
    let output = git_output(dest_path, &["rev-parse", "--verify", reference])?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ));
    }
    Ok(None)
}

fn git_is_ancestor(dest_path: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = git_output(
        dest_path,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            ensure_git_output_succeeded(output, "compare git history")?;
            unreachable!()
        }
    }
}

fn git_commit_count(dest_path: &Path, range: &str) -> Result<u64> {
    git_stdout(
        dest_path,
        &["rev-list", "--count", range],
        "count git commits",
    )?
    .parse()
    .context("parse git commit count")
}

fn git_stdout(dest_path: &Path, args: &[&str], action: &str) -> Result<String> {
    let output = git_output(dest_path, args)?;
    ensure_git_output_succeeded(output, action)
}

fn git_output(dest_path: &Path, args: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new("git");
    prepare_git_command(&mut command);
    command
        .arg("-C")
        .arg(dest_path)
        .args(args)
        .output()
        .with_context(|| format!("run git in {}", dest_path.display()))
}

fn prepare_git_command(command: &mut Command) {
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("submodule.recurse=false")
        .arg("-c")
        .arg("fetch.recurseSubmodules=false")
        .arg("-c")
        .arg("protocol.ext.allow=never");
    for (key, _) in std::env::vars_os() {
        if git_environment_key_can_redirect_operation(&key.to_string_lossy()) {
            command.env_remove(key);
        }
    }
}

fn git_environment_key_can_redirect_operation(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    matches!(
        key.as_str(),
        "GIT_DIR"
            | "GIT_WORK_TREE"
            | "GIT_COMMON_DIR"
            | "GIT_OBJECT_DIRECTORY"
            | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
            | "GIT_INDEX_FILE"
            | "GIT_NAMESPACE"
            | "GIT_SHALLOW_FILE"
            | "GIT_GRAFT_FILE"
            | "GIT_QUARANTINE_PATH"
    )
}

fn ensure_git_output_succeeded(output: std::process::Output, action: &str) -> Result<String> {
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        output.status.exit_ok(action)?;
    }
    bail!("{} failed: {}", action, detail)
}

fn short_git_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn apply_brew_install(package: &str, cask: bool) -> Result<String> {
    let already_installed = if cask {
        Command::new("brew")
            .args(["list", "--cask", package])
            .status()
            .with_context(|| format!("check brew cask {}", package))?
            .success()
    } else {
        Command::new("brew")
            .args(["list", package])
            .status()
            .with_context(|| format!("check brew package {}", package))?
            .success()
    };
    if already_installed {
        return Ok(format!("brew package already installed: {}", package));
    }
    let mut command = Command::new("brew");
    command.arg("install");
    if cask {
        command.arg("--cask");
    }
    command.arg(package);
    command
        .status()
        .with_context(|| format!("install brew package {}", package))?
        .exit_ok("brew install")?;
    Ok(format!("installed brew package {}", package))
}

fn apply_download_file(
    url: &str,
    dest: &str,
    checksum: &crate::automation::task::Checksum,
) -> Result<String> {
    let dest_path = expand_required_path(dest)?;
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create download parent {}", parent.display()))?;
    }
    let (temp_path, temp_file) = create_unique_temp_file(&dest_path)?;
    let curl_program = if cfg!(target_os = "macos") {
        "/usr/bin/curl"
    } else {
        "curl"
    };
    let status = match Command::new(curl_program)
        .args(["-L", "--fail", "--silent", "--show-error", url])
        .stdout(Stdio::from(temp_file))
        .status()
    {
        Ok(status) => status,
        Err(err) => {
            fs::remove_file(&temp_path).ok();
            return Err(err).with_context(|| format!("download {}", url));
        }
    };
    if !status.success() {
        fs::remove_file(&temp_path).ok();
        status.exit_ok("curl download")?;
    }
    if !checksum.sha256.trim().is_empty() {
        let expected = checksum.sha256.trim();
        let actual = sha256_file(&temp_path)?;
        if actual != expected {
            fs::remove_file(&temp_path).ok();
            bail!(
                "checksum mismatch for {}: expected {}, got {}",
                url,
                expected,
                actual
            );
        }
    }
    if path_entry_exists(&dest_path)? {
        fs::remove_file(&dest_path)
            .with_context(|| format!("replace existing destination {}", dest_path.display()))?;
    }
    fs::rename(&temp_path, &dest_path)
        .with_context(|| format!("move downloaded file to {}", dest_path.display()))?;
    Ok(format!("downloaded {} to {}", url, dest_path.display()))
}

const BAMBU_RELEASES_API: &str =
    "https://api.github.com/repos/bambulab/BambuStudio/releases?per_page=30";
const BAMBU_DOWNLOAD_PREFIX: &str = "https://github.com/bambulab/BambuStudio/releases/download/";

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    published_at: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

struct ResolvedBambuRelease {
    tag: String,
    version: String,
    asset_name: String,
    download_url: String,
    sha256: String,
}

fn release_channel_name(channel: ReleaseChannel) -> &'static str {
    match channel {
        ReleaseChannel::Release => "release",
        ReleaseChannel::Beta => "beta",
    }
}

fn fetch_bambu_releases() -> Result<Vec<GithubRelease>> {
    let curl_program = if cfg!(target_os = "macos") {
        "/usr/bin/curl"
    } else {
        "curl"
    };
    let output = Command::new(curl_program)
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-filesize",
            "5242880",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            BAMBU_RELEASES_API,
        ])
        .output()
        .context("fetch official Bambu Studio release metadata")?;
    if !output.status.success() {
        bail!(
            "fetch official Bambu Studio release metadata failed: {}",
            command_error_detail(&output)
        );
    }
    serde_json::from_slice(&output.stdout).context("parse Bambu Studio release metadata")
}

fn resolve_bambu_release(
    releases: &[GithubRelease],
    channel: ReleaseChannel,
) -> Result<ResolvedBambuRelease> {
    let release = releases
        .iter()
        .filter(|release| {
            !release.draft
                && match channel {
                    ReleaseChannel::Release => !release.prerelease,
                    ReleaseChannel::Beta => release.prerelease,
                }
        })
        .max_by(|left, right| left.published_at.cmp(&right.published_at))
        .ok_or_else(|| {
            anyhow!(
                "official Bambu Studio repository has no {} release",
                release_channel_name(channel)
            )
        })?;

    let mut assets = release.assets.iter().filter(|asset| {
        asset.name.starts_with("Bambu_Studio_mac-v")
            && asset.name.ends_with(".dmg")
            && !asset.name.contains("pre_release")
    });
    let asset = assets.next().ok_or_else(|| {
        anyhow!(
            "Bambu Studio {} has no unambiguous macOS DMG",
            release.tag_name
        )
    })?;
    if assets.next().is_some() {
        bail!(
            "Bambu Studio {} has multiple matching macOS DMGs; refusing an ambiguous download",
            release.tag_name
        );
    }
    if Path::new(&asset.name).components().count() != 1 {
        bail!("Bambu Studio release contains an unsafe asset name");
    }
    if !asset
        .browser_download_url
        .starts_with(BAMBU_DOWNLOAD_PREFIX)
    {
        bail!("Bambu Studio asset URL is outside the official GitHub repository");
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .ok_or_else(|| anyhow!("Bambu Studio macOS asset has no official SHA-256 digest"))?;
    if digest.len() != 64 || !digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
        bail!("Bambu Studio macOS asset has an invalid SHA-256 digest");
    }
    let version = asset
        .name
        .strip_prefix("Bambu_Studio_mac-v")
        .and_then(|rest| rest.split_once('-').map(|(version, _)| version))
        .ok_or_else(|| anyhow!("cannot derive Bambu Studio version from asset name"))?;
    parse_version(version)?;

    Ok(ResolvedBambuRelease {
        tag: release.tag_name.clone(),
        version: version.to_string(),
        asset_name: asset.name.clone(),
        download_url: asset.browser_download_url.clone(),
        sha256: digest.to_ascii_lowercase(),
    })
}

fn apply_bambu_studio_release(channel: ReleaseChannel) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("bambu-studio-release is only supported on macOS");
    }
    let resolved = resolve_bambu_release(&fetch_bambu_releases()?, channel)?;
    let install_root = expand_required_path("$HOME/Applications")?;
    let destination = install_root.join("BambuStudio.app");

    if path_entry_exists(&destination)? {
        let installed_version = verify_bambu_app_and_read_version(&destination)?;
        if compare_versions(&installed_version, &resolved.version)? != Ordering::Less {
            return Ok(format!(
                "Bambu Studio {} is already installed; latest {} is {} ({})",
                installed_version,
                release_channel_name(channel),
                resolved.version,
                resolved.tag
            ));
        }
        let running = Command::new("/usr/bin/pgrep")
            .args(["-x", "BambuStudio"])
            .status()
            .context("check whether Bambu Studio is running")?;
        if running.success() {
            bail!("Bambu Studio is running; quit it before updating");
        }
        if running.code() != Some(1) {
            bail!("could not determine whether Bambu Studio is running");
        }
    }

    let cache_path = dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory unavailable"))?
        .join("Library/Caches/ppduster/downloads")
        .join(&resolved.asset_name);
    let cache = cache_path.to_string_lossy().into_owned();
    apply_download_file(
        &resolved.download_url,
        &cache,
        &crate::automation::task::Checksum {
            sha256: resolved.sha256.clone(),
        },
    )?;
    let identity = AppBundleIdentity {
        bundle_identifier: "com.bambulab.bambu-studio".into(),
        team_identifier: "T3UBR9Y3B2".into(),
        version: resolved.version.clone(),
    };
    apply_install_dmg(
        &cache,
        Some("BambuStudio.app"),
        Some("$HOME/Applications"),
        Some(&identity),
        true,
    )?;
    Ok(format!(
        "installed Bambu Studio {} from latest {} ({})",
        resolved.version,
        release_channel_name(channel),
        resolved.tag
    ))
}

const MAX_ARCHIVE_ENTRIES: usize = 100_000;

fn archive_format_name(format: ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::Auto => "auto-detected",
        ArchiveFormat::Zip => "zip",
        ArchiveFormat::Tar => "tar",
        ArchiveFormat::TarGz => "tar.gz",
        ArchiveFormat::TarBz2 => "tar.bz2",
        ArchiveFormat::TarXz => "tar.xz",
    }
}

fn detect_archive_format(path: &Path, requested: ArchiveFormat) -> Result<ArchiveFormat> {
    if requested != ArchiveFormat::Auto {
        return Ok(requested);
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("archive file name is not valid UTF-8"))?
        .to_ascii_lowercase();
    if name.ends_with(".zip") {
        Ok(ArchiveFormat::Zip)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok(ArchiveFormat::TarGz)
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") || name.ends_with(".tbz") {
        Ok(ArchiveFormat::TarBz2)
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        Ok(ArchiveFormat::TarXz)
    } else if name.ends_with(".tar") {
        Ok(ArchiveFormat::Tar)
    } else {
        bail!(
            "cannot detect archive format from {}; set format explicitly",
            path.display()
        )
    }
}

fn apply_extract_archive(
    src: &str,
    dest: &str,
    requested_format: ArchiveFormat,
    max_unpacked_bytes: u64,
) -> Result<String> {
    let src_path = expand_required_path(src)?;
    let dest_path = expand_required_path(dest)?;
    let source_metadata = fs::symlink_metadata(&src_path)
        .with_context(|| format!("inspect archive source {}", src_path.display()))?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        bail!(
            "archive source is not a regular file: {}",
            src_path.display()
        );
    }
    if path_entry_exists(&dest_path)? {
        bail!(
            "archive destination already exists; refusing to merge or overwrite: {}",
            dest_path.display()
        );
    }
    let destination_parent = dest_path
        .parent()
        .ok_or_else(|| anyhow!("archive destination has no parent"))?;
    fs::create_dir_all(destination_parent).with_context(|| {
        format!(
            "create archive destination parent {}",
            destination_parent.display()
        )
    })?;
    require_real_directory(destination_parent)?;
    let format = detect_archive_format(&src_path, requested_format)?;
    let staging = create_archive_staging_directory(destination_parent)?;

    let extract_result = match format {
        ArchiveFormat::Zip => extract_zip_archive(&src_path, &staging, max_unpacked_bytes),
        ArchiveFormat::Tar
        | ArchiveFormat::TarGz
        | ArchiveFormat::TarBz2
        | ArchiveFormat::TarXz => {
            extract_tar_archive(&src_path, &staging, format, max_unpacked_bytes)
        }
        ArchiveFormat::Auto => unreachable!(),
    };
    if let Err(extract_err) = extract_result {
        if let Err(cleanup_err) = remove_archive_staging(destination_parent, &staging) {
            return Err(extract_err.context(format!(
                "also failed to remove archive staging directory: {cleanup_err:#}"
            )));
        }
        return Err(extract_err);
    }
    if let Err(commit_err) = fs::rename(&staging, &dest_path) {
        if let Err(cleanup_err) = remove_archive_staging(destination_parent, &staging) {
            return Err(commit_err).context(format!(
                "commit archive extraction failed and staging cleanup also failed: {cleanup_err:#}"
            ));
        }
        return Err(commit_err)
            .with_context(|| format!("commit extracted archive to {}", dest_path.display()));
    }
    Ok(format!(
        "extracted {} archive {} into {}",
        archive_format_name(format),
        src_path.display(),
        dest_path.display()
    ))
}

fn extract_zip_archive(src: &Path, staging: &Path, max_unpacked_bytes: u64) -> Result<()> {
    let file = File::open(src).with_context(|| format!("open zip archive {}", src.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("read zip archive {}", src.display()))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("archive contains too many entries: {}", archive.len());
    }
    let mut total_bytes = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("read zip entry {index}"))?;
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("zip entry has an unsafe path: {}", entry.name()))?;
        if entry.is_symlink() || (!entry.is_dir() && !entry.is_file()) {
            bail!(
                "archive links and special files are not allowed: {}",
                entry.name()
            );
        }
        let output = checked_archive_output_path(staging, &rel)?;
        if entry.is_dir() {
            fs::create_dir_all(&output)
                .with_context(|| format!("create extracted directory {}", output.display()))?;
            continue;
        }
        total_bytes = add_unpacked_size(total_bytes, entry.size(), max_unpacked_bytes)?;
        create_archive_parent(staging, &output)?;
        let mut output_file = create_extracted_file(&output)?;
        let copied = io::copy(&mut entry, &mut output_file)
            .with_context(|| format!("extract zip entry {}", entry.name()))?;
        if copied != entry.size() {
            bail!(
                "zip entry size changed while extracting {}: expected {}, wrote {}",
                entry.name(),
                entry.size(),
                copied
            );
        }
        set_safe_file_permissions(&output, entry.unix_mode())?;
    }
    Ok(())
}

fn extract_tar_archive(
    src: &Path,
    staging: &Path,
    format: ArchiveFormat,
    max_unpacked_bytes: u64,
) -> Result<()> {
    let file = File::open(src).with_context(|| format!("open tar archive {}", src.display()))?;
    let reader: Box<dyn Read> = match format {
        ArchiveFormat::Tar => Box::new(BufReader::new(file)),
        ArchiveFormat::TarGz => Box::new(flate2::read::GzDecoder::new(BufReader::new(file))),
        ArchiveFormat::TarBz2 => Box::new(bzip2::read::BzDecoder::new(BufReader::new(file))),
        ArchiveFormat::TarXz => Box::new(xz2::read::XzDecoder::new(BufReader::new(file))),
        _ => bail!("invalid tar archive format"),
    };
    let mut archive = tar::Archive::new(reader);
    let mut entry_count = 0usize;
    let mut total_bytes = 0u64;
    for entry in archive.entries().context("read tar archive entries")? {
        entry_count += 1;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            bail!("archive contains more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        let mut entry = entry.context("read tar archive entry")?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() && !entry_type.is_dir() {
            bail!("archive links and special files are not allowed");
        }
        let rel = entry.path().context("read tar entry path")?.into_owned();
        let output = checked_archive_output_path(staging, &rel)?;
        if entry_type.is_dir() {
            fs::create_dir_all(&output)
                .with_context(|| format!("create extracted directory {}", output.display()))?;
            continue;
        }
        let size = entry.header().size().context("read tar entry size")?;
        total_bytes = add_unpacked_size(total_bytes, size, max_unpacked_bytes)?;
        create_archive_parent(staging, &output)?;
        let mode = entry.header().mode().ok();
        let mut output_file = create_extracted_file(&output)?;
        let copied = io::copy(&mut entry, &mut output_file)
            .with_context(|| format!("extract tar entry {}", rel.display()))?;
        if copied != size {
            bail!(
                "tar entry size changed while extracting {}: expected {}, wrote {}",
                rel.display(),
                size,
                copied
            );
        }
        set_safe_file_permissions(&output, mode)?;
    }
    Ok(())
}

fn checked_archive_output_path(staging: &Path, rel: &Path) -> Result<PathBuf> {
    if rel.as_os_str().is_empty()
        || rel.is_absolute()
        || rel.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("archive entry escapes the destination: {}", rel.display());
    }
    Ok(staging.join(rel))
}

fn create_archive_parent(staging: &Path, output: &Path) -> Result<()> {
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("archive entry has no parent"))?;
    if !parent.starts_with(staging) {
        bail!("archive entry parent escapes the staging directory");
    }
    fs::create_dir_all(parent)
        .with_context(|| format!("create extracted file parent {}", parent.display()))
}

fn create_extracted_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create extracted file {}", path.display()))
}

fn add_unpacked_size(current: u64, entry: u64, maximum: u64) -> Result<u64> {
    let total = current
        .checked_add(entry)
        .ok_or_else(|| anyhow!("archive unpacked size overflow"))?;
    if total > maximum {
        bail!("archive exceeds max_unpacked_bytes ({maximum})");
    }
    Ok(total)
}

#[cfg(unix)]
fn set_safe_file_permissions(path: &Path, archived_mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let executable = archived_mode.is_some_and(|mode| mode & 0o111 != 0);
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("set safe permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_safe_file_permissions(_path: &Path, _archived_mode: Option<u32>) -> Result<()> {
    Ok(())
}

fn create_archive_staging_directory(parent: &Path) -> Result<PathBuf> {
    for index in 0..1_000u32 {
        let candidate = parent.join(format!(".ppduster-archive-{}-{index}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("create archive staging directory {}", candidate.display())
                });
            }
        }
    }
    bail!("could not allocate an archive staging directory")
}

fn remove_archive_staging(parent: &Path, staging: &Path) -> Result<()> {
    let name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid archive staging path {}", staging.display()))?;
    if staging.parent() != Some(parent) || !name.starts_with(".ppduster-archive-") {
        bail!(
            "refusing to remove unexpected archive staging path {}",
            staging.display()
        );
    }
    fs::remove_dir_all(staging)
        .with_context(|| format!("remove archive staging directory {}", staging.display()))
}

fn apply_install_dmg(
    dmg: &str,
    app_name: Option<&str>,
    target: Option<&str>,
    identity: Option<&AppBundleIdentity>,
    replace_existing: bool,
) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("install-dmg is only supported on macOS");
    }
    let dmg_path = expand_required_path(dmg)?;
    if !dmg_path.is_file() {
        bail!("dmg not found: {}", dmg_path.display());
    }

    verify_dmg(&dmg_path)?;
    let mount = MountedDmg::attach(&dmg_path)?;
    let install_result = (|| {
        let source_app = find_mounted_app(mount.path(), app_name)?;
        verify_installable_app(&source_app, identity)?;

        let install_root = expand_required_path(target.unwrap_or("$HOME/Applications"))?;
        fs::create_dir_all(&install_root)
            .with_context(|| format!("create install root {}", install_root.display()))?;
        require_real_directory(&install_root)?;

        let bundle_name = source_app
            .file_name()
            .ok_or_else(|| anyhow!("mounted app has no bundle name"))?;
        let destination = install_root.join(bundle_name);
        let destination_exists = path_entry_exists(&destination)?;
        if destination_exists && !replace_existing {
            bail!(
                "application already exists: {}; remove it explicitly before reinstalling",
                destination.display()
            );
        }
        if destination_exists {
            let expected = identity.ok_or_else(|| {
                anyhow!("replacing an application requires an exact signed identity")
            })?;
            verify_app_publisher(&destination, expected).with_context(|| {
                format!(
                    "refuse to replace an application from a different publisher: {}",
                    destination.display()
                )
            })?;
        }

        let staging = unique_app_staging_path(&install_root, bundle_name)?;
        if let Err(copy_err) = copy_app_bundle(&source_app, &staging) {
            if let Err(cleanup_err) = remove_staged_app(&install_root, &staging) {
                return Err(copy_err.context(format!(
                    "also failed to remove staging app: {cleanup_err:#}"
                )));
            }
            return Err(copy_err);
        }
        if let Err(verify_err) = verify_installable_app(&staging, identity) {
            if let Err(cleanup_err) = remove_staged_app(&install_root, &staging) {
                return Err(verify_err.context(format!(
                    "also failed to remove staging app: {cleanup_err:#}"
                )));
            }
            return Err(verify_err.context(format!("verify staged app {}", staging.display())));
        }
        let commit_result = if destination_exists {
            replace_with_staged_app(&install_root, &staging, &destination, identity)
        } else {
            commit_staged_app(&staging, &destination)
        };
        if let Err(commit_err) = commit_result {
            if let Err(cleanup_err) = remove_staged_app(&install_root, &staging) {
                return Err(commit_err.context(format!(
                    "also failed to remove staging app: {cleanup_err:#}"
                )));
            }
            return Err(commit_err);
        }
        verify_installable_app(&destination, identity)
            .with_context(|| format!("verify installed app {}", destination.display()))?;
        Ok(destination)
    })();

    let detach_result = mount.detach();
    let destination = match (install_result, detach_result) {
        (Ok(destination), Ok(())) => destination,
        (Ok(_), Err(detach_err)) => return Err(detach_err),
        (Err(install_err), Ok(())) => return Err(install_err),
        (Err(install_err), Err(detach_err)) => {
            return Err(install_err.context(format!("also failed to detach dmg: {detach_err:#}")));
        }
    };

    Ok(format!(
        "installed application from {} into {}",
        dmg_path.display(),
        destination.display()
    ))
}

struct MountedDmg {
    mount_point: PathBuf,
    attached: bool,
}

impl MountedDmg {
    fn attach(dmg_path: &Path) -> Result<Self> {
        let mount_point = unique_temp_directory("ppduster-dmg")?;
        let output = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-readonly", "-nobrowse", "-mountpoint"])
            .arg(&mount_point)
            .arg(dmg_path)
            .output()
            .with_context(|| format!("mount dmg {}", dmg_path.display()))?;
        if !output.status.success() {
            fs::remove_dir_all(&mount_point).ok();
            bail!(
                "mount dmg {} failed: {}",
                dmg_path.display(),
                command_error_detail(&output)
            );
        }
        Ok(Self {
            mount_point,
            attached: true,
        })
    }

    fn path(&self) -> &Path {
        &self.mount_point
    }

    fn detach(mut self) -> Result<()> {
        let output = Command::new("/usr/bin/hdiutil")
            .arg("detach")
            .arg(&self.mount_point)
            .output()
            .with_context(|| format!("detach dmg at {}", self.mount_point.display()))?;
        if !output.status.success() {
            bail!(
                "detach dmg at {} failed: {}",
                self.mount_point.display(),
                command_error_detail(&output)
            );
        }
        self.attached = false;
        fs::remove_dir_all(&self.mount_point).with_context(|| {
            format!(
                "remove temporary mount point {}",
                self.mount_point.display()
            )
        })?;
        Ok(())
    }
}

impl Drop for MountedDmg {
    fn drop(&mut self) {
        if self.attached {
            let detached = Command::new("/usr/bin/hdiutil")
                .arg("detach")
                .arg(&self.mount_point)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if detached {
                self.attached = false;
                let _ = fs::remove_dir_all(&self.mount_point);
            }
        }
    }
}

fn verify_dmg(dmg_path: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/hdiutil")
        .arg("verify")
        .arg(dmg_path)
        .output()
        .with_context(|| format!("verify dmg {}", dmg_path.display()))?;
    if !output.status.success() {
        bail!(
            "dmg verification failed for {}: {}",
            dmg_path.display(),
            command_error_detail(&output)
        );
    }
    Ok(())
}

fn find_mounted_app(mount_point: &Path, app_name: Option<&str>) -> Result<PathBuf> {
    let source_app = if let Some(app_name) = app_name {
        mount_point.join(app_name)
    } else {
        let mut candidates = fs::read_dir(mount_point)
            .with_context(|| format!("read mounted dmg {}", mount_point.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("app"))
            .collect::<Vec<_>>();
        candidates.sort();
        match candidates.as_slice() {
            [only] => only.clone(),
            [] => bail!("mounted dmg contains no .app bundle"),
            _ => bail!("mounted dmg contains multiple .app bundles; set app_name"),
        }
    };

    let metadata = fs::symlink_metadata(&source_app)
        .with_context(|| format!("inspect app bundle {}", source_app.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "app bundle is not a real directory: {}",
            source_app.display()
        );
    }
    let canonical_mount = mount_point
        .canonicalize()
        .with_context(|| format!("canonicalize mount point {}", mount_point.display()))?;
    let canonical_app = source_app
        .canonicalize()
        .with_context(|| format!("canonicalize app bundle {}", source_app.display()))?;
    if !canonical_app.starts_with(&canonical_mount) {
        bail!("app bundle escapes mounted dmg: {}", source_app.display());
    }
    Ok(source_app)
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("inspect path {}", path.display())),
    }
}

fn require_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "expected a real directory, not a symlink: {}",
            path.display()
        );
    }
    Ok(())
}

fn verify_installable_app(app_path: &Path, identity: Option<&AppBundleIdentity>) -> Result<()> {
    match identity {
        Some(identity) => verify_app_identity(app_path, identity),
        None => verify_app_signature(app_path),
    }
}

fn verify_app_signature(app_path: &Path) -> Result<()> {
    require_real_directory(app_path)?;
    let codesign = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app_path)
        .output()
        .with_context(|| format!("verify code signature for {}", app_path.display()))?;
    if !codesign.status.success() {
        bail!(
            "code signature verification failed for {}: {}",
            app_path.display(),
            command_error_detail(&codesign)
        );
    }

    let assessment = Command::new("/usr/sbin/spctl")
        .args(["--assess", "--type", "execute"])
        .arg(app_path)
        .output()
        .with_context(|| format!("assess app trust for {}", app_path.display()))?;
    if !assessment.status.success() {
        bail!(
            "Gatekeeper assessment failed for {}: {}",
            app_path.display(),
            command_error_detail(&assessment)
        );
    }
    Ok(())
}

fn app_identity_requirement(identity: &AppBundleIdentity) -> String {
    format!(
        "=identifier \"{}\" and anchor apple generic and certificate leaf[subject.OU] = \"{}\" and info[CFBundleShortVersionString] = \"{}\"",
        identity.bundle_identifier, identity.team_identifier, identity.version
    )
}

fn app_identity_verification_arguments(identity: &AppBundleIdentity) -> Vec<OsString> {
    vec![
        "--verify".into(),
        "--deep".into(),
        "--strict".into(),
        "--test-requirement".into(),
        app_identity_requirement(identity).into(),
    ]
}

fn verify_app_identity(app_path: &Path, identity: &AppBundleIdentity) -> Result<()> {
    verify_app_signature(app_path)?;
    let arguments = app_identity_verification_arguments(identity);
    let output = Command::new("/usr/bin/codesign")
        .args(arguments)
        .arg(app_path)
        .output()
        .with_context(|| format!("verify app identity for {}", app_path.display()))?;
    if !output.status.success() {
        bail!(
            "app identity verification failed for {}: {}",
            app_path.display(),
            command_error_detail(&output)
        );
    }
    Ok(())
}

fn app_publisher_requirement(identity: &AppBundleIdentity) -> String {
    format!(
        "=identifier \"{}\" and anchor apple generic and certificate leaf[subject.OU] = \"{}\"",
        identity.bundle_identifier, identity.team_identifier
    )
}

fn verify_app_publisher(app_path: &Path, identity: &AppBundleIdentity) -> Result<()> {
    verify_app_signature(app_path)?;
    let output = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "--test-requirement"])
        .arg(app_publisher_requirement(identity))
        .arg(app_path)
        .output()
        .with_context(|| format!("verify app publisher for {}", app_path.display()))?;
    if !output.status.success() {
        bail!(
            "app publisher verification failed for {}: {}",
            app_path.display(),
            command_error_detail(&output)
        );
    }
    Ok(())
}

fn read_app_version(app_path: &Path) -> Result<String> {
    let info_plist = app_path.join("Contents/Info.plist");
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(&info_plist)
        .output()
        .with_context(|| format!("read app version from {}", info_plist.display()))?;
    if !output.status.success() {
        bail!(
            "read app version from {} failed: {}",
            info_plist.display(),
            command_error_detail(&output)
        );
    }
    let version = String::from_utf8(output.stdout).context("app version is not UTF-8")?;
    let version = version.trim().to_string();
    parse_version(&version)?;
    Ok(version)
}

fn verify_bambu_app_and_read_version(app_path: &Path) -> Result<String> {
    let version = read_app_version(app_path)?;
    let identity = AppBundleIdentity {
        bundle_identifier: "com.bambulab.bambu-studio".into(),
        team_identifier: "T3UBR9Y3B2".into(),
        version: version.clone(),
    };
    verify_app_identity(app_path, &identity)?;
    Ok(version)
}

fn copy_app_bundle(source: &Path, destination: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/ditto")
        .arg(source)
        .arg(destination)
        .output()
        .with_context(|| format!("copy app bundle into {}", destination.display()))?;
    if !output.status.success() {
        bail!(
            "copy app bundle into {} failed: {}",
            destination.display(),
            command_error_detail(&output)
        );
    }
    Ok(())
}

fn unique_app_staging_path(install_root: &Path, bundle_name: &std::ffi::OsStr) -> Result<PathBuf> {
    let bundle_name = bundle_name.to_string_lossy();
    for index in 0..1_000u32 {
        let candidate = install_root.join(format!(
            ".{bundle_name}.ppduster-{}-{index}.app",
            std::process::id()
        ));
        if !path_entry_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    bail!("could not allocate an application staging path")
}

fn commit_staged_app(staging: &Path, destination: &Path) -> Result<()> {
    if path_entry_exists(destination)? {
        bail!(
            "application appeared while installing: {}; refusing to overwrite it",
            destination.display()
        );
    }

    fs::rename(staging, destination).with_context(|| {
        format!(
            "move staged app {} into {}",
            staging.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn replace_with_staged_app(
    install_root: &Path,
    staging: &Path,
    destination: &Path,
    identity: Option<&AppBundleIdentity>,
) -> Result<()> {
    let bundle_name = destination
        .file_name()
        .ok_or_else(|| anyhow!("application destination has no bundle name"))?;
    let backup = unique_app_backup_path(install_root, bundle_name)?;
    fs::rename(destination, &backup).with_context(|| {
        format!(
            "move existing app {} to rollback location",
            destination.display()
        )
    })?;
    if let Err(commit_err) = fs::rename(staging, destination) {
        fs::rename(&backup, destination).with_context(|| {
            format!(
                "restore {} after update commit failed: {commit_err}",
                destination.display()
            )
        })?;
        return Err(commit_err).with_context(|| format!("replace app {}", destination.display()));
    }
    if let Err(verify_err) = verify_installable_app(destination, identity) {
        remove_replacement_app(install_root, destination)?;
        fs::rename(&backup, destination).with_context(|| {
            format!(
                "restore {} after installed app verification failed",
                destination.display()
            )
        })?;
        return Err(verify_err).context("verify replacement app");
    }
    remove_backup_app(install_root, &backup)?;
    Ok(())
}

fn unique_app_backup_path(install_root: &Path, bundle_name: &std::ffi::OsStr) -> Result<PathBuf> {
    let bundle_name = bundle_name.to_string_lossy();
    for index in 0..1_000u32 {
        let candidate = install_root.join(format!(
            ".{bundle_name}.ppduster-backup-{}-{index}.app",
            std::process::id()
        ));
        if !path_entry_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    bail!("could not allocate an application rollback path")
}

fn remove_backup_app(install_root: &Path, backup: &Path) -> Result<()> {
    let file_name = backup
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid backup app path {}", backup.display()))?;
    if backup.parent() != Some(install_root)
        || !file_name.starts_with('.')
        || !file_name.contains(".app.ppduster-backup-")
        || !file_name.ends_with(".app")
    {
        bail!(
            "refusing to remove unexpected backup path {}",
            backup.display()
        );
    }
    fs::remove_dir_all(backup).with_context(|| format!("remove rollback app {}", backup.display()))
}

fn remove_replacement_app(install_root: &Path, destination: &Path) -> Result<()> {
    if destination.parent() != Some(install_root)
        || destination.file_name().and_then(|name| name.to_str()) != Some("BambuStudio.app")
    {
        bail!(
            "refusing to remove unexpected replacement path {}",
            destination.display()
        );
    }
    fs::remove_dir_all(destination)
        .with_context(|| format!("remove failed replacement {}", destination.display()))
}

fn remove_staged_app(install_root: &Path, staging: &Path) -> Result<()> {
    let file_name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid staging app path {}", staging.display()))?;
    if staging.parent() != Some(install_root)
        || !file_name.starts_with('.')
        || !file_name.contains(".app.ppduster-")
        || !file_name.ends_with(".app")
    {
        bail!(
            "refusing to remove unexpected staging path {}",
            staging.display()
        );
    }
    if !path_entry_exists(staging)? {
        return Ok(());
    }

    fs::remove_dir_all(staging)
        .with_context(|| format!("remove staged app {}", staging.display()))?;
    Ok(())
}

fn command_error_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("exit status {}", output.status)
    } else {
        detail.to_string()
    }
}

fn unique_temp_directory(prefix: &str) -> Result<PathBuf> {
    let root = std::env::temp_dir();
    for index in 0..1_000u32 {
        let candidate = root.join(format!("{prefix}-{}-{index}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("create mount point {}", candidate.display()));
            }
        }
    }
    bail!("could not allocate a temporary dmg mount point")
}

fn apply_macos_requirements(
    minimum_version: &str,
    require_rosetta_on_apple_silicon: bool,
) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("macos-requirements is only supported on macOS");
    }

    let version_output = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .context("read macOS version")?;
    if !version_output.status.success() {
        bail!(
            "read macOS version failed: {}",
            command_error_detail(&version_output)
        );
    }
    let current_version = String::from_utf8_lossy(&version_output.stdout)
        .trim()
        .to_owned();
    if !version_at_least(&current_version, minimum_version)? {
        bail!(
            "macOS {} is unsupported; this task requires macOS {} or newer",
            current_version,
            minimum_version
        );
    }

    let mut rosetta_checked = false;
    if require_rosetta_on_apple_silicon {
        let architecture_output = Command::new("/usr/bin/uname")
            .arg("-m")
            .output()
            .context("read Mac architecture")?;
        if !architecture_output.status.success() {
            bail!(
                "read Mac architecture failed: {}",
                command_error_detail(&architecture_output)
            );
        }
        let architecture = String::from_utf8_lossy(&architecture_output.stdout);
        if architecture.trim() == "arm64" {
            rosetta_checked = true;
            let rosetta = Command::new("/usr/sbin/pkgutil")
                .args(["--pkg-info", "com.apple.pkg.RosettaUpdateAuto"])
                .output()
                .context("check Rosetta package receipt")?;
            if !rosetta.status.success() {
                bail!(
                    "Rosetta is required on Apple Silicon but is not installed; install it with Apple's softwareupdate tool before retrying"
                );
            }
        }
    }

    Ok(format!(
        "macOS {} satisfies minimum {}{}",
        current_version,
        minimum_version,
        if rosetta_checked {
            "; Rosetta is installed"
        } else {
            ""
        }
    ))
}

fn version_at_least(current: &str, minimum: &str) -> Result<bool> {
    Ok(compare_versions(current, minimum)? != Ordering::Less)
}

fn compare_versions(left: &str, right: &str) -> Result<Ordering> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    let component_count = left.len().max(right.len());
    for index in 0..component_count {
        let left_component = *left.get(index).unwrap_or(&0);
        let right_component = *right.get(index).unwrap_or(&0);
        match left_component.cmp(&right_component) {
            Ordering::Equal => {}
            ordering => return Ok(ordering),
        }
    }
    Ok(Ordering::Equal)
}

fn parse_version(value: &str) -> Result<Vec<u64>> {
    if value.trim().is_empty() {
        bail!("version must not be empty");
    }
    value
        .trim()
        .split('.')
        .map(|component| {
            component
                .parse::<u64>()
                .with_context(|| format!("invalid version component {component:?} in {value:?}"))
        })
        .collect()
}

fn app_store_operation_name(operation: AppStoreOperation) -> &'static str {
    match operation {
        AppStoreOperation::Install => "install",
        AppStoreOperation::Get => "get",
    }
}

fn app_store_country_override() -> Result<Option<String>> {
    let country = match std::env::var("PPDUSTER_APP_STORE_COUNTRY") {
        Ok(country) if country.trim().is_empty() => return Ok(None),
        Ok(country) => country,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("PPDUSTER_APP_STORE_COUNTRY contains non-Unicode data")
        }
    };
    let country = country.trim();
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        bail!("PPDUSTER_APP_STORE_COUNTRY must be a two-letter country code");
    }
    Ok(Some(country.to_ascii_uppercase()))
}

fn apply_app_store_install(app_id: u64, operation: AppStoreOperation) -> Result<ApplyStepResult> {
    if !cfg!(target_os = "macos") {
        bail!("app-store-install is only supported on macOS");
    }
    let country = app_store_country_override()?;
    let outcome = ppstore::install(
        app_id,
        matches!(operation, AppStoreOperation::Get),
        country.as_deref(),
    )
    .with_context(|| {
        format!(
            "run ppstore {} for App Store application {}",
            app_store_operation_name(operation),
            app_id
        )
    })?;
    match outcome {
        InstallOutcome::Applied(summary) => Ok(ApplyStepResult::Applied(summary)),
        InstallOutcome::AlreadySatisfied(summary) => Ok(ApplyStepResult::AlreadySatisfied(summary)),
    }
}

fn apply_install_pkg(pkg: &str, target: Option<&str>) -> Result<String> {
    let pkg_path = expand_required_path(pkg)?;
    if !pkg_path.exists() {
        bail!("pkg not found: {}", pkg_path.display());
    }
    let target_path = if let Some(target) = target {
        expand_required_path(target)?
    } else {
        dirs::home_dir()
            .map(|home| home.join("Library/Packages"))
            .ok_or_else(|| anyhow!("home directory unavailable"))?
    };
    fs::create_dir_all(&target_path)
        .with_context(|| format!("create pkg target {}", target_path.display()))?;
    let destination = target_path.join(pkg_path.file_name().unwrap_or_default());
    fs::copy(&pkg_path, &destination).with_context(|| {
        format!(
            "copy pkg {} to {}",
            pkg_path.display(),
            destination.display()
        )
    })?;
    Ok(format!(
        "staged pkg {} into {}",
        pkg_path.display(),
        destination.display()
    ))
}

fn apply_activate_license(provider: LicenseProvider, method: LicenseMethod) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("LightBurn vendor UI activation is only supported on macOS");
    }
    let interactive = terminal_is_interactive();
    apply_activate_license_with(
        provider,
        method,
        interactive,
        launch_license_ui,
        prompt_activation_confirmation,
    )
}

fn terminal_is_interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn apply_activate_license_with<Launch, Confirm>(
    provider: LicenseProvider,
    method: LicenseMethod,
    interactive: bool,
    mut launch: Launch,
    mut confirm: Confirm,
) -> Result<String>
where
    Launch: FnMut(LicenseProvider) -> Result<()>,
    Confirm: FnMut(&str) -> Result<bool>,
{
    if !interactive {
        bail!(
            "license activation requires an interactive terminal; the key must be entered directly in the vendor UI"
        );
    }

    match (provider, method) {
        (LicenseProvider::LightBurn, LicenseMethod::VendorUi) => {
            launch(provider)?;
            eprintln!(
                "LightBurn is open. Enter the license key in its License Page (or Help -> License Management), then activate it."
            );
            if !confirm(
                "Type ACTIVATED here only after LightBurn reports a successful activation: ",
            )? {
                bail!("LightBurn activation was not confirmed; expected ACTIVATED");
            }
            Ok(
                "user confirmed LightBurn activation in the vendor UI; ppduster did not read or store the license key"
                    .into(),
            )
        }
    }
}

fn launch_license_ui(provider: LicenseProvider) -> Result<()> {
    let app_path = license_application_path(provider)?;
    match provider {
        LicenseProvider::LightBurn => verify_lightburn_identity(&app_path)?,
    }
    require_license_application_stopped(provider)?;
    let arguments = license_launch_arguments(&app_path);
    Command::new("/usr/bin/open")
        .args(arguments)
        .status()
        .with_context(|| format!("open {} license UI", app_path.display()))?
        .exit_ok(&format!("open {}", app_path.display()))?;
    Ok(())
}

fn license_application_path(provider: LicenseProvider) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    match provider {
        LicenseProvider::LightBurn => Ok(home.join("Applications/LightBurn.app")),
    }
}

fn require_license_application_stopped(provider: LicenseProvider) -> Result<()> {
    let process_name = match provider {
        LicenseProvider::LightBurn => "LightBurn",
    };
    let status = Command::new("/usr/bin/pgrep")
        .args(["-x", process_name])
        .status()
        .with_context(|| format!("check for running {} processes", process_name))?;
    match status.code() {
        Some(1) => Ok(()),
        Some(0) => bail!(
            "{} is already running; quit every instance and rerun so ppduster can open the verified app bundle",
            process_name
        ),
        Some(code) => bail!(
            "checking for running {} failed with exit code {}",
            process_name,
            code
        ),
        None => bail!("checking for running {} terminated by signal", process_name),
    }
}

fn license_launch_arguments(app_path: &Path) -> Vec<OsString> {
    vec!["-n".into(), app_path.as_os_str().to_owned()]
}

const LIGHTBURN_BUNDLE_IDENTIFIER: &str = "com.LightBurnSoftware.LightBurn";
const LIGHTBURN_TEAM_IDENTIFIER: &str = "UWZQ3LL82C";
const LIGHTBURN_VERSION: &str = "2.1.03";

fn verify_lightburn_identity(app_path: &Path) -> Result<()> {
    let identity = AppBundleIdentity {
        bundle_identifier: LIGHTBURN_BUNDLE_IDENTIFIER.into(),
        team_identifier: LIGHTBURN_TEAM_IDENTIFIER.into(),
        version: LIGHTBURN_VERSION.into(),
    };
    verify_app_identity(app_path, &identity).with_context(|| {
        format!(
            "refuse to open untrusted LightBurn app at {}",
            app_path.display()
        )
    })
}

fn prompt_activation_confirmation(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    io::stderr()
        .flush()
        .context("flush activation confirmation prompt")?;
    let mut line = String::new();
    let bytes = io::stdin()
        .read_line(&mut line)
        .context("read activation confirmation")?;
    if bytes == 0 {
        bail!("activation confirmation ended before ACTIVATED was entered");
    }
    Ok(line.trim() == "ACTIVATED")
}

fn apply_run_command(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
    shell: ShellMode,
) -> Result<ApplyStepResult> {
    let mut command = if matches!(shell, ShellMode::Allow) {
        let shell_command = render_shell_command(program, args);
        let mut shell_runner = Command::new("/bin/sh");
        shell_runner.arg("-lc").arg(shell_command);
        shell_runner
    } else {
        let trusted_program = if program == "sudo" {
            "/usr/bin/sudo"
        } else {
            program
        };
        let mut direct = Command::new(trusted_program);
        direct.args(expand_args(args)?);
        direct
    };
    if let Some(cwd) = cwd {
        command.current_dir(expand_required_path(cwd)?);
    }
    for (key, value) in env {
        command.env(key, expand_env_value(value)?);
    }
    let status = command
        .status()
        .with_context(|| format!("run command {}", program))?;
    let exit_code = status.code().map(|code| code as u32);
    let termination_signal = process_termination_signal(&status);
    let accepted = status.success();
    let output = StepOutput::ProcessExit(ProcessExitOutput {
        exit_code,
        termination_signal,
        accepted,
        success_exit_codes: vec![0],
    });
    let command_label = render_command(program, args, cwd);
    if accepted {
        return Ok(ApplyStepResult::AppliedWithOutput {
            summary: format!("ran {command_label}"),
            output,
        });
    }
    let error = match exit_code {
        Some(code) => format!("{} failed with exit code {}", program, code),
        None => match termination_signal {
            Some(signal) => format!("{} terminated by signal {}", program, signal),
            None => format!("{} did not exit normally", program),
        },
    };
    Ok(ApplyStepResult::Failed {
        summary: error.clone(),
        error,
        output: Some(output),
    })
}

fn apply_run_script(
    interpreter: ScriptInterpreter,
    script: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
    success_exit_codes: &[u32],
) -> Result<ApplyStepResult> {
    let process_cwd = std::env::current_dir().context("resolve current directory")?;
    let working_directory = cwd
        .map(expand_required_path)
        .transpose()?
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                process_cwd.join(path)
            }
        })
        .map(|path| validate_script_working_directory(&path))
        .transpose()?;
    let expanded_script = expand_required_path(script)?;
    let requested_script_path = if expanded_script.is_absolute() {
        expanded_script
    } else {
        working_directory
            .as_ref()
            .unwrap_or(&process_cwd)
            .join(expanded_script)
    };
    let metadata = fs::symlink_metadata(&requested_script_path)
        .with_context(|| format!("inspect script {}", requested_script_path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "script must not be a symlink: {}",
            requested_script_path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "script is not a regular file: {}",
            requested_script_path.display()
        );
    }
    let script_path = requested_script_path
        .canonicalize()
        .with_context(|| format!("resolve script {}", requested_script_path.display()))?;

    let expanded_args = expand_args(args)?;
    let mut missing = Vec::new();
    for program in script_interpreter_candidates(interpreter) {
        let mut command = Command::new(program);
        configure_script_command(&mut command, interpreter, &script_path, &expanded_args);
        if let Some(directory) = &working_directory {
            command.current_dir(directory);
        }
        for (key, value) in env {
            command.env(key, expand_env_value(value)?);
        }
        match command.status() {
            Ok(status) => {
                let exit_code = status.code().map(|code| code as u32);
                let termination_signal = process_termination_signal(&status);
                let accepted = exit_code.is_some_and(|code| success_exit_codes.contains(&code));
                let output = StepOutput::ProcessExit(ProcessExitOutput {
                    exit_code,
                    termination_signal,
                    accepted,
                    success_exit_codes: success_exit_codes.to_vec(),
                });
                let script_label = format!(
                    "{} script {}",
                    script_interpreter_name(interpreter),
                    script_path.display()
                );
                return match exit_code {
                    Some(code) if accepted => Ok(ApplyStepResult::AppliedWithOutput {
                        summary: format!("ran {} with accepted exit code {}", script_label, code),
                        output,
                    }),
                    Some(code) => {
                        let error = format!(
                            "{} returned exit code {}; configured success_exit_codes are [{}]",
                            script_label,
                            code,
                            format_exit_codes(success_exit_codes)
                        );
                        Ok(ApplyStepResult::Failed {
                            summary: error.clone(),
                            error,
                            output: Some(output),
                        })
                    }
                    None => {
                        let termination = termination_signal
                            .map(|signal| format!("terminated by signal {}", signal))
                            .unwrap_or_else(|| "did not exit normally".into());
                        let error = format!(
                            "{} {}; success_exit_codes apply only to normal exits",
                            script_label, termination
                        );
                        Ok(ApplyStepResult::Failed {
                            summary: error.clone(),
                            error,
                            output: Some(output),
                        })
                    }
                };
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push((*program).to_owned());
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "start {} interpreter {} for script {}",
                        script_interpreter_name(interpreter),
                        program,
                        script_path.display()
                    )
                });
            }
        }
    }

    bail!(
        "{} interpreter not found (tried {}); install it or make it available on PATH",
        script_interpreter_name(interpreter),
        missing.join(", ")
    )
}

fn process_termination_signal(status: &ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

fn format_exit_codes(codes: &[u32]) -> String {
    codes
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_script_working_directory(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect script working directory {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "script working directory must not be a symlink: {}",
            path.display()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "script working directory is not a directory: {}",
            path.display()
        );
    }
    path.canonicalize()
        .with_context(|| format!("resolve script working directory {}", path.display()))
}

fn configure_script_command(
    command: &mut Command,
    interpreter: ScriptInterpreter,
    script: &Path,
    args: &[OsString],
) {
    if matches!(interpreter, ScriptInterpreter::PowerShell) {
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive"]);
        if cfg!(target_os = "windows") {
            command.args(["-ExecutionPolicy", "Bypass"]);
        }
        command.arg("-File");
    }
    command.arg(script).args(args);
}

fn script_interpreter_candidates(interpreter: ScriptInterpreter) -> &'static [&'static str] {
    match interpreter {
        ScriptInterpreter::Sh if cfg!(target_os = "windows") => &["sh.exe", "sh"],
        ScriptInterpreter::Sh => &["/bin/sh", "sh"],
        ScriptInterpreter::Bash if cfg!(target_os = "windows") => &["bash.exe", "bash"],
        ScriptInterpreter::Bash => &["bash", "/bin/bash"],
        ScriptInterpreter::PowerShell if cfg!(target_os = "windows") => {
            &["pwsh.exe", "powershell.exe"]
        }
        ScriptInterpreter::PowerShell => &["pwsh", "powershell"],
    }
}

fn script_interpreter_name(interpreter: ScriptInterpreter) -> &'static str {
    match interpreter {
        ScriptInterpreter::Sh => "sh",
        ScriptInterpreter::Bash => "Bash",
        ScriptInterpreter::PowerShell => "PowerShell",
    }
}

fn script_interpreter_requirement(interpreter: ScriptInterpreter) -> &'static str {
    match interpreter {
        ScriptInterpreter::Sh => "a POSIX sh interpreter",
        ScriptInterpreter::Bash => "the Bash interpreter",
        ScriptInterpreter::PowerShell => "PowerShell (pwsh, or Windows PowerShell on Windows)",
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("open {} for checksum", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .with_context(|| format!("read {} for checksum", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn create_unique_temp_file(dest: &Path) -> Result<(PathBuf, fs::File)> {
    let mut index = 0u64;
    loop {
        let candidate = if let Some(ext) = dest.extension() {
            let mut path = dest.to_path_buf();
            let new_ext = format!("{}.{}.part", ext.to_string_lossy(), index);
            path.set_extension(new_ext);
            path
        } else {
            dest.with_extension(format!("part.{}", index))
        };
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("create temporary download {}", candidate.display()));
            }
        }
        index += 1;
    }
}

fn expand_required_path(raw: &str) -> Result<PathBuf> {
    expand_path_template(raw).ok_or_else(|| anyhow!("unexpanded path template {}", raw))
}

fn expand_args(args: &[String]) -> Result<Vec<OsString>> {
    args.iter().map(|arg| expand_arg(arg)).collect()
}

fn expand_arg(arg: &str) -> Result<OsString> {
    if let Some(path) = expand_path_template(arg) {
        return Ok(path.into_os_string());
    }
    Ok(OsString::from(arg))
}

fn expand_env_value(value: &str) -> Result<OsString> {
    if let Some(path) = expand_path_template(value) {
        return Ok(path.into_os_string());
    }
    Ok(OsString::from(value))
}

fn render_command(program: &str, args: &[String], cwd: Option<&str>) -> String {
    let mut rendered = String::from(program);
    for arg in args {
        rendered.push(' ');
        rendered.push_str(arg);
    }
    if let Some(cwd) = cwd {
        rendered.push_str(" (cwd ");
        rendered.push_str(cwd);
        rendered.push(')');
    }
    rendered
}

fn render_shell_command(program: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell_escape(program));
    for arg in args {
        parts.push(shell_escape(arg));
    }
    parts.join(" ")
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

trait ExitStatusExt {
    fn exit_ok(self, action: &str) -> Result<()>;
}

impl ExitStatusExt for ExitStatus {
    fn exit_ok(self, action: &str) -> Result<()> {
        if self.success() {
            return Ok(());
        }
        match self.code() {
            Some(code) => bail!("{} failed with exit code {}", action, code),
            None => bail!("{} terminated by signal", action),
        }
    }
}

fn is_satisfied(step: &Step, run_command_checks: bool) -> Result<Option<String>> {
    if let Action::CreateDirectory(action) = &step.action {
        let path = validate_create_directory_path(&action.path)?;
        return match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(Some(
                format!("directory already exists: {}", path.display()),
            )),
            Ok(_) => bail!(
                "directory destination is not a real directory: {}",
                path.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("inspect directory {}", path.display()))
            }
        };
    }
    if let Action::WriteFile(action) = &step.action {
        let path = validate_safe_mutation_path(&action.path, "write-file")?;
        return match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => bail!(
                "write-file destination is not a regular file: {}",
                path.display()
            ),
            Ok(_) if file_matches_bytes(&path, action.content.as_bytes())? => Ok(Some(format!(
                "file already has the requested content: {}",
                path.display()
            ))),
            Ok(_) if matches!(action.on_conflict, WriteConflictPolicy::Fail) => bail!(
                "write-file destination has different content: {}",
                path.display()
            ),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("inspect write destination {}", path.display()))
            }
        };
    }
    if let Action::CopyPath(action) = &step.action {
        let src = validate_declared_path(&action.src)?;
        let dest = validate_safe_mutation_path(&action.dest, "copy-path")?;
        let source_exists = match fs::symlink_metadata(&src) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect copy source {}", src.display()));
            }
        };
        if !source_exists {
            if run_command_checks {
                bail!("copy-path source does not exist: {}", src.display());
            }
            return Ok(None);
        }
        let src = validate_copy_source(&action.src)?;
        validate_copy_relationship(&src, &dest)?;
        return if path_entry_exists(&dest)? {
            if paths_have_equal_content(&src, &dest)? {
                Ok(Some(format!(
                    "destination already matches source: {}",
                    dest.display()
                )))
            } else {
                bail!(
                    "copy-path destination exists with different content: {}",
                    dest.display()
                )
            }
        } else {
            Ok(None)
        };
    }
    if let Action::RemovePath(action) = &step.action {
        let path = validate_safe_mutation_path(&action.path, "remove-path")?;
        return if path_entry_exists(&path)? {
            Ok(None)
        } else {
            Ok(Some(format!("path is already absent: {}", path.display())))
        };
    }
    // Repository presence is not repository freshness. A git-clone action must
    // reach its apply phase so it can fetch the requested branch and decide
    // whether the existing checkout is current, behind, ahead, or diverged.
    if matches!(
        &step.action,
        Action::GitClone { .. }
            | Action::GitInspect { .. }
            | Action::GitCloneIfMissing { .. }
            | Action::GitFetch { .. }
            | Action::GitFastForward { .. }
    ) {
        return Ok(None);
    }
    if run_command_checks {
        if let Action::ConfigurePackageRegistryFiles {
            secrets,
            npm,
            nuget,
        } = &step.action
        {
            if let Some(reason) = package_registry::is_satisfied(secrets, npm, nuget)? {
                return Ok(Some(reason));
            }
        }
        if let Action::InstallDmg {
            app_name: Some(app_name),
            target,
            identity: Some(identity),
            ..
        } = &step.action
        {
            let destination = dmg_install_destination(app_name, target.as_deref())?;
            if path_entry_exists(&destination)? {
                verify_app_identity(&destination, identity)?;
                return Ok(Some(format!(
                    "verified {} version {} signed by team {} at {}",
                    identity.bundle_identifier,
                    identity.version,
                    identity.team_identifier,
                    destination.display()
                )));
            }
        }
    }

    let Some(check) = &step.check else {
        return Ok(None);
    };
    if let Some(path) = &check.path_exists {
        let expanded =
            expand_path_template(&path.to_string_lossy()).unwrap_or_else(|| path.clone());
        if expanded.exists() {
            return Ok(Some(format!("path exists: {}", expanded.display())));
        }
    }
    if let Some(cmd) = &check.command_succeeds {
        if cmd.is_empty() || !run_command_checks {
            return Ok(None);
        }
        let status = Command::new(&cmd[0])
            .args(&cmd[1..])
            .status()
            .with_context(|| format!("run satisfaction check for step {}", step.id))?;
        if status.success() {
            return Ok(Some(format!("command succeeded: {}", cmd.join(" "))));
        }
    }
    Ok(None)
}

pub fn extracted_path_is_safe(root: &Path, rel: &Path) -> bool {
    let candidate = root.join(rel);
    stays_under_root(root, &candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::context::TemplatePart;
    use crate::automation::expression::ExpressionValue;
    use crate::automation::graph::{GraphEdge, LegacyTaskImporter, SwitchCase};
    use crate::automation::loader::{PackTrust, TaskPack, TaskSource};
    use crate::automation::task::{
        ActivateLicenseAction, AppBundleIdentity, AppStoreInstallAction, Checksum, CopyPathAction,
        CreateDirectoryAction, InspectPathAction, PathExpectation, PathKind, RemovePathAction,
        ScriptInterpreter, StepCondition, Task, TrustRequirement, WriteFileAction,
    };
    use std::path::PathBuf;

    fn base_task(step: Step) -> Task {
        Task {
            id: "setup-dev".into(),
            name: "Setup dev".into(),
            description: "Test setup scenario.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![step],
            graph: None,
        }
    }

    fn protected_documents_test_destination(label: &str) -> Option<PathBuf> {
        let documents = dirs::home_dir()?.join("Documents");
        if !documents.is_dir() {
            return None;
        }
        let destination = documents.join(format!(
            ".ppduster-runner-protected-{label}-test-{}",
            std::process::id()
        ));
        (!destination.exists()).then_some(destination)
    }

    /// Unit fixtures still use the compact legacy step builder. Import them
    /// explicitly before crossing the production graph-only runtime boundary.
    fn run_task(task: &Task, opts: &RunOptions) -> Result<RunReport> {
        let canonical = task.to_v3().map_err(anyhow::Error::new)?;
        super::run_task(&canonical, opts)
    }

    fn run_imported_task_with_interactivity(
        task: &Task,
        opts: &RunOptions,
        terminal_interactive: bool,
    ) -> Result<RunReport> {
        let canonical = task.to_v3().map_err(anyhow::Error::new)?;
        super::run_task_with_interactivity(&canonical, opts, terminal_interactive)
    }

    struct GitTestRepository {
        _temp: tempfile::TempDir,
        remote: PathBuf,
        seed: PathBuf,
    }

    fn test_git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-c")
            .arg("core.hooksPath=/dev/null")
            .arg("-c")
            .arg("commit.gpgSign=false")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn init_git_test_repository() -> GitTestRepository {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let seed = temp.path().join("seed");
        fs::create_dir(&seed).unwrap();
        let remote_arg = remote.to_string_lossy().into_owned();
        test_git(temp.path(), &["init", "--bare", &remote_arg]);
        test_git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        test_git(&seed, &["init"]);
        test_git(&seed, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        test_git(&seed, &["config", "user.name", "ppduster tests"]);
        test_git(
            &seed,
            &["config", "user.email", "ppduster-tests@example.invalid"],
        );
        fs::write(seed.join("state.txt"), "initial\n").unwrap();
        test_git(&seed, &["add", "state.txt"]);
        test_git(&seed, &["commit", "-m", "initial"]);
        test_git(&seed, &["remote", "add", "origin", &remote_arg]);
        test_git(&seed, &["push", "--set-upstream", "origin", "main"]);
        GitTestRepository {
            _temp: temp,
            remote,
            seed,
        }
    }

    fn push_git_test_commit(repository: &GitTestRepository, contents: &str) -> String {
        fs::write(repository.seed.join("state.txt"), contents).unwrap();
        test_git(&repository.seed, &["add", "state.txt"]);
        test_git(&repository.seed, &["commit", "-m", contents.trim()]);
        test_git(&repository.seed, &["push", "origin", "main"]);
        test_git(&repository.seed, &["rev-parse", "HEAD"])
    }

    fn git_sync_task(remote: &Path, destination: &Path) -> Task {
        base_task(Step {
            id: "sync-repository".into(),
            name: "Sync repository".into(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: Some(crate::automation::task::Check {
                path_exists: Some(destination.join(".git")),
                command_succeeds: None,
            }),
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::GitClone {
                repo: remote.to_string_lossy().into_owned(),
                dest: destination.to_string_lossy().into_owned(),
                branch: Some("main".into()),
            },
        })
    }

    fn atomic_git_sync_task(remote: &Path, destination: &Path) -> Task {
        let repo = remote.to_string_lossy().into_owned();
        let dest = destination.to_string_lossy().into_owned();
        let mut task = base_task(plain_step(
            "inspect-repository",
            Action::GitInspect {
                repo: repo.clone(),
                dest: dest.clone(),
            },
        ));
        task.steps.push(plain_step(
            "clone-repository",
            Action::GitCloneIfMissing {
                repo: repo.clone(),
                dest: dest.clone(),
                branch: Some("main".into()),
            },
        ));
        task.steps.push(plain_step(
            "fetch-repository",
            Action::GitFetch {
                repo: repo.clone(),
                dest: dest.clone(),
                branch: "main".into(),
            },
        ));
        task.steps.push(plain_step(
            "update-main",
            Action::GitFastForward {
                repo,
                dest,
                branch: "main".into(),
            },
        ));
        task
    }

    fn apply_test_task(task: &Task) -> RunReport {
        run_task(
            task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap()
    }

    fn plain_step(id: &str, action: Action) -> Step {
        Step {
            id: id.into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action,
        }
    }

    fn graph_task(graph: WorkflowGraph) -> Task {
        let mut task = base_task(plain_step(
            "placeholder",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        ));
        task.steps.clear();
        task.graph = Some(graph);
        task
    }

    fn action_node(step: Step, bindings: BTreeMap<String, Binding>) -> GraphNode {
        GraphNode::Action(Box::new(ActionNode { step, bindings }))
    }

    fn one_action_graph(step: Step, bindings: BTreeMap<String, Binding>) -> WorkflowGraph {
        WorkflowGraph {
            entries: vec![step.id.clone()],
            nodes: vec![action_node(step, bindings)],
            ..WorkflowGraph::default()
        }
    }

    #[test]
    fn canonical_v3_task_executes_without_a_steps_projection() {
        let temp = tempfile::tempdir().unwrap();
        let inspected = temp.path().join("canonical-v3");
        fs::create_dir(&inspected).unwrap();
        let step = plain_step(
            "inspect-canonical",
            Action::InspectPath(InspectPathAction {
                path: inspected.to_string_lossy().into_owned(),
                recursive_size: false,
                sha256: false,
                expect: None,
            }),
        );
        let task = graph_task(one_action_graph(step, BTreeMap::new()));

        assert!(task.steps.is_empty());
        assert!(task.is_v3());
        let report = run_task_with_interactivity(&task, &RunOptions::default(), false).unwrap();

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
        let path = report
            .context
            .resolve(&FieldRef::step("inspect-canonical").field("path"))
            .unwrap();
        assert_eq!(path.value, inspected.to_string_lossy().as_ref());
    }

    #[test]
    fn legacy_steps_require_explicit_import_before_execution() {
        let temp = tempfile::tempdir().unwrap();
        let inspected = temp.path().join("legacy-import");
        fs::create_dir(&inspected).unwrap();
        let task = base_task(plain_step(
            "inspect-legacy",
            Action::InspectPath(InspectPathAction {
                path: inspected.to_string_lossy().into_owned(),
                recursive_size: false,
                sha256: false,
                expect: None,
            }),
        ));

        assert!(task.graph.is_none());
        assert_eq!(task.steps.len(), 1);
        let error = run_task_with_interactivity(&task, &RunOptions::default(), false).unwrap_err();
        assert!(
            error.to_string().contains("not a canonical workflow graph"),
            "{error:#}"
        );

        let canonical = task.to_v3().unwrap();
        let report =
            run_task_with_interactivity(&canonical, &RunOptions::default(), false).unwrap();

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.steps.len(), 1);
        assert!(canonical.steps.is_empty());
        assert!(canonical.is_v3());
    }

    #[test]
    fn graph_action_materializes_github_repository_into_clone_plan() {
        let mut scope = GraphScopeState::default();
        let producer = plain_step("github", Action::GithubListRepositories);
        let definition = definition_for_action(&producer.action);
        scope.values.insert(
            ContextScope::Step {
                step_id: producer.id.clone(),
            },
            ContextValue::new(
                serde_json::json!({
                    "github": {
                        "account": { "login": "octocat" },
                        "repositories": [{
                            "id": "42",
                            "owner": "octocat",
                            "name": "hello-world",
                            "full_name": "octocat/hello-world",
                            "https_url": "https://github.com/octocat/hello-world.git",
                            "ssh_url": "git@github.com:octocat/hello-world.git",
                            "default_branch": "main",
                            "private": false,
                            "archived": false
                        }]
                    }
                }),
                ContextProvenance::step(&producer.id),
            )
            .with_schema(definition.output_schema.clone()),
        );
        scope.schemas.insert(
            ContextScope::Step {
                step_id: producer.id.clone(),
            },
            ContextValue::new(
                serde_json::Value::Null,
                ContextProvenance::step(&producer.id),
            )
            .with_schema(definition.output_schema),
        );
        let repository = FieldRef::step("github")
            .field("github")
            .field("repositories")
            .index(0);
        let clone = ActionNode {
            step: plain_step(
                "clone",
                Action::GitCloneIfMissing {
                    repo: "https://github.com/example/example.git".into(),
                    dest: "$HOME/Developer/example/example".into(),
                    branch: Some("main".into()),
                },
            ),
            bindings: BTreeMap::from([
                (
                    "repo".into(),
                    Binding::field(repository.clone().field("https_url")),
                ),
                (
                    "dest".into(),
                    Binding::interpolated([
                        TemplatePart::literal("$HOME/Developer/"),
                        TemplatePart::field(repository.clone().field("owner")),
                        TemplatePart::literal("/"),
                        TemplatePart::field(repository.clone().field("name")),
                    ]),
                ),
                (
                    "branch".into(),
                    Binding::field(repository.field("default_branch")),
                ),
            ]),
        };
        let task = graph_task(one_action_graph(clone.step.clone(), clone.bindings.clone()));
        let opts = RunOptions::default();
        let mut runtime = GraphRuntime {
            task: &task,
            opts: &opts,
            terminal_interactive: false,
            accumulator: GraphRunAccumulator::default(),
            budget: GraphExecutionBudget::default(),
        };

        let result = runtime.execute_action(&clone, &mut scope, "").unwrap();

        assert!(result.successful);
        assert_eq!(runtime.accumulator.plans.len(), 1);
        let summary = &runtime.accumulator.plans[0].summary;
        assert!(summary.contains("https://github.com/octocat/hello-world.git"));
        assert!(summary.contains("$HOME/Developer/octocat/hello-world"));
        assert!(summary.contains("branch main"));
    }

    #[test]
    fn missing_positional_binding_fails_before_the_consumer_action_runs() {
        let producer = plain_step("github", Action::GithubListRepositories);
        let definition = definition_for_action(&producer.action);
        let mut scope = GraphScopeState::default();
        scope.values.insert(
            ContextScope::Step {
                step_id: producer.id.clone(),
            },
            ContextValue::new(
                serde_json::json!({
                    "github": {
                        "account": { "login": "octocat" },
                        "repositories": [{
                            "id": "42",
                            "owner": "octocat",
                            "name": "hello-world",
                            "full_name": "octocat/hello-world",
                            "https_url": "https://github.com/octocat/hello-world.git",
                            "ssh_url": "git@github.com:octocat/hello-world.git",
                            "default_branch": "main",
                            "private": false,
                            "archived": false
                        }]
                    }
                }),
                ContextProvenance::step(&producer.id),
            )
            .with_schema(definition.output_schema.clone()),
        );
        scope.schemas.insert(
            ContextScope::Step {
                step_id: producer.id.clone(),
            },
            ContextValue::new(
                serde_json::Value::Null,
                ContextProvenance::step(&producer.id),
            )
            .with_schema(definition.output_schema),
        );
        let consumer = ActionNode {
            step: plain_step(
                "inspect",
                Action::GitInspect {
                    repo: "https://github.com/example/repository.git".into(),
                    dest: "/tmp/ppduster-missing-positional-binding".into(),
                },
            ),
            bindings: BTreeMap::from([(
                "repo".into(),
                Binding::field(
                    FieldRef::step("github")
                        .field("github")
                        .field("repositories")
                        .index(2)
                        .field("https_url"),
                ),
            )]),
        };
        let task = graph_task(one_action_graph(
            consumer.step.clone(),
            consumer.bindings.clone(),
        ));
        let opts = RunOptions {
            apply: true,
            ..RunOptions::default()
        };
        let mut runtime = GraphRuntime {
            task: &task,
            opts: &opts,
            terminal_interactive: false,
            accumulator: GraphRunAccumulator::default(),
            budget: GraphExecutionBudget::default(),
        };

        let result = runtime.execute_action(&consumer, &mut scope, "").unwrap();

        assert!(result.failed);
        assert_eq!(runtime.accumulator.steps.len(), 1);
        assert!(matches!(
            runtime.accumulator.steps[0].status,
            StepStatus::Failed
        ));
        assert!(runtime.accumulator.steps[0].output.is_none());
        assert!(runtime
            .accumulator
            .errors
            .iter()
            .any(|error| error.contains("binding failed")));
    }

    #[test]
    fn linear_task_with_bindings_dispatches_through_the_graph_runner() {
        let temp = tempfile::tempdir().unwrap();
        let inspected = temp.path().join("repository");
        fs::create_dir(&inspected).unwrap();
        let source = plain_step(
            "source",
            Action::InspectPath(InspectPathAction {
                path: inspected.to_string_lossy().into_owned(),
                recursive_size: false,
                sha256: false,
                expect: None,
            }),
        );
        let mut consumer = plain_step(
            "consumer",
            Action::InspectPath(InspectPathAction {
                path: "/tmp/ppduster-binding-placeholder".into(),
                recursive_size: false,
                sha256: false,
                expect: None,
            }),
        );
        consumer.bindings.insert(
            "path".into(),
            Binding::field(FieldRef::step("source").field("path")),
        );
        let mut task = base_task(source);
        task.steps.push(consumer);

        let report = apply_test_task(&task);

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.steps.len(), 2);
        let path = report
            .context
            .resolve(&FieldRef::step("consumer").field("path"))
            .unwrap();
        assert_eq!(path.value, inspected.to_string_lossy().as_ref());
    }

    #[test]
    fn linear_for_each_dispatches_each_loop_item_and_skips_an_empty_body() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("produce-exit-codes.sh");
        fs::write(&script, "exit 0\n").unwrap();
        let inspected = temp.path().join("payload.txt");
        fs::write(&inspected, "0123456789").unwrap();

        let mut producer = plain_step(
            "producer",
            Action::RunScript {
                interpreter: ScriptInterpreter::Sh,
                script: script.to_string_lossy().into_owned(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                success_exit_codes: vec![0, 1, 2],
            },
        );
        producer.dangerous = true;
        let loop_step = plain_step(
            "items",
            Action::ForEach {
                source_step: "producer".into(),
                array_path: "success_exit_codes".into(),
                item: "code".into(),
                fields: Vec::new(),
            },
        );
        let mut consumer = plain_step(
            "consumer",
            Action::InspectPath(InspectPathAction {
                path: inspected.to_string_lossy().into_owned(),
                recursive_size: true,
                sha256: false,
                expect: Some(PathExpectation {
                    exists: Some(true),
                    kind: Some(PathKind::File),
                    min_size_bytes: Some(0),
                    ..PathExpectation::default()
                }),
            }),
        );
        consumer.bindings.insert(
            "/expect/min_size_bytes".into(),
            Binding::field(FieldRef::loop_item("items")),
        );
        let mut task = base_task(producer);
        task.steps.extend([loop_step, consumer]);
        let options = RunOptions {
            apply: true,
            allow_shell: true,
            ..RunOptions::default()
        };

        let report = run_imported_task_with_interactivity(&task, &options, false).unwrap();

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let consumer_ids = report
            .steps
            .iter()
            .filter(|step| step.step_id.ends_with("/consumer"))
            .map(|step| step.step_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            consumer_ids,
            vec![
                "items[1]/consumer",
                "items[2]/consumer",
                "items[3]/consumer"
            ]
        );
        assert!(consumer_ids
            .iter()
            .all(|id| id.starts_with("items[") && id.ends_with("]/consumer")));
        assert!(report.steps.iter().any(|step| {
            step.step_id == "items"
                && step
                    .summary
                    .contains("completed 3 of 3 iteration(s); 0 failed")
        }));

        let migrated = LegacyTaskImporter::import_steps(&task.steps).unwrap();
        let GraphNode::ForEach(loop_node) = migrated
            .nodes
            .iter()
            .find(|node| node.id() == "items")
            .unwrap()
        else {
            panic!("expected migrated for-each node")
        };
        let loop_node = loop_node.clone();
        let producer_definition = definition_for_action(&task.steps[0].action);
        let mut empty_scope = GraphScopeState::default();
        let producer_scope = ContextScope::Step {
            step_id: "producer".into(),
        };
        empty_scope.values.insert(
            producer_scope.clone(),
            ContextValue::new(
                serde_json::json!({
                    "exit_code": 0,
                    "termination_signal": null,
                    "accepted": true,
                    "success_exit_codes": []
                }),
                ContextProvenance::step("producer"),
            )
            .with_schema(producer_definition.output_schema.clone()),
        );
        empty_scope.schemas.insert(
            producer_scope,
            ContextValue::new(serde_json::Value::Null, ContextProvenance::step("producer"))
                .with_schema(producer_definition.output_schema),
        );
        let empty_task = graph_task(migrated);
        let mut empty_runtime = GraphRuntime {
            task: &empty_task,
            opts: &options,
            terminal_interactive: false,
            accumulator: GraphRunAccumulator::default(),
            budget: GraphExecutionBudget::default(),
        };

        let empty_result = empty_runtime
            .execute_for_each(&loop_node, &empty_scope, "", 1)
            .unwrap();

        assert!(empty_result.successful);
        assert!(empty_runtime.accumulator.errors.is_empty());
        assert!(!empty_runtime
            .accumulator
            .steps
            .iter()
            .any(|step| step.step_id.ends_with("/consumer")));
        assert!(empty_runtime.accumulator.steps.iter().any(|step| {
            step.step_id == "items"
                && matches!(step.status, StepStatus::Satisfied)
                && step.summary == "collection was empty"
        }));
    }

    #[test]
    fn graph_if_runs_only_the_selected_branch() {
        let then_step = plain_step(
            "then-action",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let else_step = plain_step(
            "else-action",
            Action::RunCommand {
                program: "/usr/bin/false".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let graph = WorkflowGraph {
            entries: vec!["choose".into()],
            nodes: vec![GraphNode::If(IfNode {
                id: "choose".into(),
                condition: ExpressionV1::Literal {
                    value: ExpressionValue::Bool(true),
                },
                then_graph: Box::new(one_action_graph(then_step, BTreeMap::new())),
                else_graph: Some(Box::new(one_action_graph(else_step, BTreeMap::new()))),
            })],
            ..WorkflowGraph::default()
        };

        let task = graph_task(graph);
        let planned = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(
            planned.plans.len(),
            2,
            "planning must expose both conditional branches"
        );

        let report = apply_test_task(&task);

        assert!(report
            .steps
            .iter()
            .any(|step| step.step_id == "choose[then]/then-action"));
        assert!(!report
            .steps
            .iter()
            .any(|step| step.step_id.contains("else-action")));
        assert!(report.errors.is_empty());
    }

    #[test]
    fn graph_switch_runs_the_first_matching_case() {
        let red = plain_step(
            "red-action",
            Action::RunCommand {
                program: "/usr/bin/false".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let green = plain_step(
            "green-action",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let graph = WorkflowGraph {
            entries: vec!["color".into()],
            nodes: vec![GraphNode::Switch(SwitchNode {
                id: "color".into(),
                selector: Binding::literal("green"),
                cases: vec![
                    SwitchCase {
                        id: "red".into(),
                        values: vec![serde_json::json!("red")],
                        graph: Box::new(one_action_graph(red, BTreeMap::new())),
                    },
                    SwitchCase {
                        id: "green".into(),
                        values: vec![serde_json::json!("green")],
                        graph: Box::new(one_action_graph(green, BTreeMap::new())),
                    },
                ],
                default: None,
            })],
            ..WorkflowGraph::default()
        };

        let task = graph_task(graph);
        let planned = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(
            planned.plans.len(),
            2,
            "planning must expose every switch case"
        );

        let report = apply_test_task(&task);

        assert!(report
            .steps
            .iter()
            .any(|step| step.step_id == "color[case:green]/green-action"));
        assert!(!report
            .steps
            .iter()
            .any(|step| step.step_id.contains("red-action")));
        assert!(report.errors.is_empty());
    }

    #[test]
    fn graph_for_each_binds_scalar_and_object_items() {
        let scalar_action = plain_step(
            "scalar-action",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: vec!["placeholder".into()],
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let object_action = plain_step(
            "object-action",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: vec!["placeholder".into()],
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let scalar_loop = GraphNode::ForEach(ForEachNode {
            id: "scalar-loop".into(),
            collection: Binding::literal(serde_json::json!(["one", "two"])),
            item_alias: "item".into(),
            index_alias: None,
            concurrency: 8,
            on_error: LoopFailurePolicy::Stop,
            body: Box::new(one_action_graph(
                scalar_action,
                BTreeMap::from([(
                    "/env/ITEM".into(),
                    Binding::field(FieldRef::loop_item("scalar-loop")),
                )]),
            )),
        });
        let object_loop = GraphNode::ForEach(ForEachNode {
            id: "object-loop".into(),
            collection: Binding::literal(serde_json::json!([
                { "name": "alpha" },
                { "name": "beta" }
            ])),
            item_alias: "repository".into(),
            index_alias: Some("index".into()),
            concurrency: 2,
            on_error: LoopFailurePolicy::Continue,
            body: Box::new(one_action_graph(
                object_action,
                BTreeMap::from([(
                    "/env/ITEM".into(),
                    Binding::field(FieldRef::loop_item("object-loop").field("name")),
                )]),
            )),
        });
        let graph = WorkflowGraph {
            entries: vec!["scalar-loop".into()],
            nodes: vec![scalar_loop, object_loop],
            edges: vec![GraphEdge::new(
                "scalar-loop",
                EdgePort::Completed,
                "object-loop",
            )],
            ..WorkflowGraph::default()
        };

        let report = apply_test_task(&graph_task(graph));

        assert_eq!(
            report
                .steps
                .iter()
                .filter(|step| step.step_id.contains("scalar-action"))
                .count(),
            2
        );
        assert_eq!(
            report
                .steps
                .iter()
                .filter(|step| step.step_id.contains("object-action"))
                .count(),
            2
        );
        assert!(report.errors.is_empty());
    }

    #[test]
    fn graph_failure_port_activates_recovery_action() {
        let fail = plain_step(
            "fail",
            Action::RunCommand {
                program: "/usr/bin/false".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let recover = plain_step(
            "recover",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let graph = WorkflowGraph {
            entries: vec!["fail".into()],
            nodes: vec![
                action_node(fail, BTreeMap::new()),
                action_node(recover, BTreeMap::new()),
            ],
            edges: vec![GraphEdge::new("fail", EdgePort::Failure, "recover")],
            ..WorkflowGraph::default()
        };

        let task = graph_task(graph);
        let planned = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(
            planned.plans.len(),
            2,
            "planning must expose the possible recovery path"
        );

        let report = apply_test_task(&task);

        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(report.steps.iter().any(|step| {
            step.step_id == "recover" && matches!(step.status, StepStatus::Applied)
        }));
        assert!(
            report.errors.is_empty(),
            "an explicitly routed and successfully recovered failure must not fail the task"
        );
    }

    #[test]
    fn graph_join_modes_have_explicit_failure_semantics() {
        let task = graph_task(one_action_graph(
            plain_step(
                "placeholder",
                Action::RunCommand {
                    program: "/usr/bin/true".into(),
                    args: Vec::new(),
                    cwd: None,
                    env: BTreeMap::new(),
                    shell: ShellMode::Forbidden,
                },
            ),
            BTreeMap::new(),
        ));
        let opts = RunOptions {
            apply: true,
            ..RunOptions::default()
        };
        let mut runtime = GraphRuntime {
            task: &task,
            opts: &opts,
            terminal_interactive: false,
            accumulator: GraphRunAccumulator::default(),
            budget: GraphExecutionBudget::default(),
        };
        let mixed = [GraphSignal::Failed, GraphSignal::Successful];
        let failed = [GraphSignal::Failed, GraphSignal::Failed];

        let all = runtime
            .execute_join(
                &JoinNode {
                    id: "all".into(),
                    mode: JoinMode::All,
                },
                &mixed,
                2,
                "",
            )
            .unwrap();
        let any = runtime
            .execute_join(
                &JoinNode {
                    id: "any".into(),
                    mode: JoinMode::Any,
                },
                &failed,
                2,
                "",
            )
            .unwrap();
        let first_successful = runtime
            .execute_join(
                &JoinNode {
                    id: "first".into(),
                    mode: JoinMode::FirstSuccessful,
                },
                &mixed,
                2,
                "",
            )
            .unwrap();
        let no_success = runtime
            .execute_join(
                &JoinNode {
                    id: "none".into(),
                    mode: JoinMode::FirstSuccessful,
                },
                &failed,
                2,
                "",
            )
            .unwrap();

        assert!(all.failed);
        assert!(!any.failed, "any joins on arrival, regardless of status");
        assert!(!first_successful.failed);
        assert!(no_success.failed);
    }

    #[test]
    fn graph_all_join_treats_unselected_diamond_path_as_skipped() {
        let branch = plain_step(
            "branch",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let left = plain_step(
            "left",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let right = plain_step(
            "right",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let after = plain_step(
            "after",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let graph = WorkflowGraph {
            entries: vec!["branch".into()],
            nodes: vec![
                action_node(branch, BTreeMap::new()),
                action_node(left, BTreeMap::new()),
                action_node(right, BTreeMap::new()),
                GraphNode::Join(JoinNode {
                    id: "merge".into(),
                    mode: JoinMode::All,
                }),
                action_node(after, BTreeMap::new()),
            ],
            edges: vec![
                GraphEdge::new("branch", EdgePort::Success, "left"),
                GraphEdge::new("branch", EdgePort::Failure, "right"),
                GraphEdge::new("left", EdgePort::Success, "merge"),
                GraphEdge::new("right", EdgePort::Success, "merge"),
                GraphEdge::new("merge", EdgePort::Completed, "after"),
            ],
            ..WorkflowGraph::default()
        };

        let report = apply_test_task(&graph_task(graph));

        assert!(report
            .steps
            .iter()
            .any(|step| step.step_id == "merge" && step.summary.contains("1 skipped")));
        assert!(report
            .steps
            .iter()
            .any(|step| { step.step_id == "after" && matches!(step.status, StepStatus::Applied) }));
        assert!(!report.steps.iter().any(|step| step.step_id == "right"));
        assert!(report.errors.is_empty());
    }

    #[test]
    fn migrated_when_skip_continues_over_success_edge() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let mut skipped = plain_step(
            "conditional",
            Action::RunCommand {
                program: "/usr/bin/false".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        skipped.when = Some(StepCondition::Path {
            path: missing.to_string_lossy().into_owned(),
            expect: PathExpectation {
                exists: Some(true),
                ..PathExpectation::default()
            },
        });
        let next = plain_step(
            "next",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let graph = LegacyTaskImporter::import_steps(&[skipped, next]).unwrap();

        let report = apply_test_task(&graph_task(graph));

        assert!(matches!(report.steps[0].status, StepStatus::Skipped));
        assert!(report
            .steps
            .iter()
            .any(|step| { step.step_id == "next" && matches!(step.status, StepStatus::Applied) }));
    }

    #[test]
    fn graph_when_is_evaluated_before_unavailable_action_bindings() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let guard = StepCondition::Path {
            path: missing.to_string_lossy().into_owned(),
            expect: PathExpectation {
                exists: Some(true),
                ..PathExpectation::default()
            },
        };
        let mut source = plain_step(
            "source",
            Action::CreateDirectory(CreateDirectoryAction {
                path: temp.path().join("source").to_string_lossy().into_owned(),
            }),
        );
        source.when = Some(guard.clone());
        let mut consumer = plain_step(
            "consumer",
            Action::InspectPath(InspectPathAction {
                path: temp.path().join("consumer").to_string_lossy().into_owned(),
                recursive_size: false,
                sha256: false,
                expect: None,
            }),
        );
        consumer.when = Some(guard);
        let graph = WorkflowGraph {
            entries: vec!["source".into()],
            nodes: vec![
                action_node(source, BTreeMap::new()),
                action_node(
                    consumer,
                    BTreeMap::from([(
                        "recursive_size".into(),
                        Binding::field(FieldRef::step("source").field("path").field("created")),
                    )]),
                ),
            ],
            edges: vec![GraphEdge::new("source", EdgePort::Success, "consumer")],
            ..WorkflowGraph::default()
        };

        let report = apply_test_task(&graph_task(graph));

        assert_eq!(report.steps.len(), 2);
        assert!(report
            .steps
            .iter()
            .all(|step| matches!(step.status, StepStatus::Skipped)));
        assert!(report.errors.is_empty());
    }

    #[test]
    fn foreach_preflights_all_items_before_first_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let safe = temp.path().join("safe");
        let unsafe_path = "/System/ppduster-graph-preflight-must-not-exist";
        let action = plain_step(
            "mkdir",
            Action::CreateDirectory(CreateDirectoryAction {
                path: temp
                    .path()
                    .join("placeholder")
                    .to_string_lossy()
                    .into_owned(),
            }),
        );
        let graph = WorkflowGraph {
            entries: vec!["paths".into()],
            nodes: vec![GraphNode::ForEach(ForEachNode {
                id: "paths".into(),
                collection: Binding::literal(serde_json::json!([
                    safe.to_string_lossy(),
                    unsafe_path
                ])),
                item_alias: "path".into(),
                index_alias: None,
                concurrency: 1,
                on_error: LoopFailurePolicy::Stop,
                body: Box::new(one_action_graph(
                    action,
                    BTreeMap::from([(
                        "path".into(),
                        Binding::interpolated([TemplatePart::field(FieldRef::loop_item("paths"))]),
                    )]),
                )),
            })],
            ..WorkflowGraph::default()
        };

        let error = run_task(
            &graph_task(graph),
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("iteration 2 failed preflight"));
        assert!(!safe.exists());
    }

    #[test]
    fn graph_preflights_all_static_destinations_before_first_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let safe = temp.path().join("must-not-exist");
        let first = plain_step(
            "first",
            Action::CreateDirectory(CreateDirectoryAction {
                path: safe.to_string_lossy().into_owned(),
            }),
        );
        let blocked = plain_step(
            "blocked",
            Action::CreateDirectory(CreateDirectoryAction {
                path: "/System/ppduster-graph-global-preflight-must-not-exist".into(),
            }),
        );
        let graph = WorkflowGraph {
            entries: vec!["first".into()],
            nodes: vec![
                action_node(first, BTreeMap::new()),
                action_node(blocked, BTreeMap::new()),
            ],
            edges: vec![GraphEdge::new("first", EdgePort::Success, "blocked")],
            ..WorkflowGraph::default()
        };

        let error = run_task(
            &graph_task(graph),
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("blocked by safety"));
        assert!(
            !safe.exists(),
            "global graph preflight must run before the first mutation"
        );
    }

    #[test]
    fn graph_rejects_late_dynamic_safety_input_before_earlier_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let safe = temp.path().join("must-not-exist");
        let first = plain_step(
            "first",
            Action::CreateDirectory(CreateDirectoryAction {
                path: safe.to_string_lossy().into_owned(),
            }),
        );
        let inspect = plain_step(
            "inspect",
            Action::InspectPath(InspectPathAction {
                path: "/System/ppduster-graph-dynamic-preflight-must-not-exist".into(),
                recursive_size: false,
                sha256: false,
                expect: None,
            }),
        );
        let late = plain_step(
            "late",
            Action::CreateDirectory(CreateDirectoryAction {
                path: temp
                    .path()
                    .join("placeholder")
                    .to_string_lossy()
                    .into_owned(),
            }),
        );
        let graph = WorkflowGraph {
            entries: vec!["first".into()],
            nodes: vec![
                action_node(first, BTreeMap::new()),
                action_node(inspect, BTreeMap::new()),
                action_node(
                    late,
                    BTreeMap::from([(
                        "path".into(),
                        Binding::field(FieldRef::step("inspect").field("path")),
                    )]),
                ),
            ],
            edges: vec![
                GraphEdge::new("first", EdgePort::Success, "inspect"),
                GraphEdge::new("inspect", EdgePort::Success, "late"),
            ],
            ..WorkflowGraph::default()
        };

        let error = run_task(
            &graph_task(graph),
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("runtime-bound input"));
        assert!(
            !safe.exists(),
            "dynamic policy analysis must fail before the earlier action mutates"
        );
    }

    #[test]
    fn graph_rejects_late_dynamic_non_policy_input_before_earlier_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let sentinel = temp.path().join("must-not-exist");
        let first = plain_step(
            "first",
            Action::CreateDirectory(CreateDirectoryAction {
                path: sentinel.to_string_lossy().into_owned(),
            }),
        );
        let repositories = plain_step("repositories", Action::GithubListRepositories);
        let inspect = plain_step(
            "inspect",
            Action::InspectPath(InspectPathAction {
                path: temp
                    .path()
                    .join("repository")
                    .to_string_lossy()
                    .into_owned(),
                recursive_size: false,
                sha256: false,
                expect: None,
            }),
        );
        let graph = WorkflowGraph {
            entries: vec!["first".into()],
            nodes: vec![
                action_node(first, BTreeMap::new()),
                action_node(repositories, BTreeMap::new()),
                action_node(
                    inspect,
                    BTreeMap::from([(
                        "recursive_size".into(),
                        Binding::field(
                            FieldRef::step("repositories")
                                .field("github")
                                .field("repositories")
                                .index(2)
                                .field("private"),
                        ),
                    )]),
                ),
            ],
            edges: vec![
                GraphEdge::new("first", EdgePort::Success, "repositories"),
                GraphEdge::new("repositories", EdgePort::Success, "inspect"),
            ],
            ..WorkflowGraph::default()
        };

        let error = run_task(
            &graph_task(graph),
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("runtime-bound input \"recursive_size\""));
        assert!(
            !sentinel.exists(),
            "all runtime bindings must be preflighted before an earlier mutation"
        );
    }

    #[test]
    fn graph_preflight_allows_dynamic_inputs_until_the_first_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp
            .path()
            .join("repository")
            .to_string_lossy()
            .into_owned();
        let repositories = plain_step("repositories", Action::GithubListRepositories);
        let inspect = plain_step(
            "inspect",
            Action::GitInspect {
                repo: "https://github.com/example/repository.git".into(),
                dest: destination.clone(),
            },
        );
        let clone = plain_step(
            "clone",
            Action::GitCloneIfMissing {
                repo: "https://github.com/example/repository.git".into(),
                dest: destination,
                branch: None,
            },
        );
        let repository_url = || {
            Binding::field(
                FieldRef::step("repositories")
                    .field("github")
                    .field("repositories")
                    .index(2)
                    .field("https_url"),
            )
        };
        let graph = WorkflowGraph {
            entries: vec!["repositories".into()],
            nodes: vec![
                action_node(repositories, BTreeMap::new()),
                action_node(inspect, BTreeMap::from([("repo".into(), repository_url())])),
                action_node(clone, BTreeMap::from([("repo".into(), repository_url())])),
            ],
            edges: vec![
                GraphEdge::new("repositories", EdgePort::Success, "inspect"),
                GraphEdge::new("inspect", EdgePort::Success, "clone"),
            ],
            ..WorkflowGraph::default()
        };
        graph.validate().unwrap();

        preflight_graph_capabilities(
            "test-task",
            &graph,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
            false,
        )
        .unwrap();
    }

    #[test]
    fn graph_dynamic_policy_barrier_reaches_nested_branch() {
        let temp = tempfile::tempdir().unwrap();
        let safe = temp.path().join("must-not-exist");
        let inspect = plain_step(
            "inspect",
            Action::InspectPath(InspectPathAction {
                path: "/System/ppduster-graph-branch-preflight-must-not-exist".into(),
                recursive_size: false,
                sha256: false,
                expect: None,
            }),
        );
        let first = plain_step(
            "first",
            Action::CreateDirectory(CreateDirectoryAction {
                path: safe.to_string_lossy().into_owned(),
            }),
        );
        let late = plain_step(
            "late",
            Action::CreateDirectory(CreateDirectoryAction {
                path: temp
                    .path()
                    .join("placeholder")
                    .to_string_lossy()
                    .into_owned(),
            }),
        );
        let branch = one_action_graph(
            late,
            BTreeMap::from([(
                "path".into(),
                Binding::field(FieldRef::step("inspect").field("path")),
            )]),
        );
        let graph = WorkflowGraph {
            entries: vec!["inspect".into()],
            nodes: vec![
                action_node(inspect, BTreeMap::new()),
                action_node(first, BTreeMap::new()),
                GraphNode::If(IfNode {
                    id: "choose".into(),
                    condition: ExpressionV1::Literal {
                        value: ExpressionValue::Bool(true),
                    },
                    then_graph: Box::new(branch),
                    else_graph: None,
                }),
            ],
            edges: vec![
                GraphEdge::new("inspect", EdgePort::Success, "first"),
                GraphEdge::new("first", EdgePort::Success, "choose"),
            ],
            ..WorkflowGraph::default()
        };

        let error = run_task(
            &graph_task(graph),
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("runtime-bound input"));
        assert!(!safe.exists());
    }

    #[test]
    fn graph_dynamic_policy_barrier_reaches_foreach_body() {
        let temp = tempfile::tempdir().unwrap();
        let safe = temp.path().join("must-not-exist");
        let first = plain_step(
            "first",
            Action::CreateDirectory(CreateDirectoryAction {
                path: safe.to_string_lossy().into_owned(),
            }),
        );
        let late = plain_step(
            "late",
            Action::CreateDirectory(CreateDirectoryAction {
                path: temp
                    .path()
                    .join("placeholder")
                    .to_string_lossy()
                    .into_owned(),
            }),
        );
        let graph = WorkflowGraph {
            entries: vec!["first".into()],
            nodes: vec![
                action_node(first, BTreeMap::new()),
                GraphNode::ForEach(ForEachNode {
                    id: "paths".into(),
                    collection: Binding::literal(serde_json::json!([
                        "/System/ppduster-graph-loop-preflight-must-not-exist"
                    ])),
                    item_alias: "path".into(),
                    index_alias: None,
                    concurrency: 1,
                    on_error: LoopFailurePolicy::Stop,
                    body: Box::new(one_action_graph(
                        late,
                        BTreeMap::from([(
                            "path".into(),
                            Binding::interpolated([TemplatePart::field(FieldRef::loop_item(
                                "paths",
                            ))]),
                        )]),
                    )),
                }),
            ],
            edges: vec![GraphEdge::new("first", EdgePort::Success, "paths")],
            ..WorkflowGraph::default()
        };

        let error = run_task(
            &graph_task(graph),
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("runtime-bound input"));
        assert!(!safe.exists());
    }

    #[test]
    fn foreach_preflight_bounds_nested_expansion_before_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("must-not-exist");
        let action = plain_step(
            "mkdir-expanded",
            Action::CreateDirectory(CreateDirectoryAction {
                path: destination.to_string_lossy().into_owned(),
            }),
        );
        let nested = GraphNode::ForEach(ForEachNode {
            id: "inner".into(),
            collection: Binding::literal(serde_json::Value::Array(vec![serde_json::json!(0); 65])),
            item_alias: "inner_item".into(),
            index_alias: None,
            concurrency: 1,
            on_error: LoopFailurePolicy::Stop,
            body: Box::new(one_action_graph(action, BTreeMap::new())),
        });
        let graph = WorkflowGraph {
            entries: vec!["outer".into()],
            nodes: vec![GraphNode::ForEach(ForEachNode {
                id: "outer".into(),
                collection: Binding::literal(serde_json::Value::Array(vec![
                    serde_json::json!(0);
                    65
                ])),
                item_alias: "outer_item".into(),
                index_alias: None,
                concurrency: 1,
                on_error: LoopFailurePolicy::Stop,
                body: Box::new(WorkflowGraph {
                    entries: vec!["inner".into()],
                    nodes: vec![nested],
                    ..WorkflowGraph::default()
                }),
            })],
            ..WorkflowGraph::default()
        };

        let error = run_task(
            &graph_task(graph),
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("preflight exceeds"));
        assert!(!destination.exists());
    }

    #[test]
    fn github_account_clone_fixture_loads_and_plans_symbolically() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tasks/github-account-clone-v2.yaml");
        let pack = TaskPack::load_many(
            &[TaskSource {
                path,
                trust: PackTrust::Bundled,
            }],
            false,
        )
        .unwrap();
        let task = pack.resolve("github-account-clone-v2").unwrap();

        let report = run_task(&task, &RunOptions::default()).unwrap();

        assert_eq!(report.plans.len(), 2);
        assert!(report
            .steps
            .iter()
            .any(|step| step.step_id == "repositories[*]/clone-repository"));
        assert!(report.steps.iter().any(|step| {
            step.step_id == "repositories[*]/clone-repository"
                && step.summary.contains("deferred until runtime context")
        }));
        assert!(report.errors.is_empty());
    }

    #[test]
    fn create_directory_is_dry_run_safe_recursive_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("state/nested");
        let task = base_task(plain_step(
            "create-state",
            Action::CreateDirectory(CreateDirectoryAction {
                path: destination.to_string_lossy().into_owned(),
            }),
        ));

        let planned = run_task(&task, &RunOptions::default()).unwrap();
        assert!(matches!(planned.outcomes[0], ActionOutcome::Planned { .. }));
        assert!(!destination.exists());

        let applied = apply_test_task(&task);
        assert!(destination.is_dir());
        assert!(matches!(applied.outcomes[0], ActionOutcome::Applied { .. }));

        let sentinel = destination.join("keep.txt");
        fs::write(&sentinel, "keep").unwrap();
        let repeated = apply_test_task(&task);
        assert!(matches!(
            repeated.outcomes[0],
            ActionOutcome::AlreadySatisfied { .. }
        ));
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
    }

    #[test]
    fn create_directory_refuses_to_replace_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("occupied");
        fs::write(&destination, "valuable").unwrap();
        let task = base_task(plain_step(
            "create-state",
            Action::CreateDirectory(CreateDirectoryAction {
                path: destination.to_string_lossy().into_owned(),
            }),
        ));

        let error = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(format!("{error:#}").contains("not a directory"));
        assert_eq!(fs::read_to_string(destination).unwrap(), "valuable");
    }

    #[cfg(unix)]
    #[test]
    fn create_directory_refuses_a_symlink_destination() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let task = base_task(plain_step(
            "create-state",
            Action::CreateDirectory(CreateDirectoryAction {
                path: link.to_string_lossy().into_owned(),
            }),
        ));

        let error = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(format!("{error:#}").contains("must not be a symlink"));
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    #[test]
    fn inspect_path_runs_in_dry_run_and_reports_recursive_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(temp.path().join("one.bin"), b"12345").unwrap();
        fs::write(nested.join("two.bin"), b"abc").unwrap();
        let task = base_task(plain_step(
            "inspect",
            Action::InspectPath(InspectPathAction {
                path: temp.path().to_string_lossy().into_owned(),
                recursive_size: true,
                sha256: false,
                expect: Some(PathExpectation {
                    exists: Some(true),
                    kind: Some(PathKind::Directory),
                    empty: Some(false),
                    min_size_bytes: Some(8),
                    max_size_bytes: Some(8),
                    ..PathExpectation::default()
                }),
            }),
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(report.errors.is_empty());
        assert!(report.plans.is_empty());
        assert!(matches!(report.outcomes[0], ActionOutcome::Observed { .. }));
        assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
        let StepOutput::PathMetadata(metadata) =
            report.steps[0].output.as_ref().expect("path output")
        else {
            panic!("expected path metadata output");
        };
        assert!(metadata.exists);
        assert_eq!(metadata.kind, Some(PathKind::Directory));
        assert_eq!(metadata.size_bytes, Some(8));
        assert_eq!(metadata.empty, Some(false));
        assert_eq!(metadata.entry_count, Some(3));
        assert!(metadata.modified_at.is_some());

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["steps"][0]["output"]["type"], "path-metadata");
        assert_eq!(json["steps"][0]["output"]["value"]["size_bytes"], 8);
    }

    #[test]
    fn inspect_path_can_assert_absence_without_failing() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let task = base_task(plain_step(
            "inspect-missing",
            Action::InspectPath(InspectPathAction {
                path: missing.to_string_lossy().into_owned(),
                recursive_size: true,
                sha256: false,
                expect: Some(PathExpectation {
                    exists: Some(false),
                    ..PathExpectation::default()
                }),
            }),
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(report.errors.is_empty());
        let StepOutput::PathMetadata(metadata) = report.steps[0].output.as_ref().unwrap() else {
            panic!("expected path metadata output");
        };
        assert!(!metadata.exists);
        assert_eq!(metadata.kind, None);
        assert_eq!(metadata.size_bytes, None);
        assert_eq!(metadata.modified_at, None);
    }

    #[test]
    fn failed_path_expectation_does_not_activate_later_steps_in_dry_run() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let mut task = base_task(plain_step(
            "require-file",
            Action::InspectPath(InspectPathAction {
                path: missing.to_string_lossy().into_owned(),
                recursive_size: false,
                sha256: false,
                expect: Some(PathExpectation {
                    exists: Some(true),
                    kind: Some(PathKind::File),
                    ..PathExpectation::default()
                }),
            }),
        ));
        task.steps.push(plain_step(
            "later",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("expected exists"));
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(!report.steps.iter().any(|step| step.step_id == "later"));
    }

    #[test]
    fn path_timestamp_expectations_are_inclusive() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("state.txt");
        fs::write(&file, "state").unwrap();
        let initial = inspect_path(&InspectPathAction {
            path: file.to_string_lossy().into_owned(),
            recursive_size: false,
            sha256: false,
            expect: None,
        })
        .unwrap();
        let modified = initial.modified_at.unwrap();
        let task = base_task(plain_step(
            "inspect-time",
            Action::InspectPath(InspectPathAction {
                path: file.to_string_lossy().into_owned(),
                recursive_size: false,
                sha256: false,
                expect: Some(PathExpectation {
                    modified_at_or_after: Some(modified),
                    modified_at_or_before: Some(modified),
                    ..PathExpectation::default()
                }),
            }),
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(report.errors.is_empty());
        assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
    }

    #[test]
    fn inspect_path_reports_and_verifies_sha256_in_dry_run() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("payload.txt");
        fs::write(&file, b"abc").unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let task = base_task(plain_step(
            "inspect-hash",
            Action::InspectPath(InspectPathAction {
                path: file.to_string_lossy().into_owned(),
                recursive_size: false,
                sha256: true,
                expect: Some(PathExpectation {
                    kind: Some(PathKind::File),
                    sha256: Some(expected.into()),
                    ..PathExpectation::default()
                }),
            }),
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();
        let Some(StepOutput::PathMetadata(metadata)) = report.steps[0].output.as_ref() else {
            panic!("expected path metadata output");
        };
        assert_eq!(metadata.sha256.as_deref(), Some(expected));
        assert!(matches!(report.outcomes[0], ActionOutcome::Observed { .. }));
    }

    #[test]
    fn write_and_copy_files_are_dry_run_safe_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("copy.txt");
        let mut task = base_task(plain_step(
            "write",
            Action::WriteFile(WriteFileAction {
                path: source.to_string_lossy().into_owned(),
                content: "exact content\n".into(),
                on_conflict: WriteConflictPolicy::Fail,
            }),
        ));
        task.steps.push(plain_step(
            "copy",
            Action::CopyPath(CopyPathAction {
                src: source.to_string_lossy().into_owned(),
                dest: destination.to_string_lossy().into_owned(),
            }),
        ));

        let planned = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(planned.plans.len(), 2);
        assert!(!source.exists());
        assert!(!destination.exists());

        let applied = apply_test_task(&task);
        assert_eq!(fs::read_to_string(&source).unwrap(), "exact content\n");
        assert_eq!(fs::read_to_string(&destination).unwrap(), "exact content\n");
        assert!(applied
            .outcomes
            .iter()
            .all(|outcome| matches!(outcome, ActionOutcome::Applied { .. })));

        let repeated = apply_test_task(&task);
        assert!(repeated
            .outcomes
            .iter()
            .all(|outcome| matches!(outcome, ActionOutcome::AlreadySatisfied { .. })));
    }

    #[test]
    fn write_file_conflict_is_preserved_unless_replace_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.txt");
        fs::write(&path, "valuable").unwrap();
        let mut step = plain_step(
            "write",
            Action::WriteFile(WriteFileAction {
                path: path.to_string_lossy().into_owned(),
                content: "replacement".into(),
                on_conflict: WriteConflictPolicy::Fail,
            }),
        );
        let report = run_task(
            &base_task(step.clone()),
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("different content")));
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert_eq!(fs::read_to_string(&path).unwrap(), "valuable");

        let Action::WriteFile(action) = &mut step.action else {
            unreachable!()
        };
        action.on_conflict = WriteConflictPolicy::Replace;
        let report = apply_test_task(&base_task(step));
        assert!(matches!(report.outcomes[0], ActionOutcome::Applied { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
    }

    #[test]
    fn copy_directory_preserves_tree_and_rejects_different_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir_all(source.join("nested/empty")).unwrap();
        fs::write(source.join("nested/data.bin"), b"payload").unwrap();
        let task = base_task(plain_step(
            "copy-tree",
            Action::CopyPath(CopyPathAction {
                src: source.to_string_lossy().into_owned(),
                dest: destination.to_string_lossy().into_owned(),
            }),
        ));

        apply_test_task(&task);
        assert_eq!(
            fs::read(destination.join("nested/data.bin")).unwrap(),
            b"payload"
        );
        assert!(destination.join("nested/empty").is_dir());
        assert!(matches!(
            apply_test_task(&task).outcomes[0],
            ActionOutcome::AlreadySatisfied { .. }
        ));

        fs::write(destination.join("extra.txt"), "different").unwrap();
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("different content")));
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert_eq!(
            fs::read_to_string(destination.join("extra.txt")).unwrap(),
            "different"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_rejects_symlinks_in_source_tree() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(temp.path().join("outside.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(temp.path().join("outside.txt"), source.join("link")).unwrap();
        let task = base_task(plain_step(
            "copy-tree",
            Action::CopyPath(CopyPathAction {
                src: source.to_string_lossy().into_owned(),
                dest: destination.to_string_lossy().into_owned(),
            }),
        ));

        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("does not follow or copy symlinks")));
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(!destination.exists());
    }

    #[test]
    fn composite_when_skips_and_require_failure_halts() {
        let temp = tempfile::tempdir().unwrap();
        let skipped_path = temp.path().join("skipped.txt");
        let required_path = temp.path().join("required.txt");
        let later_path = temp.path().join("later.txt");
        let mut skipped = plain_step(
            "skip",
            Action::WriteFile(WriteFileAction {
                path: skipped_path.to_string_lossy().into_owned(),
                content: "no".into(),
                on_conflict: WriteConflictPolicy::Fail,
            }),
        );
        skipped.when = Some(StepCondition::Not {
            condition: Box::new(StepCondition::Path {
                path: temp.path().to_string_lossy().into_owned(),
                expect: PathExpectation {
                    exists: Some(true),
                    kind: Some(PathKind::Directory),
                    ..PathExpectation::default()
                },
            }),
        });
        let mut required = plain_step(
            "require",
            Action::WriteFile(WriteFileAction {
                path: required_path.to_string_lossy().into_owned(),
                content: "no".into(),
                on_conflict: WriteConflictPolicy::Fail,
            }),
        );
        required.require = Some(StepCondition::All {
            conditions: vec![
                StepCondition::Any {
                    conditions: vec![
                        StepCondition::Path {
                            path: temp.path().join("missing-a").to_string_lossy().into_owned(),
                            expect: PathExpectation {
                                exists: Some(true),
                                ..PathExpectation::default()
                            },
                        },
                        StepCondition::Path {
                            path: temp.path().to_string_lossy().into_owned(),
                            expect: PathExpectation {
                                kind: Some(PathKind::Directory),
                                ..PathExpectation::default()
                            },
                        },
                    ],
                },
                StepCondition::Path {
                    path: temp.path().join("missing-b").to_string_lossy().into_owned(),
                    expect: PathExpectation {
                        exists: Some(true),
                        ..PathExpectation::default()
                    },
                },
            ],
        });
        let later = plain_step(
            "later",
            Action::WriteFile(WriteFileAction {
                path: later_path.to_string_lossy().into_owned(),
                content: "no".into(),
                on_conflict: WriteConflictPolicy::Fail,
            }),
        );
        let mut task = base_task(skipped);
        task.steps.push(required);
        task.steps.push(later);

        let report = apply_test_task(&task);
        assert!(matches!(report.steps[0].status, StepStatus::Skipped));
        assert!(matches!(report.steps[1].status, StepStatus::Failed));
        assert!(!report.steps.iter().any(|step| step.step_id == "later"));
        assert_eq!(report.errors.len(), 1);
        assert!(!skipped_path.exists());
        assert!(!required_path.exists());
        assert!(!later_path.exists());
    }

    #[test]
    fn remove_path_missing_is_already_satisfied_without_touching_trash() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.txt");
        let task = base_task(plain_step(
            "remove",
            Action::RemovePath(RemovePathAction {
                path: missing.to_string_lossy().into_owned(),
            }),
        ));

        let report = apply_test_task(&task);
        assert!(matches!(
            report.outcomes[0],
            ActionOutcome::AlreadySatisfied { .. }
        ));
    }

    #[test]
    fn remove_path_delegates_once_without_permanent_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("obsolete.txt");
        let moved = temp.path().join("fake-trash.txt");
        fs::write(&source, "recoverable").unwrap();
        let mut calls = 0usize;

        let result = apply_remove_path_with(&source.to_string_lossy(), |path| {
            calls += 1;
            fs::rename(path, &moved).context("fake Trash move")?;
            Ok(())
        })
        .unwrap();

        assert!(matches!(result, ApplyStepResult::Applied(_)));
        assert_eq!(calls, 1);
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(moved).unwrap(), "recoverable");
    }

    #[cfg(unix)]
    #[test]
    fn remove_path_moves_a_final_symlink_without_touching_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("valuable.txt");
        let link = temp.path().join("obsolete-link");
        let moved_link = temp.path().join("fake-trash-link");
        fs::write(&target, "valuable").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        apply_remove_path_with(&link.to_string_lossy(), |path| {
            fs::rename(path, &moved_link).context("fake Trash symlink move")?;
            Ok(())
        })
        .unwrap();

        assert!(fs::symlink_metadata(&link).is_err());
        assert!(fs::symlink_metadata(&moved_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(target).unwrap(), "valuable");
    }

    #[test]
    fn run_task_plans_by_default() {
        let task = base_task(Step {
            id: "download".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::DownloadFile {
                url: "https://example.com/app.tgz".into(),
                dest: "$HOME/Library/Caches/app.tgz".into(),
                checksum: Checksum {
                    sha256: "abc".into(),
                },
            },
        });
        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(report.plans.len(), 1);
        assert!(matches!(report.outcomes[0], ActionOutcome::Planned { .. }));
        assert!(report.plans[0].prerequisites.is_empty());
    }

    #[test]
    fn shell_requires_flag() {
        let task = base_task(Step {
            id: "cmd".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::RunCommand {
                program: "bash".into(),
                args: vec!["-lc".into(), "echo hi".into()],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Allow,
            },
        });
        let err = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(err.to_string().contains("--allow-shell"));
    }

    #[test]
    fn script_requires_shell_flag() {
        let task = base_task(Step {
            id: "script".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::RunScript {
                interpreter: ScriptInterpreter::Sh,
                script: "/tmp/ppduster-script.sh".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                success_exit_codes: vec![0],
            },
        });
        let err = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(err.to_string().contains("--allow-shell"));
    }

    #[cfg(unix)]
    #[test]
    fn run_script_uses_direct_arguments_environment_and_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("probe.sh");
        fs::write(
            &script,
            "printf '%s|%s|%s\\n' \"$1\" \"$PPDUSTER_SCRIPT_TEST\" \"$PWD\" > result.txt\n",
        )
        .unwrap();
        let mut env = BTreeMap::new();
        env.insert("PPDUSTER_SCRIPT_TEST".into(), "environment value".into());

        apply_run_script(
            ScriptInterpreter::Sh,
            &script.to_string_lossy(),
            &["argument value".into()],
            Some(&temp.path().to_string_lossy()),
            &env,
            &[0],
        )
        .unwrap();

        let expected_cwd = temp.path().canonicalize().unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("result.txt")).unwrap(),
            format!(
                "argument value|environment value|{}\n",
                expected_cwd.display()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_script_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.sh");
        let link = temp.path().join("script.sh");
        fs::write(&target, "exit 0\n").unwrap();
        symlink(&target, &link).unwrap();

        let err = apply_run_script(
            ScriptInterpreter::Sh,
            &link.to_string_lossy(),
            &[],
            None,
            &BTreeMap::new(),
            &[0],
        )
        .unwrap_err();
        assert!(err.to_string().contains("must not be a symlink"));
    }

    #[test]
    fn archive_traversal_blocked() {
        let root = PathBuf::from("/tmp/root");
        assert!(extracted_path_is_safe(&root, Path::new("dir/file.txt")));
        assert!(!extracted_path_is_safe(&root, Path::new("../escape.txt")));
    }

    #[test]
    fn download_action_writes_file_and_verifies_checksum() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.bin");
        fs::write(&source, b"hello").unwrap();
        let dest = tmp.path().join("downloaded.bin");
        let checksum = crate::automation::task::Checksum {
            sha256: sha256_file(&source).unwrap(),
        };
        let summary = apply_download_file(
            &format!("file://{}", source.display()),
            &dest.to_string_lossy(),
            &checksum,
        )
        .unwrap();
        assert!(dest.exists());
        assert!(summary.contains("downloaded"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "hello");
    }

    #[test]
    fn archive_action_extracts_only_safe_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("archive.tar");
        let dest = tmp.path().join("out");
        fs::write(tmp.path().join("payload.txt"), b"payload").unwrap();
        let status = Command::new("tar")
            .args([
                "-cf",
                &archive.to_string_lossy(),
                "-C",
                &tmp.path().to_string_lossy(),
                "payload.txt",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let summary = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Auto,
            1024 * 1024,
        )
        .unwrap();
        assert!(summary.contains("extracted"));
        assert!(dest.join("payload.txt").exists());
    }

    #[test]
    fn zip_archive_is_detected_and_extracted() {
        use zip::write::SimpleFileOptions;

        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("payload.zip");
        let dest = tmp.path().join("out");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file(
                "bin/tool",
                SimpleFileOptions::default().unix_permissions(0o755),
            )
            .unwrap();
        writer.write_all(b"tool").unwrap();
        writer.finish().unwrap();

        apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Auto,
            1024,
        )
        .unwrap();
        assert_eq!(fs::read(dest.join("bin/tool")).unwrap(), b"tool");
    }

    #[test]
    fn zip_traversal_and_existing_destinations_are_rejected_atomically() {
        use zip::write::SimpleFileOptions;

        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("unsafe.zip");
        let dest = tmp.path().join("out");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("../escape.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"escape").unwrap();
        writer.finish().unwrap();

        let err = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Zip,
            1024,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsafe path"));
        assert!(!tmp.path().join("escape.txt").exists());
        assert!(!dest.exists());

        fs::create_dir(&dest).unwrap();
        let err = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Zip,
            1024,
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing to merge or overwrite"));
    }

    #[test]
    fn archive_unpacked_size_limit_is_enforced() {
        use zip::write::SimpleFileOptions;

        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("large.zip");
        let dest = tmp.path().join("out");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("payload.bin", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&[0u8; 32]).unwrap();
        writer.finish().unwrap();

        let err = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Auto,
            16,
        )
        .unwrap_err();
        assert!(err.to_string().contains("max_unpacked_bytes"));
        assert!(!dest.exists());
    }

    #[test]
    fn archive_symlinks_are_rejected() {
        use zip::write::SimpleFileOptions;

        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("link.zip");
        let dest = tmp.path().join("out");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .add_symlink("link", "../outside", SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap();

        let err = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Auto,
            1024,
        )
        .unwrap_err();
        assert!(err.to_string().contains("links and special files"));
        assert!(!dest.exists());
    }

    fn test_tar_bytes() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let payload = b"compressed";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "payload.txt", &payload[..])
            .unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn compressed_tar_formats_are_auto_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let tar_bytes = test_tar_bytes();

        let gzip_path = tmp.path().join("payload.tgz");
        let mut gzip = flate2::write::GzEncoder::new(
            File::create(&gzip_path).unwrap(),
            flate2::Compression::default(),
        );
        gzip.write_all(&tar_bytes).unwrap();
        gzip.finish().unwrap();

        let bzip_path = tmp.path().join("payload.tar.bz2");
        let mut bzip = bzip2::write::BzEncoder::new(
            File::create(&bzip_path).unwrap(),
            bzip2::Compression::default(),
        );
        bzip.write_all(&tar_bytes).unwrap();
        bzip.finish().unwrap();

        let xz_path = tmp.path().join("payload.txz");
        let mut xz = xz2::write::XzEncoder::new(File::create(&xz_path).unwrap(), 6);
        xz.write_all(&tar_bytes).unwrap();
        xz.finish().unwrap();

        for (index, archive) in [gzip_path, bzip_path, xz_path].iter().enumerate() {
            let dest = tmp.path().join(format!("out-{index}"));
            apply_extract_archive(
                &archive.to_string_lossy(),
                &dest.to_string_lossy(),
                ArchiveFormat::Auto,
                1024,
            )
            .unwrap();
            assert_eq!(fs::read(dest.join("payload.txt")).unwrap(), b"compressed");
        }
    }

    #[test]
    fn satisfied_non_git_step_reports_reason() {
        let task = base_task(Step {
            id: "download".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: Some(crate::automation::task::Check {
                path_exists: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
                command_succeeds: None,
            }),
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::DownloadFile {
                url: "https://example.com/release.bin".into(),
                dest: "$HOME/Library/Caches/release.bin".into(),
                checksum: Checksum {
                    sha256: "abc".into(),
                },
            },
        });
        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(report.plans.is_empty());
        match &report.outcomes[0] {
            ActionOutcome::AlreadySatisfied { reason } => {
                assert!(reason.contains("path exists"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn git_repository_is_cloned_checked_and_fast_forwarded_with_clear_reports() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);

        let cloned = apply_test_task(&task);
        assert!(matches!(cloned.steps[0].status, StepStatus::Applied));
        assert!(cloned.steps[0].summary.contains("repository was absent"));
        assert!(cloned.steps[0].summary.contains("main at"));
        assert!(matches!(
            &cloned.outcomes[0],
            ActionOutcome::Applied { summary } if summary == &cloned.steps[0].summary
        ));
        assert_eq!(
            cloned.steps[0].logs.last().unwrap().message,
            cloned.steps[0].summary
        );

        let current_sha = test_git(&destination, &["rev-parse", "HEAD"]);
        let current = apply_test_task(&task);
        assert!(matches!(current.steps[0].status, StepStatus::Satisfied));
        assert!(current.steps[0]
            .summary
            .contains("repository already existed"));
        assert!(current.steps[0]
            .summary
            .contains("main branch ref was already up to date"));
        assert!(matches!(
            &current.outcomes[0],
            ActionOutcome::AlreadySatisfied { reason } if reason == &current.steps[0].summary
        ));
        assert_eq!(
            current.steps[0].logs.last().unwrap().message,
            current.steps[0].summary
        );
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), current_sha);

        let remote_sha = push_git_test_commit(&repository, "remote update\n");
        let updated = apply_test_task(&task);
        assert!(matches!(updated.steps[0].status, StepStatus::Applied));
        assert!(updated.steps[0]
            .summary
            .contains("repository already existed"));
        assert!(updated.steps[0].summary.contains("main was outdated"));
        assert!(updated.steps[0].summary.contains("was updated"));
        assert!(matches!(
            &updated.outcomes[0],
            ActionOutcome::Applied { summary } if summary == &updated.steps[0].summary
        ));
        assert_eq!(
            updated.steps[0].logs.last().unwrap().message,
            updated.steps[0].summary
        );
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), remote_sha);
        assert_eq!(
            fs::read_to_string(destination.join("state.txt")).unwrap(),
            "remote update\n"
        );
        assert!(test_git(
            &destination,
            &["status", "--porcelain=v1", "--untracked-files=normal"]
        )
        .is_empty());
    }

    #[test]
    fn atomic_git_steps_report_inspect_clone_fetch_and_fast_forward_separately() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("atomic-checkout");
        let task = atomic_git_sync_task(&repository.remote, &destination);

        let cloned = apply_test_task(&task);
        assert_eq!(cloned.steps.len(), 4);
        assert!(matches!(cloned.steps[0].status, StepStatus::Satisfied));
        assert!(cloned.steps[0].summary.contains("absent"));
        assert!(matches!(cloned.steps[1].status, StepStatus::Applied));
        assert!(matches!(cloned.steps[2].status, StepStatus::Satisfied));
        assert!(matches!(cloned.steps[3].status, StepStatus::Satisfied));

        let remote_sha = push_git_test_commit(&repository, "atomic remote update\n");
        let updated = apply_test_task(&task);
        assert!(matches!(updated.steps[0].status, StepStatus::Satisfied));
        assert!(updated.steps[0]
            .summary
            .contains("expected repository exists"));
        assert!(matches!(updated.steps[1].status, StepStatus::Satisfied));
        assert!(matches!(updated.steps[2].status, StepStatus::Applied));
        assert!(matches!(updated.steps[3].status, StepStatus::Applied));
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), remote_sha);
    }

    #[test]
    fn git_inspect_context_distinguishes_absent_empty_and_verified_repository() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("inspect-checkout");
        let task = base_task(plain_step(
            "inspect-repository",
            Action::GitInspect {
                repo: repository.remote.to_string_lossy().into_owned(),
                dest: destination.to_string_lossy().into_owned(),
            },
        ));
        let exists_ref = FieldRef::step("inspect-repository")
            .field("repository")
            .field("exists");

        let absent = apply_test_task(&task);
        let Some(StepOutput::Structured(output)) = absent.steps[0].output.as_ref() else {
            panic!("expected structured git inspection output");
        };
        assert_eq!(output.schema_id, "ppduster.git.inspect@1");
        assert_eq!(output.value["repository"]["exists"], false);
        assert_eq!(absent.context.resolve(&exists_ref).unwrap().value, false);

        fs::create_dir(&destination).unwrap();
        let empty = apply_test_task(&task);
        let Some(StepOutput::Structured(output)) = empty.steps[0].output.as_ref() else {
            panic!("expected structured git inspection output");
        };
        assert!(empty.steps[0].summary.contains("empty directory"));
        assert_eq!(output.value["repository"]["exists"], false);
        assert_eq!(empty.context.resolve(&exists_ref).unwrap().value, false);

        apply_test_task(&git_sync_task(&repository.remote, &destination));
        let verified = apply_test_task(&task);
        let Some(StepOutput::Structured(output)) = verified.steps[0].output.as_ref() else {
            panic!("expected structured git inspection output");
        };
        assert!(verified.steps[0]
            .summary
            .contains("expected repository exists"));
        assert_eq!(output.value["repository"]["exists"], true);
        assert_eq!(output.value["repository"]["branch"], "main");
        assert_eq!(verified.context.resolve(&exists_ref).unwrap().value, true);
    }

    #[test]
    fn git_repository_refuses_to_overwrite_a_non_repository_destination() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep.txt"), "keep me\n").unwrap();

        let report = apply_test_task(&git_sync_task(&repository.remote, &destination));
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("git repository"));
        assert!(report.steps[0]
            .logs
            .last()
            .unwrap()
            .message
            .contains("failed"));
        assert_eq!(
            fs::read_to_string(destination.join("keep.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn git_repository_clones_into_an_existing_empty_destination() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("empty-checkout");
        fs::create_dir(&destination).unwrap();

        let report = apply_test_task(&git_sync_task(&repository.remote, &destination));
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(report.steps[0].summary.contains("repository was absent"));
        assert!(destination.join(".git").is_dir());
        assert_eq!(
            test_git(&destination, &["rev-parse", "HEAD"]),
            test_git(&repository.seed, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn git_repository_rejects_a_mismatched_origin_without_moving_head() {
        let actual = init_git_test_repository();
        let requested = init_git_test_repository();
        let destination = actual._temp.path().join("checkout");
        apply_test_task(&git_sync_task(&actual.remote, &destination));
        let original_head = test_git(&destination, &["rev-parse", "HEAD"]);
        let original_origin = test_git(&destination, &["remote", "get-url", "origin"]);

        let report = apply_test_task(&git_sync_task(&requested.remote, &destination));
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("origin that does not match"));
        assert_eq!(
            test_git(&destination, &["rev-parse", "HEAD"]),
            original_head
        );
        assert_eq!(
            test_git(&destination, &["remote", "get-url", "origin"]),
            original_origin
        );
    }

    #[test]
    fn git_remote_identity_accepts_github_transport_changes_but_not_distinct_local_paths() {
        assert_eq!(
            normalize_git_remote("https://github.com/OpenAI/example.git"),
            normalize_git_remote("git@github.com:openai/example.git")
        );
        assert_eq!(
            normalize_git_remote("ssh://git@github.com:22/OpenAI/example.git"),
            normalize_git_remote("https://github.com/openai/example")
        );
        assert_ne!(
            normalize_git_remote("/tmp/example"),
            normalize_git_remote("/tmp/example.git")
        );
    }

    #[test]
    fn git_inspect_allows_a_read_only_path_under_documents() {
        let destination = dirs::home_dir()
            .unwrap()
            .join("Documents")
            .join(format!(
                ".ppduster-git-inspect-policy-test-{}",
                std::process::id()
            ))
            .join("repository");
        assert!(!is_safe_rule_root(parent_or_self(&destination)));
        let task = base_task(plain_step(
            "inspect-repository",
            Action::GitInspect {
                repo: "https://github.com/example/repository.git".into(),
                dest: destination.to_string_lossy().into_owned(),
            },
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();

        assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
        assert!(report.steps[0].summary.contains("repository is absent"));
        assert!(!destination.exists());
    }

    #[test]
    fn mutating_git_action_remains_blocked_under_documents() {
        let destination = dirs::home_dir()
            .unwrap()
            .join("Documents")
            .join(format!(
                ".ppduster-git-mutation-policy-test-{}",
                std::process::id()
            ))
            .join("repository");
        assert!(!is_safe_rule_root(parent_or_self(&destination)));
        let step = plain_step(
            "clone-repository",
            Action::GitCloneIfMissing {
                repo: "https://github.com/example/repository.git".into(),
                dest: destination.to_string_lossy().into_owned(),
                branch: None,
            },
        );

        let error = validate_destinations("test-task", &step, &RunOptions::default()).unwrap_err();

        assert!(
            format!("{error:#}").contains("blocked by safety"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn documents_git_approval_is_typed_exact_and_runtime_only() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let documents = home.join("Documents");
        if !documents.is_dir() {
            return;
        }
        let destination = documents.join(format!(
            ".ppduster-protected-approval-test-{}",
            std::process::id()
        ));
        if destination.exists() {
            return;
        }
        let repository = "https://github.com/example/repository.git";
        let step = plain_step(
            "clone-repository",
            Action::GitCloneIfMissing {
                repo: repository.into(),
                dest: destination.to_string_lossy().into_owned(),
                branch: Some("main".into()),
            },
        );
        let task = base_task(step.clone());

        let error = run_task(&task, &RunOptions::default()).unwrap_err();
        let required = error
            .downcast_ref::<ProtectedPathApprovalRequired>()
            .expect("protected destination must remain a typed error through anyhow contexts");
        let request = required.request();
        assert_eq!(request.task_id(), task.id);
        assert_eq!(request.step_id(), step.id);
        assert_eq!(
            request.operation(),
            ProtectedPathOperation::GitCloneIfMissing
        );
        assert_eq!(request.expected_repository(), repository);
        assert_eq!(request.expected_branch(), Some("main"));
        assert_eq!(request.requested_path(), destination);
        assert_eq!(request.resolved_path(), destination);
        assert_eq!(request.risk(), ProtectedPathRisk::UserDocuments);

        let approval = request.approve().unwrap();
        let options = RunOptions {
            protected_path_approvals: vec![approval.clone()],
            ..RunOptions::default()
        };
        let report = run_task(&task, &options).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!destination.exists(), "dry-run consent must not write");

        let sibling = destination.with_file_name(format!(
            "{}-sibling",
            destination.file_name().unwrap().to_string_lossy()
        ));
        let sibling_task = base_task(plain_step(
            "clone-repository",
            Action::GitCloneIfMissing {
                repo: repository.into(),
                dest: sibling.to_string_lossy().into_owned(),
                branch: Some("main".into()),
            },
        ));
        let sibling_error = run_task(&sibling_task, &options).unwrap_err();
        assert!(sibling_error
            .downcast_ref::<ProtectedPathApprovalRequired>()
            .is_some());

        let changed_repo_task = base_task(plain_step(
            "clone-repository",
            Action::GitCloneIfMissing {
                repo: "https://github.com/example/different.git".into(),
                dest: destination.to_string_lossy().into_owned(),
                branch: Some("main".into()),
            },
        ));
        let changed_repo_error = run_task(&changed_repo_task, &options).unwrap_err();
        assert!(changed_repo_error
            .downcast_ref::<ProtectedPathApprovalRequired>()
            .is_some());

        let changed_branch_task = base_task(plain_step(
            "clone-repository",
            Action::GitCloneIfMissing {
                repo: repository.into(),
                dest: destination.to_string_lossy().into_owned(),
                branch: Some("release".into()),
            },
        ));
        let changed_branch_error = run_task(&changed_branch_task, &options).unwrap_err();
        assert!(changed_branch_error
            .downcast_ref::<ProtectedPathApprovalRequired>()
            .is_some());

        let changed_operation_task = base_task(plain_step(
            "clone-repository",
            Action::GitClone {
                repo: repository.into(),
                dest: destination.to_string_lossy().into_owned(),
                branch: Some("main".into()),
            },
        ));
        let changed_operation_error = run_task(&changed_operation_task, &options).unwrap_err();
        assert!(changed_operation_error
            .downcast_ref::<ProtectedPathApprovalRequired>()
            .is_some());
    }

    #[test]
    fn runtime_bound_graph_destination_preserves_typed_approval_request() {
        let Some(destination) = protected_documents_test_destination("runtime-bound") else {
            return;
        };
        let repository = "https://github.com/example/repository.git";
        let inspect = plain_step(
            "inspect-destination",
            Action::GitInspect {
                repo: repository.into(),
                dest: destination.to_string_lossy().into_owned(),
            },
        );
        let clone = plain_step(
            "clone-destination",
            Action::GitCloneIfMissing {
                repo: repository.into(),
                dest: std::env::temp_dir()
                    .join("ppduster-runtime-binding-placeholder")
                    .to_string_lossy()
                    .into_owned(),
                branch: Some("main".into()),
            },
        );
        let graph = WorkflowGraph {
            entries: vec![inspect.id.clone()],
            nodes: vec![
                action_node(inspect, BTreeMap::new()),
                action_node(
                    clone,
                    BTreeMap::from([(
                        "dest".into(),
                        Binding::field(
                            FieldRef::step("inspect-destination")
                                .field("repository")
                                .field("path"),
                        ),
                    )]),
                ),
            ],
            edges: vec![GraphEdge::new(
                "inspect-destination",
                EdgePort::Success,
                "clone-destination",
            )],
            ..WorkflowGraph::default()
        };
        graph.validate().unwrap();

        let error = run_task(
            &graph_task(graph),
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap_err();
        let request = error
            .downcast_ref::<ProtectedPathApprovalRequired>()
            .expect("runtime-bound graph approval must not be flattened into a failed report")
            .request();
        assert_eq!(request.step_id(), "clone-destination");
        assert_eq!(request.requested_path(), destination);
        assert_eq!(request.expected_repository(), repository);
        assert_eq!(request.expected_branch(), Some("main"));
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn stale_apply_approval_remains_a_typed_error() {
        let temp = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "automation::runner::tests::stale_apply_approval_subprocess_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("HOME", temp.path())
            .env("PPDUSTER_STALE_APPROVAL_HELPER", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stale approval helper failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess helper with an isolated HOME"]
    fn stale_apply_approval_subprocess_helper() {
        if std::env::var_os("PPDUSTER_STALE_APPROVAL_HELPER").is_none() {
            return;
        }
        let home = dirs::home_dir().unwrap();
        let documents = home.join("Documents");
        let parent = documents.join("approved-parent");
        let moved_parent = documents.join("moved-parent");
        fs::create_dir_all(&parent).unwrap();
        let destination = parent.join("repository");
        let clone = plain_step(
            "clone-after-anchor-change",
            Action::GitCloneIfMissing {
                repo: "https://github.com/example/repository.git".into(),
                dest: destination.to_string_lossy().into_owned(),
                branch: None,
            },
        );
        let mut task = base_task(plain_step(
            "move-approved-parent",
            Action::RunCommand {
                program: "/bin/mv".into(),
                args: vec![
                    parent.to_string_lossy().into_owned(),
                    moved_parent.to_string_lossy().into_owned(),
                ],
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        ));
        task.steps.push(plain_step(
            "replace-approved-parent",
            Action::RunCommand {
                program: "/bin/mkdir".into(),
                args: vec![parent.to_string_lossy().into_owned()],
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        ));
        task.steps.push(clone.clone());
        let approval_error =
            validate_destinations(&task.id, &clone, &RunOptions::default()).unwrap_err();
        let approval = approval_error
            .downcast_ref::<ProtectedPathApprovalRequired>()
            .unwrap()
            .request()
            .approve()
            .unwrap();

        let error = run_step_sequence_with_interactivity(
            &task,
            &task.steps,
            &RunOptions {
                apply: true,
                protected_path_approvals: vec![approval],
                ..RunOptions::default()
            },
            false,
        )
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<ProtectedPathApprovalRequired>()
                .is_some(),
            "stale apply approval was flattened: {error:#}"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn git_destination_snapshot_rejects_relative_and_parent_paths() {
        assert!(capture_git_destination_snapshot(Path::new("relative/repository")).is_err());
        let parent_path = std::env::temp_dir()
            .join("scope")
            .join("..")
            .join("repository");
        assert!(capture_git_destination_snapshot(&parent_path).is_err());
    }

    #[test]
    fn protected_snapshot_detects_replaced_ancestor_identity() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().join("anchor");
        fs::create_dir(&anchor).unwrap();
        let destination = anchor.join("repository");
        let snapshot = capture_git_destination_snapshot(&destination).unwrap();
        let old_anchor = temp.path().join("old-anchor");
        fs::rename(&anchor, &old_anchor).unwrap();
        fs::create_dir(&anchor).unwrap();

        let error = revalidate_destination_snapshot_identity(&snapshot).unwrap_err();
        assert!(format!("{error:#}").contains("identity changed"));
    }

    #[test]
    fn protected_destination_requires_existing_immediate_parent() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("missing-parent").join("repository");
        let error = protected_destination_parent_is_real(&destination).unwrap_err();
        assert!(format!("{error:#}").contains("immediate parent to already exist"));
    }

    #[cfg(unix)]
    #[test]
    fn protected_destination_rejects_every_symlink_component() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Documents");
        let real = root.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        symlink(&real, &link).unwrap();

        let error = reject_symlink_components_below(&root, &link.join("repository")).unwrap_err();
        assert!(format!("{error:#}").contains("symlink components"));
    }

    #[test]
    fn protected_repository_requires_an_in_tree_git_directory() {
        let repository = init_git_test_repository();
        let checkout = repository._temp.path().join("checkout");
        let checkout_arg = checkout.to_string_lossy().into_owned();
        let remote_arg = repository.remote.to_string_lossy().into_owned();
        test_git(
            repository._temp.path(),
            &[
                "clone",
                "--no-recurse-submodules",
                &remote_arg,
                &checkout_arg,
            ],
        );
        validate_existing_git_repository(&checkout, &remote_arg, true).unwrap();

        let linked = repository._temp.path().join("linked-worktree");
        let linked_arg = linked.to_string_lossy().into_owned();
        test_git(
            &repository.seed,
            &["worktree", "add", "--detach", &linked_arg],
        );
        let error = validate_existing_git_repository(&linked, &remote_arg, true).unwrap_err();
        assert!(
            format!("{error:#}").contains("real in-tree .git directory"),
            "unexpected error: {error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn protected_repository_rejects_symlinks_inside_git_metadata() {
        use std::os::unix::fs::symlink;

        let repository = init_git_test_repository();
        let checkout = repository._temp.path().join("symlinked-metadata-checkout");
        let checkout_arg = checkout.to_string_lossy().into_owned();
        let remote_arg = repository.remote.to_string_lossy().into_owned();
        test_git(
            repository._temp.path(),
            &[
                "clone",
                "--no-recurse-submodules",
                &remote_arg,
                &checkout_arg,
            ],
        );
        let refs = checkout.join(".git/refs");
        let external_refs = repository._temp.path().join("external-refs");
        fs::rename(&refs, &external_refs).unwrap();
        symlink(&external_refs, &refs).unwrap();

        let error = validate_existing_git_repository(&checkout, &remote_arg, true).unwrap_err();
        assert!(
            format!("{error:#}").contains("git metadata must not contain symlinks"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn protected_repository_rejects_executable_filter_config_before_git_mutation() {
        let repository = init_git_test_repository();
        let checkout = repository._temp.path().join("filter-checkout");
        let checkout_arg = checkout.to_string_lossy().into_owned();
        let remote_arg = repository.remote.to_string_lossy().into_owned();
        test_git(
            repository._temp.path(),
            &[
                "clone",
                "--no-recurse-submodules",
                &remote_arg,
                &checkout_arg,
            ],
        );
        let sentinel = repository._temp.path().join("filter-executed");
        let filter = format!(
            "sh -c 'touch {}; cat'",
            sentinel.to_string_lossy().replace('\'', "'\\''")
        );
        test_git(&checkout, &["config", "filter.evil.smudge", &filter]);
        fs::write(checkout.join(".gitattributes"), "state.txt filter=evil\n").unwrap();

        let error = validate_existing_git_repository(&checkout, &remote_arg, true).unwrap_err();
        assert!(
            format!("{error:#}").contains("executable Git configuration filter.evil.smudge"),
            "unexpected error: {error:#}"
        );
        assert!(
            !sentinel.exists(),
            "filter must be rejected before any Git mutation executes it"
        );
    }

    #[test]
    fn protected_repository_rejects_local_credential_helper() {
        let repository = init_git_test_repository();
        let checkout = repository._temp.path().join("credential-helper-checkout");
        let checkout_arg = checkout.to_string_lossy().into_owned();
        let remote_arg = repository.remote.to_string_lossy().into_owned();
        test_git(
            repository._temp.path(),
            &[
                "clone",
                "--no-recurse-submodules",
                &remote_arg,
                &checkout_arg,
            ],
        );
        test_git(
            &checkout,
            &["config", "credential.helper", "!echo untrusted-helper"],
        );

        let error = validate_existing_git_repository(&checkout, &remote_arg, true).unwrap_err();
        assert!(
            format!("{error:#}").contains("executable Git configuration credential.helper"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn executable_git_config_key_detection_is_narrow_and_case_insensitive() {
        for key in [
            "filter.evil.clean",
            "FILTER.evil.SMUDGE",
            "filter.evil.process",
            "credential.helper",
            "credential.https://example.com.helper",
            "Core.AskPass",
            "core.fsmonitor",
            "core.gitProxy",
            "Core.SshCommand",
            "remote.origin.uploadpack",
        ] {
            assert!(git_config_key_can_execute_process(key), "missed {key}");
        }
        for key in [
            "filter.evil.required",
            "core.hooksPath",
            "credential.useHttpPath",
            "remote.origin.url",
        ] {
            assert!(
                !git_config_key_can_execute_process(key),
                "overblocked {key}"
            );
        }
    }

    #[test]
    fn git_environment_scrubbing_preserves_auth_and_prompt_controls() {
        for key in [
            "GIT_DIR",
            "git_work_tree",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_INDEX_FILE",
            "GIT_QUARANTINE_PATH",
        ] {
            assert!(
                git_environment_key_can_redirect_operation(key),
                "missed {key}"
            );
        }
        for key in [
            "GIT_TERMINAL_PROMPT",
            "GIT_ASKPASS",
            "GIT_SSH_COMMAND",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_EXEC_PATH",
            "GIT_TEMPLATE_DIR",
            "GIT_TRACE2_EVENT",
            "SSH_AUTH_SOCK",
        ] {
            assert!(
                !git_environment_key_can_redirect_operation(key),
                "auth/prompt regression for {key}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn git_inspect_rejects_a_final_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("repository");
        let link = temp.path().join("repository-link");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();
        let step = plain_step(
            "inspect-repository",
            Action::GitInspect {
                repo: "https://github.com/example/repository.git".into(),
                dest: link.to_string_lossy().into_owned(),
            },
        );

        let error = validate_destinations("test-task", &step, &RunOptions::default()).unwrap_err();

        assert!(
            format!("{error:#}").contains("must not be a symlink"),
            "unexpected error: {error:#}"
        );
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert!(target.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn git_repository_rejects_a_destination_whose_ancestor_resolves_into_documents() {
        use std::os::unix::fs::symlink;

        let repository = init_git_test_repository();
        let link = repository._temp.path().join("destination-link");
        let documents = dirs::home_dir().unwrap().join("Documents");
        symlink(&documents, &link).unwrap();
        let unique_child = format!(
            ".ppduster-git-safety-{}",
            repository
                ._temp
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
        );
        let destination = link.join(&unique_child).join("repository");
        let error = validate_resolved_git_destination(&destination)
            .unwrap_err()
            .to_string();

        assert!(error.contains("not safe"), "unexpected error: {error}");
        assert!(!documents.join(unique_child).exists());
    }

    #[test]
    fn git_repository_refuses_diverged_main_without_losing_local_history() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);

        test_git(&destination, &["config", "user.name", "ppduster tests"]);
        test_git(
            &destination,
            &["config", "user.email", "ppduster-tests@example.invalid"],
        );
        fs::write(destination.join("local.txt"), "local commit\n").unwrap();
        test_git(&destination, &["add", "local.txt"]);
        test_git(&destination, &["commit", "-m", "local commit"]);
        let local_sha = test_git(&destination, &["rev-parse", "HEAD"]);
        push_git_test_commit(&repository, "different remote update\n");

        let report = apply_test_task(&task);
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("diverged"));
        assert!(report.errors[0].contains("left local history unchanged"));
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), local_sha);
    }

    #[test]
    fn git_repository_fetches_but_does_not_overwrite_dirty_outdated_main() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);
        let local_sha = test_git(&destination, &["rev-parse", "HEAD"]);

        fs::write(destination.join("state.txt"), "local work\n").unwrap();
        let remote_sha = push_git_test_commit(&repository, "remote update\n");
        let report = apply_test_task(&task);

        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("was outdated"));
        assert!(report.errors[0].contains("local changes"));
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), local_sha);
        assert_eq!(
            test_git(&destination, &["rev-parse", "refs/remotes/origin/main"]),
            remote_sha
        );
        assert_eq!(
            fs::read_to_string(destination.join("state.txt")).unwrap(),
            "local work\n"
        );
    }

    #[test]
    fn git_repository_reports_local_changes_when_main_ref_is_current() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);
        fs::write(destination.join("untracked.txt"), "local work\n").unwrap();

        let report = apply_test_task(&task);
        assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
        assert!(report.steps[0]
            .summary
            .contains("main branch ref was already up to date"));
        assert!(report.steps[0]
            .summary
            .contains("working tree has local changes that were left untouched"));
        assert_eq!(
            fs::read_to_string(destination.join("untracked.txt")).unwrap(),
            "local work\n"
        );
    }

    #[test]
    fn git_repository_does_not_overwrite_an_ignored_path_during_fast_forward() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);
        let local_sha = test_git(&destination, &["rev-parse", "HEAD"]);

        fs::write(destination.join(".git/info/exclude"), "ignored.txt\n").unwrap();
        fs::write(destination.join("ignored.txt"), "LOCAL SECRET\n").unwrap();
        assert!(test_git(
            &destination,
            &["status", "--porcelain=v1", "--untracked-files=normal"]
        )
        .is_empty());

        fs::write(repository.seed.join("ignored.txt"), "REMOTE CONTENT\n").unwrap();
        test_git(&repository.seed, &["add", "ignored.txt"]);
        test_git(
            &repository.seed,
            &["commit", "-m", "track previously ignored path"],
        );
        test_git(&repository.seed, &["push", "origin", "main"]);
        let remote_sha = test_git(&repository.seed, &["rev-parse", "HEAD"]);

        let report = apply_test_task(&task);
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("ignored path ignored.txt conflicts"));
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), local_sha);
        assert_eq!(
            test_git(&destination, &["rev-parse", "refs/remotes/origin/main"]),
            remote_sha
        );
        assert_eq!(
            fs::read_to_string(destination.join("ignored.txt")).unwrap(),
            "LOCAL SECRET\n"
        );
    }

    #[test]
    fn git_repository_updates_inactive_main_without_switching_the_active_branch() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);

        test_git(&destination, &["checkout", "-b", "feature"]);
        fs::write(destination.join("feature-work.txt"), "unfinished\n").unwrap();
        let feature_sha = test_git(&destination, &["rev-parse", "HEAD"]);
        let remote_sha = push_git_test_commit(&repository, "remote update\n");

        let report = apply_test_task(&task);
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(report.steps[0].summary.contains("main was outdated"));
        assert!(report.steps[0]
            .summary
            .contains("active branch feature was preserved"));
        assert_eq!(
            test_git(&destination, &["symbolic-ref", "--short", "HEAD"]),
            "feature"
        );
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), feature_sha);
        assert_eq!(
            test_git(&destination, &["rev-parse", "refs/heads/main"]),
            remote_sha
        );
        assert_eq!(
            fs::read_to_string(destination.join("feature-work.txt")).unwrap(),
            "unfinished\n"
        );

        let current = apply_test_task(&task);
        assert!(matches!(current.steps[0].status, StepStatus::Satisfied));
        assert!(current.steps[0]
            .summary
            .contains("main branch ref was already up to date"));
        assert!(current.steps[0]
            .summary
            .contains("active branch feature was preserved"));
    }

    #[test]
    fn planning_mode_skips_command_satisfaction_checks() {
        let task = base_task(Step {
            id: "brew".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: Some(crate::automation::task::Check {
                path_exists: None,
                command_succeeds: Some(vec!["true".into()]),
            }),
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::BrewInstall {
                package: "git".into(),
                cask: false,
            },
        });
        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(matches!(report.outcomes[0], ActionOutcome::Planned { .. }));
    }

    #[test]
    fn git_clone_plan_can_require_one_time_git_auth() {
        let task = base_task(Step {
            id: "clone".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::GitCredential,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::GitClone {
                repo: "https://github.com/example/repo.git".into(),
                dest: "$HOME/Library/Caches/repo".into(),
                branch: None,
            },
        });
        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(report.plans[0].prerequisites.len(), 1);
        assert!(report.plans[0].prerequisites[0].contains("git once"));
    }

    #[test]
    fn elevated_plan_can_require_one_time_sudo_auth() {
        let task = base_task(Step {
            id: "remote-login".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::Sudo,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Allow,
            when: None,
            require: None,
            action: Action::RunCommand {
                program: "sudo".into(),
                args: vec!["systemsetup".into(), "-getremotelogin".into()],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Forbidden,
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                allow_elevation: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.plans[0].prerequisites.len(), 1);
        assert!(report.plans[0].prerequisites[0].contains("sudo once"));
    }

    #[test]
    fn apply_mode_downloads_to_a_local_file() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("app.tgz");
        fs::write(&source, b"payload").unwrap();
        let task = base_task(Step {
            id: "download".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::DownloadFile {
                url: format!("file://{}", source.display()),
                dest: tmp
                    .path()
                    .join("downloaded.tgz")
                    .to_string_lossy()
                    .into_owned(),
                checksum: Checksum {
                    sha256: sha256_file(&source).unwrap(),
                },
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(report.errors.is_empty());
    }

    #[test]
    fn planning_mode_keeps_auth_steps_in_order() {
        let task = Task {
            id: "setup-dev".into(),
            name: "Setup dev".into(),
            description: "Test setup scenario.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![
                Step {
                    id: "clone".into(),
                    name: String::new(),
                    bindings: BTreeMap::new(),
                    auth: AuthPolicy::GitCredential,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    when: None,
                    require: None,
                    action: Action::GitClone {
                        repo: "https://github.com/example/repo.git".into(),
                        dest: "$HOME/Library/Caches/repo".into(),
                        branch: None,
                    },
                },
                Step {
                    id: "brew".into(),
                    name: String::new(),
                    bindings: BTreeMap::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    when: None,
                    require: None,
                    action: Action::BrewInstall {
                        package: "git".into(),
                        cask: false,
                    },
                },
            ],
            graph: None,
        };

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(report.plans.len(), 2);
        assert!(matches!(report.outcomes[0], ActionOutcome::Planned { .. }));
        assert!(matches!(report.outcomes[1], ActionOutcome::Planned { .. }));
        assert_eq!(report.steps[0].step_id, "clone");
        assert_eq!(report.steps[1].step_id, "brew");
    }

    #[test]
    fn failed_step_is_reported_in_run_report() {
        let task = base_task(Step {
            id: "cmd".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::RunCommand {
                program: "false".into(),
                args: vec![],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Forbidden,
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert_eq!(report.errors.len(), 1);
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        let Some(StepOutput::ProcessExit(process)) = report.steps[0].output.as_ref() else {
            panic!("expected failed command process context");
        };
        assert_eq!(process.exit_code, Some(1));
        assert!(!process.accepted);
    }

    #[test]
    fn successful_run_command_apply_is_reported() {
        let task = base_task(Step {
            id: "cmd".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::RunCommand {
                program: "true".into(),
                args: vec![],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Forbidden,
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(matches!(report.outcomes[0], ActionOutcome::Applied { .. }));
        let Some(StepOutput::ProcessExit(process)) = report.steps[0].output.as_ref() else {
            panic!("expected successful command process context");
        };
        assert_eq!(process.exit_code, Some(0));
        assert!(process.accepted);
    }

    #[test]
    fn satisfied_run_command_does_not_publish_invented_process_context() {
        let mut step = plain_step(
            "checked-command",
            Action::RunCommand {
                program: "/usr/bin/false".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        step.check = Some(crate::automation::task::Check {
            path_exists: None,
            command_succeeds: Some(vec!["/usr/bin/true".into()]),
        });

        let report = apply_test_task(&base_task(step));

        assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
        assert!(report.steps[0].output.is_none());
        assert!(report.context.entries().is_empty());
        assert!(report.errors.is_empty());
    }

    #[test]
    fn steps_after_failure_are_not_activated() {
        let task = Task {
            id: "setup-dev".into(),
            name: "Setup dev".into(),
            description: "Test setup scenario.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![
                Step {
                    id: "fail".into(),
                    name: String::new(),
                    bindings: BTreeMap::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    when: None,
                    require: None,
                    action: Action::RunCommand {
                        program: "false".into(),
                        args: vec![],
                        cwd: None,
                        env: Default::default(),
                        shell: ShellMode::Forbidden,
                    },
                },
                Step {
                    id: "later".into(),
                    name: String::new(),
                    bindings: BTreeMap::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    when: None,
                    require: None,
                    action: Action::RunCommand {
                        program: "true".into(),
                        args: vec![],
                        cwd: None,
                        env: Default::default(),
                        shell: ShellMode::Forbidden,
                    },
                },
            ],
            graph: None,
        };
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.steps.len(), 1);
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
    }

    #[test]
    fn run_task_rejects_invalid_programmatic_package_registry_action() {
        let task = base_task(Step {
            id: "package-config".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::ConfigurePackageRegistryFiles {
                secrets: crate::automation::task::EncryptedSecretsSpec {
                    profile: "github-packages".into(),
                    username_env: "GITHUB_PACKAGES_USER".into(),
                    token_env: "GITHUB_PACKAGES_TOKEN".into(),
                },
                npm: crate::automation::task::NpmRegistryFileSpec {
                    scope: "@dodopizza".into(),
                    registry: "http://npm.pkg.github.com/".into(),
                },
                nuget: crate::automation::task::NugetRegistryFileSpec {
                    public_source_name: "nuget.org".into(),
                    public_source: "https://api.nuget.org/v3/index.json".into(),
                    source_name: "github".into(),
                    source: "https://nuget.pkg.github.com/dodopizza/index.json".into(),
                    package_patterns: vec!["Dodo.*".into()],
                },
            },
        });

        let err = run_task(&task, &RunOptions::default()).unwrap_err();

        assert!(err.to_string().contains("npm.registry to be an HTTPS URL"));
    }

    #[test]
    fn run_task_accepts_imported_steps_with_resolved_scenario_provenance() {
        let mut task = base_task(Step {
            id: "inspect".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        });
        task.resolved_scenarios = vec!["child-scenario".into()];

        let report = run_task(&task, &RunOptions::default()).unwrap();

        assert_eq!(report.scenarios, ["child-scenario"]);
        assert_eq!(report.plans.len(), 1);
    }

    #[test]
    fn shell_mode_allow_runs_via_shell() {
        let task = base_task(Step {
            id: "cmd".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::RunCommand {
                program: "sh".into(),
                args: vec!["-lc".into(), ":".into()],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Allow,
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                allow_shell: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(matches!(report.outcomes[0], ActionOutcome::Applied { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn run_script_passes_arguments_environment_and_working_directory_without_shell_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.sh");
        fs::write(
            &script,
            r#"printf '%s\n%s\n%s\n' "$1" "$PPDUSTER_SCRIPT_VALUE" "$PWD" > result.txt
"#,
        )
        .unwrap();
        let argument = "value with spaces; $(exit 91)";
        let environment = "environment with spaces; $(exit 92)";
        let task = base_task(Step {
            id: "script".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::RunScript {
                interpreter: ScriptInterpreter::Sh,
                script: "verify.sh".into(),
                args: vec![argument.into()],
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                env: BTreeMap::from([("PPDUSTER_SCRIPT_VALUE".into(), environment.into())]),
                success_exit_codes: vec![0],
            },
        });

        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                allow_shell: true,
                ..RunOptions::default()
            },
        )
        .unwrap();

        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        let result = fs::read_to_string(dir.path().join("result.txt")).unwrap();
        let lines = result.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], argument);
        assert_eq!(lines[1], environment);
        assert_eq!(
            PathBuf::from(lines[2]).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn powershell_script_command_uses_file_mode_without_command_concatenation() {
        let mut command = Command::new("pwsh");
        configure_script_command(
            &mut command,
            ScriptInterpreter::PowerShell,
            Path::new("script with spaces.ps1"),
            &[OsString::from("argument; Write-Error should-not-run")],
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(args.starts_with(&[
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
        ]));
        assert!(args.iter().any(|arg| arg == "-File"));
        assert!(args.iter().any(|arg| arg == "script with spaces.ps1"));
        assert_eq!(args.last().unwrap(), "argument; Write-Error should-not-run");
        assert!(!args.iter().any(|arg| arg == "-Command"));
        if cfg!(target_os = "windows") {
            assert!(args
                .windows(2)
                .any(|pair| pair == ["-ExecutionPolicy", "Bypass"]));
        }
    }

    #[test]
    fn run_script_executes_powershell_with_direct_arguments_when_available() {
        let probe_dir = tempfile::tempdir().unwrap();
        let powershell_available = script_interpreter_candidates(ScriptInterpreter::PowerShell)
            .iter()
            .any(|program| {
                Command::new(program)
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "exit 0",
                    ])
                    .current_dir(probe_dir.path())
                    .status()
                    .is_ok_and(|status| status.success())
            });
        if !powershell_available {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.ps1");
        fs::write(
            &script,
            r#"param([string]$Value)
$Content = @($Value, $env:PPDUSTER_SCRIPT_VALUE, (Get-Location).Path) -join "`n"
$Encoding = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText((Join-Path (Get-Location).Path "result.txt"), $Content, $Encoding)
"#,
        )
        .unwrap();
        let argument = "value with spaces; Write-Error should-not-run";
        let environment = "environment with spaces; Write-Error should-not-run";

        apply_run_script(
            ScriptInterpreter::PowerShell,
            &script.to_string_lossy(),
            &[argument.into()],
            Some(&dir.path().to_string_lossy()),
            &BTreeMap::from([("PPDUSTER_SCRIPT_VALUE".into(), environment.into())]),
            &[0],
        )
        .unwrap();

        let result = fs::read_to_string(dir.path().join("result.txt")).unwrap();
        let lines = result.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], argument);
        assert_eq!(lines[1], environment);
        assert_eq!(
            PathBuf::from(lines[2]).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn lightburn_activation_refuses_non_interactive_runs_before_launch() {
        let launched = std::cell::Cell::new(false);
        let err = apply_activate_license_with(
            LicenseProvider::LightBurn,
            LicenseMethod::VendorUi,
            false,
            |_| {
                launched.set(true);
                Ok(())
            },
            |_| Ok(true),
        )
        .unwrap_err();

        assert!(!launched.get());
        assert!(err.to_string().contains("interactive terminal"));
    }

    #[test]
    fn lightburn_activation_launches_only_vendor_ui_and_uses_nonsecret_confirmation() {
        let launched = std::cell::RefCell::new(Vec::new());
        let prompts = std::cell::RefCell::new(Vec::new());
        let summary = apply_activate_license_with(
            LicenseProvider::LightBurn,
            LicenseMethod::VendorUi,
            true,
            |provider| {
                launched.borrow_mut().push(provider);
                Ok(())
            },
            |prompt| {
                prompts.borrow_mut().push(prompt.to_owned());
                Ok(true)
            },
        )
        .unwrap();

        assert_eq!(launched.into_inner(), vec![LicenseProvider::LightBurn]);
        let app_path = license_application_path(LicenseProvider::LightBurn).unwrap();
        assert_eq!(
            app_path,
            dirs::home_dir().unwrap().join("Applications/LightBurn.app")
        );
        let launch_arguments = license_launch_arguments(&app_path);
        assert_eq!(launch_arguments[0], "-n");
        assert_eq!(launch_arguments[1], app_path.as_os_str());
        let prompts = prompts.into_inner();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("ACTIVATED"));
        assert!(summary.contains("did not read or store"));
    }

    #[test]
    fn lightburn_activation_requires_explicit_confirmation() {
        let err = apply_activate_license_with(
            LicenseProvider::LightBurn,
            LicenseMethod::VendorUi,
            true,
            |_| Ok(()),
            |_| Ok(false),
        )
        .unwrap_err();

        assert!(err.to_string().contains("expected ACTIVATED"));
    }

    #[test]
    fn non_interactive_license_preflight_runs_before_download() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.dmg");
        let destination = tmp.path().join("downloaded.dmg");
        fs::write(&source, b"not-reached").unwrap();
        let task = Task {
            id: "lightburn-preflight".into(),
            name: "LightBurn preflight".into(),
            description: "Test LightBurn preflight scenario.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![
                Step {
                    id: "download".into(),
                    name: String::new(),
                    bindings: BTreeMap::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    when: None,
                    require: None,
                    action: Action::DownloadFile {
                        url: format!("file://{}", source.display()),
                        dest: destination.to_string_lossy().into_owned(),
                        checksum: Checksum {
                            sha256: sha256_file(&source).unwrap(),
                        },
                    },
                },
                Step {
                    id: "activate".into(),
                    name: String::new(),
                    bindings: BTreeMap::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    when: None,
                    require: None,
                    action: Action::ActivateLicense(ActivateLicenseAction {
                        provider: LicenseProvider::LightBurn,
                        method: LicenseMethod::VendorUi,
                    }),
                },
            ],
            graph: None,
        };

        let err = run_imported_task_with_interactivity(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("interactive terminal"));
        assert!(!destination.exists());
    }

    #[test]
    fn macos_version_comparison_handles_missing_components() {
        assert!(version_at_least("26.6", "12.0").unwrap());
        assert!(version_at_least("12", "12.0").unwrap());
        assert!(version_at_least("12.0.1", "12").unwrap());
        assert!(!version_at_least("11.7.10", "12.0").unwrap());
    }

    #[test]
    fn app_identity_uses_test_requirement_with_signed_version() {
        let identity = AppBundleIdentity {
            bundle_identifier: "com.LightBurnSoftware.LightBurn".into(),
            team_identifier: "UWZQ3LL82C".into(),
            version: "2.1.03".into(),
        };
        let arguments = app_identity_verification_arguments(&identity);
        let arguments = arguments
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();

        assert!(arguments.iter().any(|arg| arg == "--test-requirement"));
        assert!(!arguments.iter().any(|arg| arg == "--requirement"));
        let requirement = arguments.last().unwrap();
        assert!(requirement.contains("identifier \"com.LightBurnSoftware.LightBurn\""));
        assert!(requirement.contains("subject.OU] = \"UWZQ3LL82C\""));
        assert!(requirement.contains("CFBundleShortVersionString] = \"2.1.03\""));
    }

    #[test]
    fn app_store_install_plan_is_typed_unprivileged_and_reports_prerequisites() {
        let task = base_task(Step {
            id: "install-xcode".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::AppStoreInstall(AppStoreInstallAction {
                app_id: 497_799_835,
                operation: AppStoreOperation::Install,
            }),
        });

        let report = run_task(&task, &RunOptions::default()).unwrap();

        assert!(report.plans[0]
            .summary
            .contains("App Store install request for application 497799835"));
        assert!(report.plans[0]
            .prerequisites
            .iter()
            .any(|item| item.contains("ppstore 0.1.x")));
        assert!(report.plans[0]
            .prerequisites
            .iter()
            .any(|item| item.contains("Mac App Store")));
        assert!(report.plans[0]
            .prerequisites
            .iter()
            .any(|item| item.contains("operation: get")));
    }

    #[cfg(unix)]
    #[test]
    fn install_root_must_not_be_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real-applications");
        let link = tmp.path().join("Applications");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = require_real_directory(&link).unwrap_err();
        assert!(err.to_string().contains("not a symlink"));
    }

    #[test]
    fn system_app_dmg_install_is_rejected() {
        let task = base_task(Step {
            id: "install".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::InstallDmg {
                dmg: "$HOME/Library/Caches/app.dmg".into(),
                app_name: Some("Example.app".into()),
                target: Some("/Applications".into()),
                identity: None,
            },
        });

        let err = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(err.to_string().contains("restricted to $HOME/Applications"));
    }

    #[test]
    fn user_app_dmg_install_plans_without_elevation() {
        let task = base_task(Step {
            id: "install".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::InstallDmg {
                dmg: "$HOME/Library/Caches/app.dmg".into(),
                app_name: Some("Example.app".into()),
                target: Some("$HOME/Applications".into()),
                identity: None,
            },
        });

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(matches!(report.outcomes[0], ActionOutcome::Planned { .. }));
        assert!(report.plans[0].summary.contains("$HOME/Applications"));
        assert!(report.plans[0].prerequisites.is_empty());
    }

    fn github_release(
        tag: &str,
        prerelease: bool,
        published_at: &str,
        asset_name: &str,
        digest: &str,
    ) -> GithubRelease {
        GithubRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            published_at: published_at.into(),
            assets: vec![GithubAsset {
                name: asset_name.into(),
                browser_download_url: format!("{BAMBU_DOWNLOAD_PREFIX}{tag}/{asset_name}"),
                digest: Some(format!("sha256:{digest}")),
            }],
        }
    }

    #[test]
    fn bambu_release_resolver_selects_stable_or_beta_and_asset_version() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let stable = github_release(
            "v02.07.01.62",
            false,
            "2026-06-16T00:00:00Z",
            "Bambu_Studio_mac-v02.07.01.62-20260616.dmg",
            digest,
        );
        let beta = github_release(
            "v02.08.01.55",
            true,
            "2026-07-14T00:00:00Z",
            "Bambu_Studio_mac-v02.08.01.55-20260714.dmg",
            digest,
        );
        let releases = vec![stable, beta];

        let resolved_stable = resolve_bambu_release(&releases, ReleaseChannel::Release).unwrap();
        let resolved_beta = resolve_bambu_release(&releases, ReleaseChannel::Beta).unwrap();
        assert_eq!(resolved_stable.version, "02.07.01.62");
        assert_eq!(resolved_beta.version, "02.08.01.55");
    }

    #[test]
    fn bambu_plan_honors_channel_override_without_network() {
        let task = base_task(Step {
            id: "bambu".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::BambuStudioRelease(crate::automation::task::BambuStudioReleaseAction {
                channel: ReleaseChannel::Release,
            }),
        });
        let report = run_task(
            &task,
            &RunOptions {
                release_channel: Some(ReleaseChannel::Beta),
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(report.plans[0].summary.contains("latest Bambu Studio beta"));
    }

    #[test]
    fn github_repository_output_serializes_full_typed_context() {
        let output = StepOutput::GithubRepositories(GithubRepositoriesOutput {
            github: GithubContextOutput {
                account: GithubAccountOutput {
                    login: "octocat".into(),
                },
                repositories: vec![GithubRepositoryOutput {
                    id: "R_123".into(),
                    owner: "owner".into(),
                    name: "repository".into(),
                    full_name: "owner/repository".into(),
                    https_url: "https://github.com/owner/repository".into(),
                    ssh_url: "git@github.com:owner/repository.git".into(),
                    default_branch: Some("main".into()),
                    private: true,
                    archived: false,
                }],
            },
        });

        let json = serde_json::to_value(output).unwrap();
        assert_eq!(json["type"], "github-repositories");
        assert_eq!(json["value"]["github"]["account"]["login"], "octocat");
        let repository = &json["value"]["github"]["repositories"][0];
        assert_eq!(repository["id"], "R_123");
        assert_eq!(repository["owner"], "owner");
        assert_eq!(repository["name"], "repository");
        assert_eq!(repository["full_name"], "owner/repository");
        assert_eq!(
            repository["https_url"],
            "https://github.com/owner/repository"
        );
        assert_eq!(repository["ssh_url"], "git@github.com:owner/repository.git");
        assert_eq!(repository["default_branch"], "main");
        assert_eq!(repository["private"], true);
        assert_eq!(repository["archived"], false);
    }

    fn github_selection_repository(id: &str, name: &str) -> GithubRepositoryInput {
        GithubRepositoryInput {
            id: id.into(),
            owner: "owner".into(),
            name: name.into(),
            full_name: format!("owner/{name}"),
            https_url: format!("https://github.com/owner/{name}"),
            ssh_url: format!("git@github.com:owner/{name}.git"),
            default_branch: Some("main".into()),
            private: false,
            archived: false,
        }
    }

    fn github_selection_input() -> GithubContextInput {
        GithubContextInput {
            account: crate::automation::task::GithubAccountInput {
                login: "octocat".into(),
            },
            repositories: vec![
                github_selection_repository("R_alpha", "alpha"),
                github_selection_repository("R_beta", "beta"),
            ],
        }
    }

    fn github_selection_source_context(input: &GithubContextInput) -> ContextValue {
        let output = StepOutput::GithubRepositories(GithubRepositoriesOutput {
            github: GithubContextOutput {
                account: GithubAccountOutput {
                    login: input.account.login.clone(),
                },
                repositories: input
                    .repositories
                    .iter()
                    .map(github_repository_input_output)
                    .collect(),
            },
        });
        ContextValue::new(
            output.context_value().unwrap(),
            ContextProvenance::step("list"),
        )
        .with_schema(
            crate::automation::block::block_definition(
                crate::automation::block::ActionKind::GithubListRepositories,
            )
            .output_schema,
        )
    }

    #[test]
    fn github_selection_filters_exact_ids_in_authored_order() {
        let result = apply_github_select_repositories(
            &github_selection_input(),
            "octocat",
            &["R_beta".into(), "R_alpha".into()],
        )
        .unwrap();
        let ApplyStepResult::AppliedWithOutput { output, .. } = result else {
            panic!("expected typed selection output")
        };
        let StepOutput::GithubRepositories(output) = output else {
            panic!("expected GitHub repositories output")
        };
        assert_eq!(output.github.account.login, "octocat");
        assert_eq!(
            output
                .github
                .repositories
                .iter()
                .map(|repository| repository.id.as_str())
                .collect::<Vec<_>>(),
            ["R_beta", "R_alpha"]
        );
    }

    #[test]
    fn github_selection_fails_closed_on_account_or_repository_drift() {
        let input = github_selection_input();
        let account_error =
            apply_github_select_repositories(&input, "another-user", &["R_alpha".into()])
                .unwrap_err();
        assert!(format!("{account_error:#}").contains("authored for account"));

        let repository_error =
            apply_github_select_repositories(&input, "octocat", &["R_missing".into()]).unwrap_err();
        assert!(format!("{repository_error:#}").contains("no longer visible"));

        let mut duplicate_input = input;
        duplicate_input
            .repositories
            .push(github_selection_repository("R_alpha", "duplicate"));
        let duplicate_error =
            apply_github_select_repositories(&duplicate_input, "octocat", &["R_alpha".into()])
                .unwrap_err();
        assert!(format!("{duplicate_error:#}").contains("duplicate node ID"));
    }

    #[test]
    fn graph_runtime_materializes_and_publishes_github_selection_output() {
        let placeholder = GithubContextInput {
            account: crate::automation::task::GithubAccountInput {
                login: "octocat".into(),
            },
            repositories: Vec::new(),
        };
        let selector = plain_step(
            "select",
            Action::GithubSelectRepositories {
                github: placeholder,
                expected_account_login: "octocat".into(),
                repository_ids: vec!["R_beta".into()],
            },
        );
        let graph = WorkflowGraph {
            entries: vec!["select".into()],
            nodes: vec![action_node(
                selector,
                BTreeMap::from([(
                    "github".into(),
                    Binding::field(FieldRef::step("list").field("github")),
                )]),
            )],
            ..WorkflowGraph::default()
        };
        let task = graph_task(graph.clone());
        let options = RunOptions {
            apply: true,
            ..RunOptions::default()
        };
        let mut runtime = GraphRuntime {
            task: &task,
            opts: &options,
            terminal_interactive: false,
            accumulator: GraphRunAccumulator::default(),
            budget: GraphExecutionBudget::default(),
        };
        let input = github_selection_input();
        let context = github_selection_source_context(&input);
        let mut scope = GraphScopeState::default();
        scope.values.insert(
            ContextScope::Step {
                step_id: "list".into(),
            },
            context,
        );

        let invocation = runtime.execute_graph(&graph, &mut scope, "", 1).unwrap();
        assert!(!invocation.failed);
        let selection = scope
            .values
            .get(&ContextScope::Step {
                step_id: "select".into(),
            })
            .expect("selector must publish typed output");
        assert_eq!(selection.value["github"]["account"]["login"], "octocat");
        assert_eq!(
            selection.value["github"]["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .map(|repository| repository["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["R_beta"]
        );
    }

    #[test]
    fn stale_github_selection_does_not_activate_its_success_edge_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("must-not-be-created");
        let placeholder = GithubContextInput {
            account: crate::automation::task::GithubAccountInput {
                login: "octocat".into(),
            },
            repositories: Vec::new(),
        };
        let selector = plain_step(
            "select",
            Action::GithubSelectRepositories {
                github: placeholder,
                expected_account_login: "octocat".into(),
                repository_ids: vec!["R_missing".into()],
            },
        );
        let mutation = plain_step(
            "mutate",
            Action::CreateDirectory(CreateDirectoryAction {
                path: target.to_string_lossy().into_owned(),
            }),
        );
        let graph = WorkflowGraph {
            entries: vec!["select".into()],
            nodes: vec![
                action_node(
                    selector,
                    BTreeMap::from([(
                        "github".into(),
                        Binding::field(FieldRef::step("list").field("github")),
                    )]),
                ),
                action_node(mutation, BTreeMap::new()),
            ],
            edges: vec![GraphEdge::new("select", EdgePort::Success, "mutate")],
            ..WorkflowGraph::default()
        };
        let task = graph_task(graph.clone());
        let options = RunOptions {
            apply: true,
            ..RunOptions::default()
        };
        let mut runtime = GraphRuntime {
            task: &task,
            opts: &options,
            terminal_interactive: false,
            accumulator: GraphRunAccumulator::default(),
            budget: GraphExecutionBudget::default(),
        };
        let input = github_selection_input();
        let context = github_selection_source_context(&input);
        let mut scope = GraphScopeState::default();
        scope.values.insert(
            ContextScope::Step {
                step_id: "list".into(),
            },
            context,
        );

        let invocation = runtime.execute_graph(&graph, &mut scope, "", 1).unwrap();
        assert!(invocation.failed);
        assert!(!target.exists());
        assert!(runtime
            .accumulator
            .steps
            .iter()
            .all(|report| report.step_id != "mutate"));
        assert!(runtime
            .accumulator
            .errors
            .iter()
            .any(|error| error.contains("no longer visible")));
    }

    #[test]
    fn structured_filesystem_output_is_published_for_applied_and_satisfied_steps() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("nested");
        let task = base_task(plain_step(
            "mkdir",
            Action::CreateDirectory(CreateDirectoryAction {
                path: directory.to_string_lossy().into_owned(),
            }),
        ));

        let applied = apply_test_task(&task);
        let Some(StepOutput::Structured(output)) = applied.steps[0].output.as_ref() else {
            panic!("expected structured create-directory output");
        };
        assert_eq!(output.schema_id, "ppduster.filesystem.create-directory@1");
        assert_eq!(output.value["path"]["created"], true);
        assert_eq!(output.value["path"]["exists"], true);
        let path = applied
            .context
            .resolve(
                &crate::automation::context::FieldRef::step("mkdir")
                    .field("path")
                    .field("value"),
            )
            .unwrap();
        assert_eq!(path.value, directory.to_string_lossy().as_ref());

        let satisfied = apply_test_task(&task);
        let Some(StepOutput::Structured(output)) = satisfied.steps[0].output.as_ref() else {
            panic!("expected structured satisfied output");
        };
        assert_eq!(output.value["path"]["created"], false);
        assert_eq!(output.value["path"]["changed"], false);
    }

    #[test]
    fn context_value_removes_step_output_transport_envelope() {
        let output = structured_step_output(
            "example@1",
            serde_json::json!({"items": [{"url": "https://example.invalid"}]}),
        );
        let value = output.context_value().unwrap();
        assert_eq!(value["items"][0]["url"], "https://example.invalid");
        assert!(value.get("schema_id").is_none());
    }

    fn created_rule(step_id: &str) -> StepCondition {
        use crate::automation::expression::{
            ComparisonOperator, ExpressionV1, ExpressionValue, ReferenceV1,
        };

        StepCondition::Expression {
            rule: ExpressionV1::Compare {
                operator: ComparisonOperator::Equal,
                left: Box::new(ExpressionV1::Ref {
                    reference: ReferenceV1::Context {
                        field: crate::automation::context::FieldRef::step(step_id)
                            .field("path")
                            .field("created"),
                    },
                }),
                right: Box::new(ExpressionV1::Literal {
                    value: ExpressionValue::Bool(true),
                }),
            },
            policy: crate::automation::task::RuleOutcomePolicy::default(),
        }
    }

    #[test]
    fn typed_context_rule_controls_a_later_step() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let mut task = base_task(plain_step(
            "first",
            Action::CreateDirectory(CreateDirectoryAction {
                path: first.to_string_lossy().into_owned(),
            }),
        ));
        let mut dependent = plain_step(
            "second",
            Action::CreateDirectory(CreateDirectoryAction {
                path: second.to_string_lossy().into_owned(),
            }),
        );
        dependent.when = Some(created_rule("first"));
        task.steps.push(dependent);

        task.validate().unwrap();
        let first_run = apply_test_task(&task);
        assert!(matches!(first_run.steps[0].status, StepStatus::Applied));
        assert!(matches!(first_run.steps[1].status, StepStatus::Applied));

        fs::remove_dir(&second).unwrap();
        let second_run = apply_test_task(&task);
        assert!(matches!(second_run.steps[0].status, StepStatus::Satisfied));
        assert!(matches!(second_run.steps[1].status, StepStatus::Skipped));
        assert!(!second.exists());
    }

    #[test]
    fn context_rule_unknown_requires_an_explicit_policy() {
        let temp = tempfile::tempdir().unwrap();
        let absent = temp.path().join("absent");
        let target = temp.path().join("target");
        let mut source = plain_step(
            "source",
            Action::CreateDirectory(CreateDirectoryAction {
                path: temp.path().join("source").to_string_lossy().into_owned(),
            }),
        );
        source.when = Some(StepCondition::Path {
            path: absent.to_string_lossy().into_owned(),
            expect: PathExpectation {
                exists: Some(true),
                ..PathExpectation::default()
            },
        });
        let mut consumer = plain_step(
            "consumer",
            Action::CreateDirectory(CreateDirectoryAction {
                path: target.to_string_lossy().into_owned(),
            }),
        );
        let mut rule = created_rule("source");
        let StepCondition::Expression { policy, .. } = &mut rule else {
            unreachable!()
        };
        policy.on_unknown = IndeterminatePolicy::TreatAsFalse;
        consumer.when = Some(rule);
        let mut task = base_task(source);
        task.steps.push(consumer);

        let report = apply_test_task(&task);
        assert!(report.errors.is_empty());
        assert!(matches!(report.steps[0].status, StepStatus::Skipped));
        assert!(matches!(report.steps[1].status, StepStatus::Skipped));
        assert!(!target.exists());
    }

    #[test]
    fn foreach_resolves_repository_array_and_renders_clone_fields() {
        let output = StepOutput::GithubRepositories(GithubRepositoriesOutput {
            github: GithubContextOutput {
                account: GithubAccountOutput {
                    login: "octocat".into(),
                },
                repositories: vec![GithubRepositoryOutput {
                    id: "R_123".into(),
                    owner: "owner".into(),
                    name: "repository".into(),
                    full_name: "owner/repository".into(),
                    https_url: "https://github.com/owner/repository".into(),
                    ssh_url: "git@github.com:owner/repository.git".into(),
                    default_branch: Some("main".into()),
                    private: false,
                    archived: false,
                }],
            },
        });
        let reports = vec![StepReport {
            step_id: "repositories".into(),
            step_name: "Repositories".into(),
            summary: String::new(),
            status: StepStatus::Applied,
            prerequisites: Vec::new(),
            logs: Vec::new(),
            output: Some(output),
        }];

        let selected_fields = vec![
            "https_url".into(),
            "owner".into(),
            "name".into(),
            "default_branch".into(),
        ];
        let items = resolve_for_each_items(
            "repositories",
            "github.repositories",
            &selected_fields,
            &reports,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].get("id").is_none());
        assert!(items[0].get("ssh_url").is_none());
        assert_eq!(
            render_item_template("{{repository.https_url}}", "repository", &items[0]).unwrap(),
            "https://github.com/owner/repository"
        );
        assert_eq!(
            render_item_template(
                "$HOME/Developer/{{repository.owner}}/{{repository.name}}",
                "repository",
                &items[0]
            )
            .unwrap(),
            "$HOME/Developer/owner/repository"
        );
        assert_eq!(
            render_optional_item_template("{{repository.default_branch}}", "repository", &items[0])
                .unwrap(),
            Some("main".into())
        );
    }

    #[test]
    fn github_base64_node_id_validates_and_materializes_through_foreach_binding() {
        const NODE_ID: &str = "MDEwOlJlcG9zaXRvcnkxMjM0NTY=";

        let producer = plain_step("repositories", Action::GithubListRepositories);
        let task = graph_task(one_action_graph(producer.clone(), BTreeMap::new()));
        let reports = vec![StepReport {
            step_id: producer.id.clone(),
            step_name: "Repositories".into(),
            summary: String::new(),
            status: StepStatus::Applied,
            prerequisites: Vec::new(),
            logs: Vec::new(),
            output: Some(StepOutput::GithubRepositories(GithubRepositoriesOutput {
                github: GithubContextOutput {
                    account: GithubAccountOutput {
                        login: "octocat".into(),
                    },
                    repositories: vec![GithubRepositoryOutput {
                        id: NODE_ID.into(),
                        owner: "owner".into(),
                        name: "repository".into(),
                        full_name: "owner/repository".into(),
                        https_url: "https://github.com/owner/repository".into(),
                        ssh_url: "git@github.com:owner/repository.git".into(),
                        default_branch: Some("main".into()),
                        private: false,
                        archived: false,
                    }],
                },
            })),
        }];

        let values = context_store_from_reports(&task, &reports).unwrap();
        let collection_field = FieldRef::step("repositories")
            .field("github")
            .field("repositories");
        let expected_collection = ResolvedSchemaOwned {
            value_type: ContextType::array(ContextType::Any),
            required: true,
            nullable: false,
            sensitivity: Sensitivity::Public,
            allowed_values: Vec::new(),
        };
        let collection = resolve_binding(
            &Binding::field(collection_field),
            &expected_collection,
            &values,
            BindingLimits::default(),
        )
        .unwrap();
        let repository = collection.value.as_array().unwrap()[0].clone();
        let repository_type = definition_for_action(&producer.action)
            .output_schema
            .resolve(&[
                ContextPathSegment::field("github"),
                ContextPathSegment::field("repositories"),
                ContextPathSegment::index(0),
            ])
            .unwrap()
            .value_type
            .clone();
        let mut iteration_scope = GraphScopeState {
            values,
            ..GraphScopeState::default()
        };
        insert_loop_value(
            &mut iteration_scope,
            "repositories-loop",
            0,
            repository,
            repository_type,
            collection.sensitivity,
        );
        let consumer = plain_step(
            "consume-id",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        );
        let consumer_bindings = BTreeMap::from([(
            "/env/REPOSITORY_ID".into(),
            Binding::field(FieldRef::loop_item("repositories-loop").field("id")),
        )]);
        let materialized = materialize_step(
            &consumer,
            &consumer_bindings,
            &iteration_scope.values,
            BindingLimits::default(),
        )
        .unwrap();
        let Action::RunCommand { env, .. } = materialized.action else {
            panic!("expected run-command consumer")
        };
        assert_eq!(env.get("REPOSITORY_ID").map(String::as_str), Some(NODE_ID));
    }

    #[test]
    fn foreach_optional_template_maps_null_to_none() {
        let item = serde_json::json!({"default_branch": null});
        assert_eq!(
            render_optional_item_template("{{repository.default_branch}}", "repository", &item)
                .unwrap(),
            None
        );
    }

    #[test]
    fn numeric_version_comparison_prevents_downgrades() {
        assert_eq!(
            compare_versions("02.08.01.55", "02.07.01.62").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("02.07.01.62", "2.7.1.62").unwrap(),
            Ordering::Equal
        );
    }
}

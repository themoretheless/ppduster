use crate::automation::block::definition_for_action;
use crate::automation::context::{
    Binding, ContextProvenance, ContextScope, ContextStore, ContextValue, TemplatePart,
};
use crate::automation::expression::{check_rule, ExpressionLimits, RuleExprV1};
use crate::automation::graph::{
    GraphValidationError, LegacyTaskImporter, LinearMigrationError, WorkflowGraph,
    WorkflowGraphMigrationError, WORKFLOW_GRAPH_VERSION,
};
use crate::rules::Platform;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustRequirement {
    #[default]
    BundledOnly,
    UserConfigAllowed,
    ExternalAllowed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShellMode {
    #[default]
    Forbidden,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScriptInterpreter {
    #[serde(rename = "sh")]
    Sh,
    #[serde(rename = "bash")]
    Bash,
    #[serde(rename = "powershell", alias = "pwsh", alias = "power-shell")]
    PowerShell,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ElevationPolicy {
    #[default]
    Forbidden,
    Allow,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Checksum {
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Check {
    #[serde(default)]
    pub path_exists: Option<PathBuf>,
    #[serde(default)]
    pub command_succeeds: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Assertions evaluated against one atomic path inspection.
///
/// Every populated field is combined with logical AND. This is intentionally
/// separate from `Check`, whose existing meaning is "the mutating step is
/// already satisfied" rather than "fail when this assertion is false".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathExpectation {
    #[serde(default)]
    pub exists: Option<bool>,
    #[serde(default)]
    pub kind: Option<PathKind>,
    #[serde(default)]
    pub empty: Option<bool>,
    #[serde(default)]
    pub min_size_bytes: Option<u64>,
    #[serde(default)]
    pub max_size_bytes: Option<u64>,
    #[serde(default)]
    pub modified_at_or_after: Option<DateTime<Utc>>,
    #[serde(default)]
    pub modified_at_or_before: Option<DateTime<Utc>>,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl PathExpectation {
    fn has_metadata_assertion(&self) -> bool {
        self.kind.is_some()
            || self.empty.is_some()
            || self.min_size_bytes.is_some()
            || self.max_size_bytes.is_some()
            || self.modified_at_or_after.is_some()
            || self.modified_at_or_before.is_some()
            || self.sha256.is_some()
    }

    fn is_empty(&self) -> bool {
        self.exists.is_none() && !self.has_metadata_assertion()
    }

    fn validate(&self, step_id: &str) -> Result<(), String> {
        if self.is_empty() {
            return Err(format!(
                "step {} path expectation must contain at least one assertion",
                step_id
            ));
        }
        if matches!(self.exists, Some(false)) && self.has_metadata_assertion() {
            return Err(format!(
                "step {} path expectation exists: false cannot be combined with metadata assertions",
                step_id
            ));
        }
        if let (Some(minimum), Some(maximum)) = (self.min_size_bytes, self.max_size_bytes) {
            if minimum > maximum {
                return Err(format!(
                    "step {} path expectation min_size_bytes must not exceed max_size_bytes",
                    step_id
                ));
            }
        }
        if let (Some(after), Some(before)) = (
            self.modified_at_or_after.as_ref(),
            self.modified_at_or_before.as_ref(),
        ) {
            if after > before {
                return Err(format!(
                    "step {} path expectation modified_at_or_after must not be later than modified_at_or_before",
                    step_id
                ));
            }
        }
        if matches!(self.empty, Some(true)) && self.min_size_bytes.is_some_and(|size| size > 0) {
            return Err(format!(
                "step {} path expectation empty: true cannot require a positive min_size_bytes",
                step_id
            ));
        }
        if let Some(sha256) = &self.sha256 {
            if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!(
                    "step {} path expectation sha256 must contain exactly 64 hexadecimal characters",
                    step_id
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateDirectoryAction {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectPathAction {
    pub path: String,
    /// Recursively total regular-file bytes for a directory. Symlinks are not
    /// followed. File sizes are always reported regardless of this setting.
    #[serde(default)]
    pub recursive_size: bool,
    /// Compute SHA-256 for a regular file. Symlinks are never followed.
    #[serde(default)]
    pub sha256: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<PathExpectation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyPathAction {
    pub src: String,
    pub dest: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteConflictPolicy {
    #[default]
    Fail,
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteFileAction {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub on_conflict: WriteConflictPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovePathAction {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NpmRegistryFileSpec {
    pub scope: String,
    pub registry: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NugetRegistryFileSpec {
    pub public_source_name: String,
    pub public_source: String,
    pub source_name: String,
    pub source: String,
    pub package_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptedSecretsSpec {
    pub profile: String,
    pub username_env: String,
    pub token_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFile {
    pub task: Task,
}

/// Canonical task document version. Versions 1 and 2 are accepted only by the
/// compatibility importer and are never emitted again.
pub const TASK_FORMAT_VERSION: u32 = 3;

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub description: String,
    pub platform: Platform,
    pub trust: TrustRequirement,
    /// Other scenarios included by a legacy reusable template, in execution
    /// order.
    ///
    /// This field is import-only. `TaskPack::resolve` expands it and returns a
    /// canonical graph-only v3 task.
    pub scenarios: Vec<String>,
    /// Root scenario references retained after `TaskPack::resolve` flattens a
    /// template. This is runtime provenance only and is never written to YAML.
    pub resolved_scenarios: Vec<String>,
    /// Programmatic compatibility for legacy callers. Deserializing `steps`
    /// immediately imports them into `graph` and leaves this vector empty.
    /// New code must edit `graph` directly.
    pub steps: Vec<Step>,
    /// Canonical v3 execution graph. Canvas layout metadata is deliberately
    /// not part of this representation and is never used to infer control
    /// flow.
    pub graph: Option<WorkflowGraph>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskWire {
    #[serde(default)]
    format_version: Option<u32>,
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    platform: Platform,
    #[serde(default)]
    trust: TrustRequirement,
    #[serde(default, alias = "includes")]
    scenarios: Vec<String>,
    #[serde(default)]
    steps: Vec<Step>,
    #[serde(default)]
    workflow_graph: Option<WorkflowGraph>,
    /// Historical v2 key. Keeping it distinct lets the v3 envelope reject a
    /// document that merely relabels legacy graph syntax as format v3.
    #[serde(default, rename = "graph")]
    legacy_graph: Option<WorkflowGraph>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct TaskV3Wire<'a> {
    format_version: u32,
    id: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    description: &'a str,
    platform: Platform,
    trust: TrustRequirement,
    workflow_graph: &'a WorkflowGraph,
}

impl<'de> Deserialize<'de> for Task {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TaskWire::deserialize(deserializer)?;
        if let Some(version) = wire.format_version {
            if version == 0 || version > TASK_FORMAT_VERSION {
                return Err(serde::de::Error::custom(format!(
                    "unsupported task format version {version}; current version is {TASK_FORMAT_VERSION}"
                )));
            }
        }

        if wire.format_version == Some(TASK_FORMAT_VERSION)
            && (!wire.scenarios.is_empty()
                || !wire.steps.is_empty()
                || wire.legacy_graph.is_some()
                || wire.workflow_graph.is_none())
        {
            return Err(serde::de::Error::custom(
                "task format version 3 requires exactly one workflow_graph and forbids legacy scenarios, steps, and graph",
            ));
        }

        let forms = usize::from(!wire.scenarios.is_empty())
            + usize::from(!wire.steps.is_empty())
            + usize::from(wire.workflow_graph.is_some())
            + usize::from(wire.legacy_graph.is_some());
        if forms > 1 {
            return Err(serde::de::Error::custom(
                "task must define exactly one of legacy scenarios, legacy steps, or workflow_graph",
            ));
        }

        let imported_graph = wire.workflow_graph.or(wire.legacy_graph);
        let graph = match (imported_graph, wire.steps.as_slice()) {
            (Some(graph), _) => Some(graph.into_v3().map_err(serde::de::Error::custom)?),
            (None, []) => None,
            (None, steps) => {
                Some(LegacyTaskImporter::import_steps(steps).map_err(serde::de::Error::custom)?)
            }
        };

        Ok(Self {
            id: wire.id,
            name: wire.name,
            description: wire.description,
            platform: wire.platform,
            trust: wire.trust,
            scenarios: wire.scenarios,
            resolved_scenarios: Vec::new(),
            steps: Vec::new(),
            graph,
        })
    }
}

impl Serialize for Task {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let workflow_graph = self.workflow_graph().map_err(serde::ser::Error::custom)?;
        TaskV3Wire {
            format_version: TASK_FORMAT_VERSION,
            id: &self.id,
            name: &self.name,
            description: &self.description,
            platform: self.platform,
            trust: self.trust,
            workflow_graph,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskMigrationError {
    InvalidMetadata {
        task: String,
        message: String,
    },
    NoExecutableForm {
        task: String,
    },
    MultipleExecutableForms {
        task: String,
    },
    UnresolvedScenarios {
        task: String,
        scenarios: Vec<String>,
    },
    LegacySteps {
        task: String,
        source: LinearMigrationError,
    },
    WorkflowGraph {
        task: String,
        source: WorkflowGraphMigrationError,
    },
    InvalidGraph {
        task: String,
        errors: Vec<GraphValidationError>,
    },
}

impl std::fmt::Display for TaskMigrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMetadata { task, message } => {
                write!(formatter, "task {task} metadata is invalid: {message}")
            }
            Self::NoExecutableForm { task } => {
                write!(formatter, "task {task} has no executable workflow graph")
            }
            Self::MultipleExecutableForms { task } => write!(
                formatter,
                "task {task} contains conflicting legacy and workflow graph forms"
            ),
            Self::UnresolvedScenarios { task, scenarios } => write!(
                formatter,
                "task {task} contains unresolved legacy scenarios: {}",
                scenarios.join(", ")
            ),
            Self::LegacySteps { task, source } => {
                write!(formatter, "task {task} legacy import failed: {source}")
            }
            Self::WorkflowGraph { task, source } => {
                write!(formatter, "task {task} graph migration failed: {source}")
            }
            Self::InvalidGraph { task, errors } => write!(
                formatter,
                "task {task} has an invalid workflow graph: {}",
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }
}

impl std::error::Error for TaskMigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LegacySteps { source, .. } => Some(source),
            Self::WorkflowGraph { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StepCondition {
    ExitCode {
        step: String,
        codes: Vec<u32>,
    },
    Path {
        path: String,
        expect: PathExpectation,
    },
    All {
        conditions: Vec<StepCondition>,
    },
    Any {
        conditions: Vec<StepCondition>,
    },
    Not {
        condition: Box<StepCondition>,
    },
    /// A checked, side-effect-free rule over typed context from dominating
    /// steps. Indeterminate states are never implicitly coerced to false.
    Expression {
        rule: RuleExprV1,
        #[serde(default)]
        policy: RuleOutcomePolicy,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IndeterminatePolicy {
    #[default]
    Fail,
    TreatAsFalse,
    TreatAsTrue,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleOutcomePolicy {
    #[serde(default)]
    pub on_null: IndeterminatePolicy,
    #[serde(default)]
    pub on_missing: IndeterminatePolicy,
    #[serde(default)]
    pub on_unknown: IndeterminatePolicy,
}

impl StepCondition {
    fn validate(&self, step_id: &str) -> Result<(), String> {
        let mut nodes = 0usize;
        self.validate_inner(step_id, 1, &mut nodes)
    }

    fn validate_inner(&self, step_id: &str, depth: usize, nodes: &mut usize) -> Result<(), String> {
        const MAX_DEPTH: usize = 32;
        const MAX_NODES: usize = 256;

        if depth > MAX_DEPTH {
            return Err(format!(
                "step {} condition nesting exceeds maximum depth {}",
                step_id, MAX_DEPTH
            ));
        }
        *nodes = nodes.saturating_add(1);
        if *nodes > MAX_NODES {
            return Err(format!(
                "step {} condition tree exceeds maximum size of {} nodes",
                step_id, MAX_NODES
            ));
        }

        match self {
            Self::ExitCode { step, codes } => {
                if step.trim().is_empty() {
                    return Err(format!(
                        "step {} exit-code condition requires a source step",
                        step_id
                    ));
                }
                if codes.is_empty() {
                    return Err(format!(
                        "step {} exit-code condition requires at least one code",
                        step_id
                    ));
                }
                let mut seen = std::collections::BTreeSet::new();
                for code in codes {
                    if !seen.insert(code) {
                        return Err(format!(
                            "step {} exit-code condition contains duplicate code {}",
                            step_id, code
                        ));
                    }
                }
            }
            Self::Path { path, expect } => {
                if path.trim().is_empty() {
                    return Err(format!("step {} path condition requires a path", step_id));
                }
                expect.validate(step_id)?;
            }
            Self::All { conditions } | Self::Any { conditions } => {
                if conditions.is_empty() {
                    return Err(format!(
                        "step {} {} condition requires at least one child condition",
                        step_id,
                        match self {
                            Self::All { .. } => "all",
                            Self::Any { .. } => "any",
                            _ => unreachable!(),
                        }
                    ));
                }
                for condition in conditions {
                    condition.validate_inner(step_id, depth + 1, nodes)?;
                }
            }
            Self::Not { condition } => {
                condition.validate_inner(step_id, depth + 1, nodes)?;
            }
            Self::Expression { .. } => {
                // The expression checker owns its independent depth, node,
                // regex, and operation limits. It runs with the task's typed
                // upstream schema in `Task::validate_steps`.
            }
        }
        Ok(())
    }

    fn try_for_each_exit_code<F>(&self, visit: &mut F) -> Result<(), String>
    where
        F: FnMut(&str, &[u32]) -> Result<(), String>,
    {
        match self {
            Self::ExitCode { step, codes } => visit(step, codes),
            Self::Path { .. } => Ok(()),
            Self::Expression { .. } => Ok(()),
            Self::All { conditions } | Self::Any { conditions } => {
                for condition in conditions {
                    condition.try_for_each_exit_code(visit)?;
                }
                Ok(())
            }
            Self::Not { condition } => condition.try_for_each_exit_code(visit),
        }
    }

    fn try_for_each_rule<F>(&self, visit: &mut F) -> Result<(), String>
    where
        F: FnMut(&RuleExprV1) -> Result<(), String>,
    {
        match self {
            Self::Expression { rule, .. } => visit(rule),
            Self::All { conditions } | Self::Any { conditions } => {
                for condition in conditions {
                    condition.try_for_each_rule(visit)?;
                }
                Ok(())
            }
            Self::Not { condition } => condition.try_for_each_rule(visit),
            Self::ExitCode { .. } | Self::Path { .. } => Ok(()),
        }
    }

    fn prefix_source_step(&mut self, prefix: &str) {
        match self {
            Self::ExitCode { step, .. } => *step = format!("{prefix}/{step}"),
            Self::Path { .. } => {}
            Self::Expression { rule, .. } => {
                rule.visit_context_references_mut(|field| match &mut field.scope {
                    ContextScope::Step { step_id } | ContextScope::LoopItem { step_id } => {
                        *step_id = format!("{prefix}/{step_id}");
                    }
                    ContextScope::Scenario => {}
                });
            }
            Self::All { conditions } | Self::Any { conditions } => {
                for condition in conditions {
                    condition.prefix_source_step(prefix);
                }
            }
            Self::Not { condition } => condition.prefix_source_step(prefix),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Typed values copied from visible context into the action's declared
    /// input fields immediately before the step runs.
    ///
    /// Linear tasks are lowered to the v2 workflow graph when this map is not
    /// empty. Graph action nodes keep bindings in `ActionNode::bindings`, so
    /// their embedded step must leave this map empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, Binding>,
    #[serde(default)]
    pub auth: AuthPolicy,
    #[serde(default)]
    pub check: Option<Check>,
    #[serde(default)]
    pub dangerous: bool,
    #[serde(default)]
    pub allow_elevation: ElevationPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<StepCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<StepCondition>,
    #[serde(flatten)]
    pub action: Action,
}

#[derive(Deserialize)]
struct StepWire {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    bindings: BTreeMap<String, Binding>,
    #[serde(default)]
    auth: AuthPolicy,
    #[serde(default)]
    check: Option<Check>,
    #[serde(default)]
    dangerous: bool,
    #[serde(default)]
    allow_elevation: ElevationPolicy,
    #[serde(default)]
    when: Option<StepCondition>,
    #[serde(default)]
    require: Option<StepCondition>,
    #[serde(flatten)]
    action: BTreeMap<String, serde_json::Value>,
}

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = StepWire::deserialize(deserializer)?;
        let action =
            serde_json::from_value(serde_json::Value::Object(wire.action.into_iter().collect()))
                .map_err(|error| {
                    serde::de::Error::custom(format!(
                        "step {} has an invalid action: {error}",
                        wire.id
                    ))
                })?;
        Ok(Self {
            id: wire.id,
            name: wire.name,
            bindings: wire.bindings,
            auth: wire.auth,
            check: wire.check,
            dangerous: wire.dangerous,
            allow_elevation: wire.allow_elevation,
            when: wire.when,
            require: wire.require,
            action,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthPolicy {
    #[default]
    None,
    GitCredential,
    Sudo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LicenseProvider {
    LightBurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LicenseMethod {
    VendorUi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateLicenseAction {
    pub provider: LicenseProvider,
    pub method: LicenseMethod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppBundleIdentity {
    pub bundle_identifier: String,
    pub team_identifier: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppStoreOperation {
    #[default]
    Install,
    Get,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppStoreInstallAction {
    pub app_id: u64,
    #[serde(default)]
    pub operation: AppStoreOperation,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseChannel {
    #[default]
    Release,
    Beta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BambuStudioReleaseAction {
    #[serde(default)]
    pub channel: ReleaseChannel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveFormat {
    #[default]
    Auto,
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
}

fn default_archive_max_unpacked_bytes() -> u64 {
    10 * 1024 * 1024 * 1024
}

fn default_script_success_exit_codes() -> Vec<u32> {
    vec![0]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Action {
    GithubListRepositories,
    ForEach {
        source_step: String,
        array_path: String,
        item: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<String>,
    },
    ForEachGitCloneIfMissing {
        loop_step: String,
        repo: String,
        dest: String,
        #[serde(default)]
        branch: Option<String>,
    },
    CreateDirectory(CreateDirectoryAction),
    InspectPath(InspectPathAction),
    CopyPath(CopyPathAction),
    WriteFile(WriteFileAction),
    RemovePath(RemovePathAction),
    GitClone {
        repo: String,
        dest: String,
        #[serde(default)]
        branch: Option<String>,
    },
    GitInspect {
        repo: String,
        dest: String,
    },
    GitCloneIfMissing {
        repo: String,
        dest: String,
        #[serde(default)]
        branch: Option<String>,
    },
    GitFetch {
        repo: String,
        dest: String,
        branch: String,
    },
    GitFastForward {
        repo: String,
        dest: String,
        branch: String,
    },
    BrewInstall {
        package: String,
        #[serde(default)]
        cask: bool,
    },
    RunCommand {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        shell: ShellMode,
    },
    RunScript {
        interpreter: ScriptInterpreter,
        /// Path to a script file. Inline script bodies are intentionally not
        /// accepted so plans and task packs do not become secret-bearing
        /// arbitrary shell payloads.
        script: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default = "default_script_success_exit_codes")]
        success_exit_codes: Vec<u32>,
    },
    ConfigurePackageRegistryFiles {
        secrets: EncryptedSecretsSpec,
        npm: NpmRegistryFileSpec,
        nuget: NugetRegistryFileSpec,
    },
    DownloadFile {
        url: String,
        dest: String,
        checksum: Checksum,
    },
    ExtractArchive {
        src: String,
        dest: String,
        #[serde(default)]
        format: ArchiveFormat,
        #[serde(default = "default_archive_max_unpacked_bytes")]
        max_unpacked_bytes: u64,
    },
    InstallDmg {
        dmg: String,
        #[serde(default)]
        app_name: Option<String>,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        identity: Option<AppBundleIdentity>,
    },
    InstallPkg {
        pkg: String,
        #[serde(default)]
        target: Option<String>,
    },
    MacosRequirements {
        minimum_version: String,
        #[serde(default)]
        require_rosetta_on_apple_silicon: bool,
    },
    AppStoreInstall(AppStoreInstallAction),
    BambuStudioRelease(BambuStudioReleaseAction),
    ActivateLicense(ActivateLicenseAction),
}

impl Task {
    /// Normalize any supported legacy executable form into the only runtime
    /// representation: a validated, graph-only WorkflowGraph v3 task.
    pub fn into_v3(mut self) -> Result<Self, TaskMigrationError> {
        if !self.scenarios.is_empty() {
            if !self.steps.is_empty() || self.graph.is_some() {
                return Err(TaskMigrationError::MultipleExecutableForms {
                    task: self.id.clone(),
                });
            }
            return Err(TaskMigrationError::UnresolvedScenarios {
                task: self.id.clone(),
                scenarios: self.scenarios.clone(),
            });
        }

        let graph = match (self.steps.is_empty(), self.graph.take()) {
            (true, Some(graph)) => {
                graph
                    .into_v3()
                    .map_err(|source| TaskMigrationError::WorkflowGraph {
                        task: self.id.clone(),
                        source,
                    })?
            }
            (false, None) => LegacyTaskImporter::import_steps(&self.steps).map_err(|source| {
                TaskMigrationError::LegacySteps {
                    task: self.id.clone(),
                    source,
                }
            })?,
            (true, None) => {
                return Err(TaskMigrationError::NoExecutableForm {
                    task: self.id.clone(),
                });
            }
            (false, Some(_)) => {
                return Err(TaskMigrationError::MultipleExecutableForms {
                    task: self.id.clone(),
                });
            }
        };

        graph
            .validate()
            .map_err(|errors| TaskMigrationError::InvalidGraph {
                task: self.id.clone(),
                errors,
            })?;
        self.validate_metadata()
            .map_err(|message| TaskMigrationError::InvalidMetadata {
                task: self.id.clone(),
                message,
            })?;
        self.steps.clear();
        self.scenarios.clear();
        self.graph = Some(graph);
        Ok(self)
    }

    pub fn to_v3(&self) -> Result<Self, TaskMigrationError> {
        self.clone().into_v3()
    }

    /// Borrow the canonical runtime graph without performing implicit legacy
    /// lowering. Call [`Task::into_v3`] at an import boundary first.
    pub fn workflow_graph(&self) -> Result<&WorkflowGraph, TaskMigrationError> {
        if !self.steps.is_empty() || !self.scenarios.is_empty() {
            return Err(TaskMigrationError::MultipleExecutableForms {
                task: self.id.clone(),
            });
        }
        let graph = self
            .graph
            .as_ref()
            .ok_or_else(|| TaskMigrationError::NoExecutableForm {
                task: self.id.clone(),
            })?;
        if graph.version != WORKFLOW_GRAPH_VERSION {
            return Err(TaskMigrationError::WorkflowGraph {
                task: self.id.clone(),
                source: WorkflowGraphMigrationError::UnsupportedVersion {
                    path: "workflow_graph".into(),
                    found: graph.version,
                    minimum: crate::automation::graph::MIN_MIGRATABLE_WORKFLOW_GRAPH_VERSION,
                    current: WORKFLOW_GRAPH_VERSION,
                },
            });
        }
        graph
            .validate()
            .map_err(|errors| TaskMigrationError::InvalidGraph {
                task: self.id.clone(),
                errors,
            })?;
        Ok(graph)
    }

    pub fn is_v3(&self) -> bool {
        self.workflow_graph().is_ok()
    }

    pub fn validate(&self) -> Result<(), String> {
        self.validate_metadata()?;
        let executable_forms = usize::from(!self.steps.is_empty())
            + usize::from(!self.scenarios.is_empty())
            + usize::from(self.graph.is_some());
        if executable_forms == 0 {
            return Err(format!(
                "task {} has no steps, scenarios, or graph",
                self.id
            ));
        }
        if executable_forms > 1 {
            return Err(format!(
                "task {} must define exactly one of steps, scenarios, or graph",
                self.id
            ));
        }

        let mut scenario_ids = std::collections::BTreeSet::new();
        for scenario_id in &self.scenarios {
            if scenario_id.trim().is_empty() {
                return Err(format!(
                    "task {} contains an empty scenario reference",
                    self.id
                ));
            }
            if scenario_id.contains('/') {
                return Err(format!(
                    "task {} scenario reference {} must not contain '/'",
                    self.id, scenario_id
                ));
            }
            if !scenario_ids.insert(scenario_id) {
                return Err(format!(
                    "task {} includes scenario {} more than once",
                    self.id, scenario_id
                ));
            }
        }

        if !self.steps.is_empty() {
            self.validate_steps()?;
        }
        if let Some(graph) = &self.graph {
            graph
                .to_v3()
                .map_err(|error| format!("task {} graph migration failed: {error}", self.id))?
                .validate()
                .map_err(|errors| {
                    format!(
                        "task {} has an invalid workflow graph: {}",
                        self.id,
                        errors
                            .into_iter()
                            .map(|error| error.to_string())
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                })?;
        }
        Ok(())
    }

    pub fn validate_executable(&self) -> Result<(), String> {
        self.workflow_graph()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn validate_metadata(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("task id must not be empty".into());
        }
        if self.id.contains('/') {
            return Err(format!("task {} id must not contain '/'", self.id));
        }
        if self.name.trim().is_empty() {
            return Err(format!("task {} name must not be empty", self.id));
        }
        if self.description.trim().is_empty() {
            return Err(format!("task {} description must not be empty", self.id));
        }
        Ok(())
    }

    fn validate_steps(&self) -> Result<(), String> {
        let mut step_ids = std::collections::BTreeSet::<&str>::new();
        let mut foreach_ids = std::collections::BTreeSet::<&str>::new();
        let mut script_exit_codes = std::collections::BTreeMap::<&str, &[u32]>::new();
        let mut context_schemas = ContextStore::default();
        for step in &self.steps {
            step.validate()?;
            if step_ids.contains(step.id.as_str()) {
                return Err(format!(
                    "task {} contains duplicate step id {}",
                    self.id, step.id
                ));
            }
            for condition in [step.when.as_ref(), step.require.as_ref()]
                .into_iter()
                .flatten()
            {
                condition.try_for_each_exit_code(&mut |source_id, codes| {
                    if !step_ids.contains(source_id) {
                        return Err(format!(
                            "step {} exit-code condition must reference an earlier step, got {}",
                            step.id, source_id
                        ));
                    }
                    let Some(success_codes) = script_exit_codes.get(source_id) else {
                        return Err(format!(
                            "step {} exit-code condition source {} is not a run-script step",
                            step.id, source_id
                        ));
                    };
                    for code in codes {
                        if !success_codes.contains(code) {
                            return Err(format!(
                                "step {} condition code {} is not listed in source step {} success_exit_codes",
                                step.id, code, source_id
                            ));
                        }
                    }
                    Ok(())
                })?;
                condition.try_for_each_rule(&mut |rule| {
                    check_rule(rule.clone(), &context_schemas, ExpressionLimits::default())
                        .map(|_| ())
                        .map_err(|diagnostics| {
                            let details = diagnostics
                                .into_iter()
                                .map(|diagnostic| {
                                    format!(
                                        "{:?} at {}: {}",
                                        diagnostic.code, diagnostic.location, diagnostic.message
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("; ");
                            format!("step {} has an invalid context rule: {details}", step.id)
                        })
                })?;
            }
            match &step.action {
                Action::ForEach { source_step, .. } => {
                    if !step_ids.contains(source_step.as_str()) {
                        return Err(format!(
                            "step {} for-each must reference an earlier source step, got {}",
                            step.id, source_step
                        ));
                    }
                    foreach_ids.insert(step.id.as_str());
                }
                Action::ForEachGitCloneIfMissing { loop_step, .. }
                    if !foreach_ids.contains(loop_step.as_str()) =>
                {
                    return Err(format!(
                        "step {} foreach clone must reference an earlier for-each step, got {}",
                        step.id, loop_step
                    ));
                }
                _ => {}
            }
            step_ids.insert(step.id.as_str());
            let definition = definition_for_action(&step.action);
            context_schemas.insert(
                ContextScope::Step {
                    step_id: step.id.clone(),
                },
                ContextValue::new(serde_json::Value::Null, ContextProvenance::step(&step.id))
                    .with_schema(definition.output_schema),
            );
            if let Action::RunScript {
                success_exit_codes, ..
            } = &step.action
            {
                script_exit_codes.insert(step.id.as_str(), success_exit_codes);
            }
        }
        if self.steps.iter().any(|step| !step.bindings.is_empty()) {
            LegacyTaskImporter::import_steps(&self.steps).map_err(|error| {
                format!(
                    "task {} has linear input bindings that cannot be lowered: {error}",
                    self.id
                )
            })?;
        }
        Ok(())
    }

    pub fn is_template(&self) -> bool {
        !self.scenarios.is_empty() || !self.resolved_scenarios.is_empty()
    }

    pub fn included_scenarios(&self) -> &[String] {
        if self.scenarios.is_empty() {
            &self.resolved_scenarios
        } else {
            &self.scenarios
        }
    }
}

impl Step {
    pub(crate) fn prefix_condition_step(&mut self, prefix: &str) {
        if let Some(condition) = &mut self.when {
            condition.prefix_source_step(prefix);
        }
        if let Some(condition) = &mut self.require {
            condition.prefix_source_step(prefix);
        }
        for binding in self.bindings.values_mut() {
            prefix_binding_source_steps(binding, prefix);
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("step id must not be empty".into());
        }
        if let Some(condition) = &self.when {
            condition.validate(&self.id)?;
        }
        if let Some(condition) = &self.require {
            condition.validate(&self.id)?;
        }
        match &self.action {
            Action::GithubListRepositories => {
                if !matches!(self.auth, AuthPolicy::None)
                    || !matches!(self.allow_elevation, ElevationPolicy::Forbidden)
                    || self.dangerous
                {
                    return Err(format!(
                        "step {} github-list-repositories must be read-only and must not request authentication or elevation",
                        self.id
                    ));
                }
            }
            Action::ForEach {
                source_step,
                array_path,
                item,
                fields,
            } => {
                if !self.bindings.is_empty() {
                    return Err(format!(
                        "step {} legacy for-each cannot declare typed input bindings; migrate the loop to graph v2",
                        self.id
                    ));
                }
                if source_step.trim().is_empty()
                    || array_path.trim().is_empty()
                    || item.trim().is_empty()
                {
                    return Err(format!(
                        "step {} for-each requires source_step, array_path, and item",
                        self.id
                    ));
                }
                if source_step == &self.id {
                    return Err(format!("step {} for-each cannot reference itself", self.id));
                }
                let mut unique_fields = std::collections::BTreeSet::new();
                for field in fields {
                    if field.trim().is_empty()
                        || field.contains('.')
                        || !unique_fields.insert(field.as_str())
                    {
                        return Err(format!(
                            "step {} for-each fields must be unique non-empty object keys",
                            self.id
                        ));
                    }
                }
            }
            Action::ForEachGitCloneIfMissing {
                loop_step,
                repo,
                dest,
                ..
            } => {
                if !self.bindings.is_empty() {
                    return Err(format!(
                        "step {} legacy foreach clone cannot declare typed input bindings; migrate the loop to graph v2",
                        self.id
                    ));
                }
                if loop_step.trim().is_empty() || repo.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!(
                        "step {} foreach clone requires loop_step, repo, and dest",
                        self.id
                    ));
                }
                if loop_step == &self.id {
                    return Err(format!(
                        "step {} foreach clone cannot reference itself",
                        self.id
                    ));
                }
            }
            Action::CreateDirectory(action) => {
                if action.path.trim().is_empty() {
                    return Err(format!("step {} requires path", self.id));
                }
                self.validate_typed_filesystem_policy("create-directory")?;
            }
            Action::InspectPath(action) => {
                if action.path.trim().is_empty() {
                    return Err(format!("step {} requires path", self.id));
                }
                self.validate_typed_filesystem_policy("inspect-path")?;
                if let Some(expectation) = &action.expect {
                    expectation.validate(&self.id)?;
                }
            }
            Action::CopyPath(action) => {
                if action.src.trim().is_empty() || action.dest.trim().is_empty() {
                    return Err(format!("step {} requires src and dest", self.id));
                }
                if action.src == action.dest {
                    return Err(format!(
                        "step {} copy-path src and dest must be different",
                        self.id
                    ));
                }
                self.validate_typed_filesystem_policy("copy-path")?;
            }
            Action::WriteFile(action) => {
                const MAX_CONTENT_BYTES: usize = 1024 * 1024;

                if action.path.trim().is_empty() {
                    return Err(format!("step {} requires path", self.id));
                }
                if action.content.len() > MAX_CONTENT_BYTES {
                    return Err(format!(
                        "step {} write-file content must not exceed {} bytes",
                        self.id, MAX_CONTENT_BYTES
                    ));
                }
                if action.content.contains('\0') {
                    return Err(format!(
                        "step {} write-file content must not contain NUL bytes",
                        self.id
                    ));
                }
                self.validate_typed_filesystem_policy("write-file")?;
            }
            Action::RemovePath(action) => {
                if action.path.trim().is_empty() {
                    return Err(format!("step {} requires path", self.id));
                }
                self.validate_typed_filesystem_policy("remove-path")?;
            }
            Action::GitClone { repo, dest, branch } => {
                if repo.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!("step {} requires repo and dest", self.id));
                }
                if branch
                    .as_ref()
                    .is_some_and(|branch| branch.trim().is_empty())
                {
                    return Err(format!("step {} git branch must not be empty", self.id));
                }
            }
            Action::GitInspect { repo, dest } => {
                if repo.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!("step {} requires repo and dest", self.id));
                }
            }
            Action::GitCloneIfMissing { repo, dest, branch } => {
                if repo.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!("step {} requires repo and dest", self.id));
                }
                if branch
                    .as_ref()
                    .is_some_and(|branch| branch.trim().is_empty())
                {
                    return Err(format!("step {} git branch must not be empty", self.id));
                }
            }
            Action::GitFetch { repo, dest, branch }
            | Action::GitFastForward { repo, dest, branch } => {
                if repo.trim().is_empty() || dest.trim().is_empty() || branch.trim().is_empty() {
                    return Err(format!("step {} requires repo, dest, and branch", self.id));
                }
            }
            Action::BrewInstall { package, .. } => {
                if package.trim().is_empty() {
                    return Err(format!("step {} requires package", self.id));
                }
            }
            Action::RunCommand { program, shell, .. } => {
                if program.trim().is_empty() {
                    return Err(format!("step {} requires program", self.id));
                }
                if matches!(shell, ShellMode::Allow) && !self.dangerous {
                    return Err(format!(
                        "step {} enables shell mode but is not marked dangerous",
                        self.id
                    ));
                }
            }
            Action::RunScript {
                script,
                cwd,
                success_exit_codes,
                ..
            } => {
                if script.trim().is_empty() {
                    return Err(format!("step {} requires script", self.id));
                }
                if script.contains(['\n', '\r']) {
                    return Err(format!(
                        "step {} script must be a file path, not inline source",
                        self.id
                    ));
                }
                if cwd
                    .as_deref()
                    .is_some_and(|directory| directory.trim().is_empty())
                {
                    return Err(format!("step {} script cwd must not be empty", self.id));
                }
                if !self.dangerous {
                    return Err(format!(
                        "step {} runs a script but is not marked dangerous",
                        self.id
                    ));
                }
                if success_exit_codes.is_empty() {
                    return Err(format!(
                        "step {} success_exit_codes must contain at least one exit code",
                        self.id
                    ));
                }
                let mut seen_exit_codes = std::collections::BTreeSet::new();
                for code in success_exit_codes {
                    if !seen_exit_codes.insert(code) {
                        return Err(format!(
                            "step {} success_exit_codes contains duplicate exit code {}",
                            self.id, code
                        ));
                    }
                }
            }
            Action::ConfigurePackageRegistryFiles {
                secrets,
                npm,
                nuget,
            } => {
                validate_secret_profile(&secrets.profile, &self.id, "secrets.profile")?;
                validate_env_name(&secrets.username_env, &self.id, "secrets.username_env")?;
                validate_env_name(&secrets.token_env, &self.id, "secrets.token_env")?;
                if secrets.username_env == secrets.token_env
                    || is_reserved_secret_env(&secrets.username_env)
                    || is_reserved_secret_env(&secrets.token_env)
                {
                    return Err(format!(
                        "step {} requires distinct, non-reserved secret environment names",
                        self.id
                    ));
                }
                validate_https_url(&npm.registry, &self.id, "npm.registry")?;
                validate_https_url(&nuget.public_source, &self.id, "nuget.public_source")?;
                validate_https_url(&nuget.source, &self.id, "nuget.source")?;
                if urls_equal(&nuget.public_source, &nuget.source) {
                    return Err(format!(
                        "step {} requires distinct public and private NuGet sources",
                        self.id
                    ));
                }
                if !is_valid_npm_scope(&npm.scope) {
                    return Err(format!(
                        "step {} requires npm.scope in @namespace form",
                        self.id
                    ));
                }
                validate_xml_name(
                    &nuget.public_source_name,
                    &self.id,
                    "nuget.public_source_name",
                )?;
                validate_xml_name(&nuget.source_name, &self.id, "nuget.source_name")?;
                if nuget
                    .public_source_name
                    .eq_ignore_ascii_case(&nuget.source_name)
                {
                    return Err(format!(
                        "step {} requires distinct NuGet source names",
                        self.id
                    ));
                }
                if nuget.package_patterns.is_empty()
                    || nuget
                        .package_patterns
                        .iter()
                        .any(|pattern| !is_valid_nuget_package_pattern(pattern))
                {
                    return Err(format!(
                        "step {} requires specific NuGet package prefix patterns",
                        self.id
                    ));
                }
            }
            Action::DownloadFile {
                url,
                dest,
                checksum,
            } => {
                if url.trim().is_empty()
                    || dest.trim().is_empty()
                    || checksum.sha256.trim().is_empty()
                {
                    return Err(format!(
                        "step {} requires url, dest, and checksum.sha256",
                        self.id
                    ));
                }
            }
            Action::ExtractArchive {
                src,
                dest,
                max_unpacked_bytes,
                ..
            } => {
                if src.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!("step {} requires src and dest", self.id));
                }
                if *max_unpacked_bytes == 0 {
                    return Err(format!(
                        "step {} max_unpacked_bytes must be greater than zero",
                        self.id
                    ));
                }
            }
            Action::InstallDmg {
                dmg,
                app_name,
                target,
                identity,
            } => {
                if dmg.trim().is_empty() {
                    return Err(format!("step {} requires dmg", self.id));
                }
                if let Some(app_name) = app_name {
                    let path = std::path::Path::new(app_name);
                    if app_name.trim().is_empty()
                        || path.components().count() != 1
                        || path.extension().and_then(|ext| ext.to_str()) != Some("app")
                    {
                        return Err(format!(
                            "step {} app_name must be a single .app bundle name",
                            self.id
                        ));
                    }
                }
                if target
                    .as_deref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    return Err(format!("step {} target must not be empty", self.id));
                }
                if let Some(identity) = identity {
                    if app_name.is_none() {
                        return Err(format!(
                            "step {} requires app_name when identity is set",
                            self.id
                        ));
                    }
                    if !valid_bundle_identifier(&identity.bundle_identifier) {
                        return Err(format!(
                            "step {} identity.bundle_identifier is invalid",
                            self.id
                        ));
                    }
                    if identity.team_identifier.is_empty()
                        || !identity
                            .team_identifier
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric())
                    {
                        return Err(format!(
                            "step {} identity.team_identifier is invalid",
                            self.id
                        ));
                    }
                    if !valid_version(&identity.version) {
                        return Err(format!("step {} identity.version is invalid", self.id));
                    }
                }
            }
            Action::InstallPkg { pkg, .. } => {
                if pkg.trim().is_empty() {
                    return Err(format!("step {} requires pkg", self.id));
                }
            }
            Action::BambuStudioRelease(_) => {}
            Action::MacosRequirements {
                minimum_version, ..
            } => {
                if !valid_version(minimum_version) {
                    return Err(format!(
                        "step {} minimum_version must contain dot-separated integers",
                        self.id
                    ));
                }
            }
            Action::AppStoreInstall(action) => {
                if action.app_id == 0 {
                    return Err(format!("step {} app_id must be greater than zero", self.id));
                }
                if !matches!(self.auth, AuthPolicy::None)
                    || !matches!(self.allow_elevation, ElevationPolicy::Forbidden)
                    || self.dangerous
                {
                    return Err(format!(
                        "step {} app-store-install must not request authentication, elevation, or dangerous execution",
                        self.id
                    ));
                }
            }
            Action::ActivateLicense(_) => {}
        }
        Ok(())
    }

    fn validate_typed_filesystem_policy(&self, action: &str) -> Result<(), String> {
        if !matches!(self.auth, AuthPolicy::None)
            || !matches!(self.allow_elevation, ElevationPolicy::Forbidden)
        {
            return Err(format!(
                "step {} {} must not request authentication or elevation",
                self.id, action
            ));
        }
        if self.dangerous {
            return Err(format!(
                "step {} {} is typed and must not be marked dangerous",
                self.id, action
            ));
        }
        if self.check.is_some() {
            return Err(format!(
                "step {} {} must not use check; use the typed action's intrinsic idempotency or expect assertions",
                self.id, action
            ));
        }
        Ok(())
    }
}

fn prefix_binding_source_steps(binding: &mut Binding, prefix: &str) {
    let prefix_field = |field: &mut crate::automation::context::FieldRef| match &mut field.scope {
        ContextScope::Step { step_id } | ContextScope::LoopItem { step_id } => {
            *step_id = format!("{prefix}/{step_id}");
        }
        ContextScope::Scenario => {}
    };
    match binding {
        Binding::Field { field } => prefix_field(field),
        Binding::Interpolated { parts } => {
            for part in parts {
                if let TemplatePart::Field { field } = part {
                    prefix_field(field);
                }
            }
        }
        Binding::Literal { .. } | Binding::Template { .. } => {}
    }
}

fn validate_https_url(value: &str, step_id: &str, field: &str) -> Result<(), String> {
    let trimmed = value.trim();
    let Some(remainder) = trimmed.strip_prefix("https://") else {
        return Err(format!(
            "step {step_id} requires {field} to be an HTTPS URL"
        ));
    };
    let (authority, path) = remainder
        .split_once('/')
        .map_or((remainder, ""), |(authority, path)| (authority, path));
    if trimmed != value
        || !is_valid_url_authority(authority)
        || !is_valid_url_path(path)
        || remainder.contains('\\')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '$' | '{' | '}' | '%'))
    {
        return Err(format!(
            "step {step_id} requires {field} to be an HTTPS URL"
        ));
    }
    Ok(())
}

fn is_valid_url_authority(value: &str) -> bool {
    if value.is_empty() || value.contains('@') {
        return false;
    }
    if let Some(ipv6) = value.strip_prefix('[') {
        let Some((address, port)) = ipv6.split_once(']') else {
            return false;
        };
        return address.parse::<std::net::Ipv6Addr>().is_ok() && is_valid_port_suffix(port);
    }
    if value.matches(':').count() > 1 {
        return false;
    }
    let (host, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    is_valid_dns_host(host) && port.map(is_valid_port).unwrap_or(true)
}

fn is_valid_port_suffix(value: &str) -> bool {
    value.is_empty() || value.strip_prefix(':').map(is_valid_port).unwrap_or(false)
}

fn is_valid_port(value: &str) -> bool {
    value.parse::<u16>().map(|port| port != 0).unwrap_or(false)
}

fn is_valid_dns_host(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 {
        return false;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && label
                .chars()
                .next()
                .map(|c| c.is_ascii_alphanumeric())
                .unwrap_or(false)
            && label
                .chars()
                .last()
                .map(|c| c.is_ascii_alphanumeric())
                .unwrap_or(false)
    })
}

fn is_valid_url_path(value: &str) -> bool {
    value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '/' | '-'
                    | '.'
                    | '_'
                    | '~'
                    | '!'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | ':'
                    | '@'
            )
    })
}

fn urls_equal(left: &str, right: &str) -> bool {
    left.trim_end_matches('/')
        .eq_ignore_ascii_case(right.trim_end_matches('/'))
}

fn validate_xml_name(value: &str, step_id: &str, field: &str) -> Result<(), String> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(format!("step {step_id} requires {field}"));
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(format!(
            "step {step_id} requires {field} to be a portable XML name"
        ));
    }
    Ok(())
}

fn is_valid_nuget_package_pattern(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "*" || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let star_count = trimmed.chars().filter(|c| *c == '*').count();
    (star_count == 0 || (star_count == 1 && trimmed.ends_with('*')))
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '*'))
}

fn is_valid_npm_scope(value: &str) -> bool {
    let Some(namespace) = value.strip_prefix('@') else {
        return false;
    };
    !namespace.is_empty()
        && namespace
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

fn validate_secret_profile(value: &str, step_id: &str, field: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let valid = bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.len() <= 64
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        });
    if !valid {
        return Err(format!(
            "step {step_id} requires {field} to match [a-z0-9][a-z0-9._-]{{0,63}}"
        ));
    }
    Ok(())
}

fn validate_env_name(value: &str, step_id: &str, field: &str) -> Result<(), String> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(format!("step {step_id} requires {field}"));
    };
    if !(first.is_ascii_uppercase() || first == '_')
        || !chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!(
            "step {step_id} requires {field} to be an uppercase environment variable name"
        ));
    }
    Ok(())
}

fn is_reserved_secret_env(value: &str) -> bool {
    matches!(
        value,
        "PATH"
            | "HOME"
            | "USERPROFILE"
            | "NODE_OPTIONS"
            | "NODE_PATH"
            | "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "LD_AUDIT"
            | "PPDUSTER_AUDIT_LOG"
    ) || value.starts_with("NPM_CONFIG_")
        || value.starts_with("DOTNET_")
        || value.starts_with("MSBUILD")
        || value.starts_with("DYLD_")
        || value.starts_with("NUGET_")
}

fn valid_version(value: &str) -> bool {
    !value.trim().is_empty()
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

fn valid_bundle_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_registry_step() -> Step {
        Step {
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
                secrets: EncryptedSecretsSpec {
                    profile: "github-packages".into(),
                    username_env: "GITHUB_PACKAGES_USER".into(),
                    token_env: "GITHUB_PACKAGES_TOKEN".into(),
                },
                npm: NpmRegistryFileSpec {
                    scope: "@dodopizza".into(),
                    registry: "https://npm.pkg.github.com/".into(),
                },
                nuget: NugetRegistryFileSpec {
                    public_source_name: "nuget.org".into(),
                    public_source: "https://api.nuget.org/v3/index.json".into(),
                    source_name: "github".into(),
                    source: "https://nuget.pkg.github.com/dodopizza/index.json".into(),
                    package_patterns: vec!["Dodo.*".into()],
                },
            },
        }
    }

    #[test]
    fn package_registry_action_validates() {
        package_registry_step().validate().unwrap();
    }

    #[test]
    fn package_registry_action_rejects_broad_nuget_mapping() {
        let mut step = package_registry_step();
        let Action::ConfigurePackageRegistryFiles { nuget, .. } = &mut step.action else {
            unreachable!()
        };
        nuget.package_patterns = vec!["*".into()];

        let err = step.validate().unwrap_err();
        assert!(err.contains("specific NuGet package prefix patterns"));
    }

    #[test]
    fn package_registry_action_rejects_secret_bearing_or_insecure_urls() {
        for invalid in [
            "http://npm.pkg.github.com/",
            "https://token@npm.pkg.github.com/",
            "https://npm.pkg.github.com/?token=secret",
            "https://npm.pkg.github.com/#secret",
            "https://:443/",
            "https://[]/",
            "https://npm.pkg.github.com/${AWS_SECRET_ACCESS_KEY}/",
            "https://npm.pkg.github.com/%AWS_SECRET_ACCESS_KEY%/",
            "https://npm.pkg.github.com/\u{0000}/",
        ] {
            let mut step = package_registry_step();
            let Action::ConfigurePackageRegistryFiles { npm, .. } = &mut step.action else {
                unreachable!()
            };
            npm.registry = invalid.into();
            assert!(step.validate().is_err(), "accepted invalid URL: {invalid}");
        }
    }

    #[test]
    fn package_registry_action_rejects_nonportable_env_names() {
        let mut step = package_registry_step();
        let Action::ConfigurePackageRegistryFiles { secrets, .. } = &mut step.action else {
            unreachable!()
        };
        secrets.token_env = "github-token".into();

        let err = step.validate().unwrap_err();
        assert!(err.contains("uppercase environment variable name"));
    }

    #[test]
    fn package_registry_action_rejects_reserved_secret_env_names() {
        let mut step = package_registry_step();
        let Action::ConfigurePackageRegistryFiles { secrets, .. } = &mut step.action else {
            unreachable!()
        };
        secrets.token_env = "PATH".into();

        let err = step.validate().unwrap_err();
        assert!(err.contains("non-reserved secret environment names"));
    }

    #[test]
    fn package_registry_action_rejects_invalid_secret_profiles() {
        for invalid in [
            "",
            "GitHub-packages",
            "-github-packages",
            "github packages",
            "github/packages",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let mut step = package_registry_step();
            let Action::ConfigurePackageRegistryFiles { secrets, .. } = &mut step.action else {
                unreachable!()
            };
            secrets.profile = invalid.into();

            let err = step.validate().unwrap_err();
            assert!(err.contains("[a-z0-9][a-z0-9._-]{0,63}"));
        }
    }

    #[test]
    fn foreach_actions_validate_order_and_round_trip() {
        let task = Task {
            id: "clone-account-repositories".into(),
            name: "Clone repositories".into(),
            description: "Clone every repository returned by GitHub.".into(),
            platform: crate::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![
                Step {
                    id: "repositories".into(),
                    name: String::new(),
                    bindings: BTreeMap::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    when: None,
                    require: None,
                    action: Action::GithubListRepositories,
                },
                Step {
                    id: "repositories-loop".into(),
                    name: String::new(),
                    bindings: BTreeMap::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    when: None,
                    require: None,
                    action: Action::ForEach {
                        source_step: "repositories".into(),
                        array_path: "github.repositories".into(),
                        item: "repository".into(),
                        fields: vec![
                            "https_url".into(),
                            "owner".into(),
                            "name".into(),
                            "default_branch".into(),
                        ],
                    },
                },
                Step {
                    id: "clone".into(),
                    name: String::new(),
                    bindings: BTreeMap::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    when: None,
                    require: None,
                    action: Action::ForEachGitCloneIfMissing {
                        loop_step: "repositories-loop".into(),
                        repo: "{{repository.https_url}}".into(),
                        dest: "$HOME/Developer/{{repository.owner}}/{{repository.name}}".into(),
                        branch: None,
                    },
                },
            ],
            graph: None,
        };

        task.validate().unwrap();
        let canonical = task.to_v3().unwrap();
        let yaml = serde_yaml::to_string(&canonical).unwrap();
        assert!(yaml.contains("format_version: 3"));
        assert!(yaml.contains("workflow_graph:"));
        assert!(yaml.contains("kind: for-each"));
        assert!(yaml.contains("type: git-clone-if-missing"));
        assert!(!yaml.contains("\nsteps:"));
        assert!(!yaml.contains("\ngraph:"));
        let round_trip = serde_yaml::from_str::<Task>(&yaml).unwrap();
        assert!(round_trip.is_v3());
    }

    #[test]
    fn legacy_task_yaml_is_imported_and_serialized_only_as_v3() {
        let yaml = r#"
id: legacy-linear
name: Legacy linear
description: Existing v1 task.
trust: external-allowed
steps:
  - id: repositories
    type: github-list-repositories
"#;
        let task: Task = serde_yaml::from_str(yaml).unwrap();
        assert!(task.steps.is_empty());
        assert_eq!(task.graph.as_ref().unwrap().version, WORKFLOW_GRAPH_VERSION);
        task.validate().unwrap();

        let serialized = serde_yaml::to_string(&task).unwrap();
        assert!(serialized.contains("format_version: 3"));
        assert!(serialized.contains("workflow_graph:"));
        assert!(!serialized.contains("\nsteps:"));
        assert!(!serialized.contains("\ngraph:"));
        let round_trip: Task = serde_yaml::from_str(&serialized).unwrap();
        assert!(round_trip.steps.is_empty());
        assert!(round_trip.is_v3());
    }

    #[test]
    fn linear_binding_yaml_round_trips_a_positional_field_reference() {
        let yaml = r#"
id: indexed-binding
name: Indexed binding
description: Inspect the third visible repository.
trust: external-allowed
steps:
  - id: list-repositories
    type: github-list-repositories
  - id: inspect-repository
    bindings:
      repo:
        kind: field
        field:
          scope:
            kind: step
            step_id: list-repositories
          segments:
            - kind: field
              name: github
            - kind: field
              name: repositories
            - kind: index
              index: 2
            - kind: field
              name: https_url
    type: git-inspect
    repo: https://github.com/example/repository.git
    dest: /tmp/repository
"#;
        let task: Task = serde_yaml::from_str(yaml).unwrap();
        task.validate().unwrap();
        let graph = task.workflow_graph().unwrap();
        let crate::automation::graph::GraphNode::Action(action) = &graph.nodes[1] else {
            panic!("expected imported action node")
        };
        let Binding::Field { field } = &action.bindings["repo"] else {
            panic!("expected field binding")
        };
        assert_eq!(
            field.segments,
            vec![
                crate::automation::context::ContextPathSegment::field("github"),
                crate::automation::context::ContextPathSegment::field("repositories"),
                crate::automation::context::ContextPathSegment::index(2),
                crate::automation::context::ContextPathSegment::field("https_url"),
            ]
        );

        let serialized = serde_yaml::to_string(&task).unwrap();
        let round_trip: Task = serde_yaml::from_str(&serialized).unwrap();
        let crate::automation::graph::GraphNode::Action(round_trip_action) =
            &round_trip.workflow_graph().unwrap().nodes[1]
        else {
            panic!("expected round-trip action node")
        };
        assert_eq!(round_trip_action.bindings, action.bindings);
        round_trip.validate().unwrap();
    }

    #[test]
    fn linear_foreach_validates_an_immediate_loop_item_consumer() {
        let mut list = package_registry_step();
        list.id = "repositories".into();
        list.action = Action::GithubListRepositories;

        let mut loop_step = package_registry_step();
        loop_step.id = "repositories-loop".into();
        loop_step.action = Action::ForEach {
            source_step: "repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["https_url".into()],
        };

        let mut inspect = package_registry_step();
        inspect.id = "inspect".into();
        inspect.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        inspect.bindings.insert(
            "repo".into(),
            Binding::field(
                crate::automation::context::FieldRef::loop_item("repositories-loop")
                    .field("https_url"),
            ),
        );

        let task = Task {
            id: "foreach-item-consumer".into(),
            name: "For each item consumer".into(),
            description: "Consume one repository inside the loop body.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![list, loop_step, inspect],
            graph: None,
        };

        task.validate().unwrap();
    }

    #[test]
    fn linear_binding_rejects_a_future_context_source() {
        let mut inspect = package_registry_step();
        inspect.id = "inspect".into();
        inspect.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        inspect.bindings.insert(
            "repo".into(),
            Binding::field(
                crate::automation::context::FieldRef::step("list")
                    .field("github")
                    .field("repositories")
                    .index(2)
                    .field("https_url"),
            ),
        );
        let mut list = inspect.clone();
        list.id = "list".into();
        list.bindings.clear();
        list.action = Action::GithubListRepositories;
        let task = Task {
            id: "future-source".into(),
            name: "Future source".into(),
            description: "Reject a non-dominating source.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![inspect, list],
            graph: None,
        };

        let error = task.validate().unwrap_err();
        assert!(error.contains("does not dominate"), "{error}");
    }

    #[test]
    fn linear_binding_rejects_a_semantically_wrong_source_type() {
        let mut list = package_registry_step();
        list.id = "list".into();
        list.action = Action::GithubListRepositories;
        let mut inspect = list.clone();
        inspect.id = "inspect".into();
        inspect.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        inspect.bindings.insert(
            "repo".into(),
            Binding::field(
                crate::automation::context::FieldRef::step("list")
                    .field("github")
                    .field("repositories")
                    .index(2)
                    .field("name"),
            ),
        );
        let task = Task {
            id: "wrong-source-type".into(),
            name: "Wrong source type".into(),
            description: "Reject a repository name used as a Git URL.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![list, inspect],
            graph: None,
        };

        let error = task.validate().unwrap_err();
        assert!(error.contains("expects"), "{error}");
    }

    #[test]
    fn action_yaml_rejects_unknown_fields_instead_of_using_unsafe_defaults() {
        let yaml = r#"
id: typo
type: extract-archive
src: archive.zip
dest: unpacked
max_unpack_bytes: 1024
"#;

        let error = serde_yaml::from_str::<Step>(yaml).unwrap_err();
        let error = error.to_string();
        assert!(error.contains("step typo"));
        assert!(error.contains("max_unpack_bytes"));

        let valid = yaml.replace("max_unpack_bytes", "max_unpacked_bytes");
        let step = serde_yaml::from_str::<Step>(&valid).unwrap();
        assert!(matches!(
            step.action,
            Action::ExtractArchive {
                max_unpacked_bytes: 1024,
                ..
            }
        ));
    }

    #[test]
    fn task_requires_exactly_one_executable_form() {
        let step = Step {
            id: "repositories".into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::GithubListRepositories,
        };
        let graph = LegacyTaskImporter::import_steps(std::slice::from_ref(&step)).unwrap();
        let mut task = Task {
            id: "exclusive".into(),
            name: "Exclusive".into(),
            description: "Exactly one executable form.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![step],
            graph: Some(graph),
        };

        let error = task.validate().unwrap_err();
        assert!(error.contains("exactly one of steps, scenarios, or graph"));

        task.steps.clear();
        task.validate().unwrap();
        task.scenarios.push("child".into());
        let error = task.validate().unwrap_err();
        assert!(error.contains("exactly one of steps, scenarios, or graph"));
    }
}

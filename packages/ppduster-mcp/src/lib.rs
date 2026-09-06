#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use ppduster::automation::{
    block_definitions, load_project_yaml, make_project_external, run_task, validate_project,
    CanvasPoint, ComposerCanvas, GithubRepositoryInput, GraphNode, ProjectEntry, RunOptions,
    ScenarioProject, ScenarioProjectFile, Step, Task, TrustRequirement, WorkflowGraph,
};
use ppduster::rules::Platform;
use rmcp::schemars::{self, JsonSchema};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ProtocolVersion},
    tool, tool_handler, tool_router, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

const SCHEME_FORMAT_VERSION: u32 = 1;
const MAX_SCENARIOS: usize = 256;
const MAX_GROUP_DEPTH: usize = 16;
const MAX_STEPS_PER_SCENARIO: usize = 512;
const MAX_TOTAL_STEPS: usize = 4_096;
const MAX_SPEC_BYTES: usize = 8 * 1024 * 1024;
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2024_11_05,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2026_07_28,
];
const RESERVED_CANVAS_IDS: &[&str] = &["start"];
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SchemePlatform {
    #[default]
    Any,
    Macos,
    Linux,
    Windows,
}

impl From<SchemePlatform> for Platform {
    fn from(value: SchemePlatform) -> Self {
        match value {
            SchemePlatform::Any => Self::Any,
            SchemePlatform::Macos => Self::Macos,
            SchemePlatform::Linux => Self::Linux,
            SchemePlatform::Windows => Self::Windows,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupSpec {
    #[schemars(description = "Stable group identifier")]
    pub id: String,
    #[schemars(description = "Human-readable group name")]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScenarioSpec {
    #[schemars(description = "Stable scenario identifier; must not contain '/'")]
    pub id: String,
    pub name: String,
    #[schemars(description = "Useful overview of outcome, prerequisites, permissions, and limits")]
    pub description: String,
    #[serde(default)]
    pub platform: SchemePlatform,
    #[serde(default)]
    #[schemars(description = "Optional nested group path, from outermost to innermost")]
    pub group_path: Vec<GroupSpec>,
    #[schemars(
        description = "Ordered ppduster block declarations. Each object needs id, name, type, and the inputs declared by list_blocks. The MCP boundary compiles this order into explicit WorkflowGraph v3 edges."
    )]
    #[schemars(schema_with = "steps_input_schema")]
    #[serde(default)]
    pub steps: Vec<Value>,
    #[serde(default)]
    #[schemars(
        description = "Explicit WorkflowGraph v3, including nested loops, bindings, variables, and edges. Supply either workflow_graph or steps, never both."
    )]
    pub workflow_graph: Option<Value>,
}

fn steps_input_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let action_kinds = block_definitions()
        .into_iter()
        .map(|definition| definition.kind.id())
        .collect::<Vec<_>>();
    schemars::json_schema!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_STEPS_PER_SCENARIO,
        "items": {
            "type": "object",
            "required": ["id", "name", "type"],
            "properties": {
                "id": {
                    "type": "string",
                    "minLength": 1,
                    "pattern": "\\S",
                    "not": { "enum": RESERVED_CANVAS_IDS }
                },
                "name": { "type": "string", "minLength": 1, "pattern": "\\S" },
                "type": { "type": "string", "enum": action_kinds }
            },
            "additionalProperties": true
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SchemeSpec {
    #[schemars(description = "Stable project identifier")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[schemars(
        description = "One or more scenarios whose ordered block declarations are compiled to WorkflowGraph v3"
    )]
    pub scenarios: Vec<ScenarioSpec>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListBlocksRequest {
    #[schemars(description = "Optional exact action kind, for example create-directory")]
    pub kind: Option<String>,
    #[schemars(description = "Optional case-insensitive category filter")]
    pub category: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidateSchemeRequest {
    pub scheme: SchemeSpec,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateSchemeRequest {
    pub scheme: SchemeSpec,
    #[schemars(
        description = "Optional path relative to the configured output directory. Parent directory must exist; extension must be .yaml or .yml. Existing files are never replaced."
    )]
    pub output_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadSchemeRequest {
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunSchemeRequest {
    #[schemars(
        description = "Saved YAML path relative to output_root, as returned by create_scheme or create_github_scheme"
    )]
    pub path: String,
    pub scenario_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateGithubSchemeRequest {
    #[schemars(
        description = "Repository IDs selected from the most recent list_github_repositories result in this MCP session. Selection is frozen into the file, and never refreshed at runtime."
    )]
    pub repository_ids: Vec<String>,
    pub destination_root: String,
    pub output_path: String,
}

#[derive(Debug, Clone)]
pub struct PpdusterMcp {
    output_root: Arc<PathBuf>,
    output_dir: Arc<Dir>,
    allow_apply: bool,
    github_preview: Arc<Mutex<Option<Vec<GithubRepositoryInput>>>>,
}

impl PpdusterMcp {
    pub fn new(output_root: impl AsRef<Path>) -> anyhow::Result<Self> {
        let requested = output_root.as_ref();
        let directory = Dir::open_ambient_dir(requested, ambient_authority()).map_err(|error| {
            anyhow::anyhow!(
                "cannot use MCP output directory {}: {error}",
                requested.display()
            )
        })?;
        let root = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| anyhow::anyhow!("cannot resolve current directory: {error}"))?
                .join(requested)
        };
        Ok(Self {
            output_root: Arc::new(root),
            output_dir: Arc::new(directory),
            allow_apply: false,
            github_preview: Arc::new(Mutex::new(None)),
        })
    }

    pub fn with_apply(mut self, allow_apply: bool) -> Self {
        self.allow_apply = allow_apply;
        self
    }

    fn read_document(&self, requested_path: &str) -> Result<String, String> {
        let path = validate_output_path(requested_path)?;
        let file = self
            .output_dir
            .open(&path)
            .map_err(|e| format!("read project: {e}"))?;
        let mut yaml = String::new();
        file.take(MAX_SPEC_BYTES as u64 + 1)
            .read_to_string(&mut yaml)
            .map_err(|e| format!("read project: {e}"))?;
        if yaml.len() > MAX_SPEC_BYTES {
            return Err("project file exceeds size limit".into());
        }
        Ok(yaml)
    }

    fn read_project(&self, requested_path: &str) -> Result<ScenarioProject, String> {
        let yaml = self.read_document(requested_path)?;
        let mut project = load_project_yaml(&yaml).map_err(|e| format!("load project: {e:#}"))?;
        make_project_external(&mut project.entries);
        ppduster::automation::project::validate_project_for_editing(&project)?;
        Ok(project)
    }

    fn execute_saved(&self, request: RunSchemeRequest, apply: bool) -> CallToolResult {
        if apply && !self.allow_apply {
            return tool_error(
                "execution is disabled; start this server with --allow-apply to enable run_scheme",
            );
        }
        let result = (|| {
            let project = self.read_project(&request.path)?;
            let task = find_task(&project.entries, &request.scenario_id).ok_or_else(|| {
                format!(
                    "scenario {:?} not found in saved project",
                    request.scenario_id
                )
            })?;
            run_task(
                task,
                &RunOptions {
                    apply,
                    ..RunOptions::default()
                },
            )
            .map_err(|e| format!("{e:#}"))
        })();
        match result {
            Ok(report) => {
                let result =
                    json!({"success": report.errors.is_empty(), "apply": apply, "report": report});
                if report.errors.is_empty() {
                    CallToolResult::structured(result)
                } else {
                    CallToolResult::structured_error(result)
                }
            }
            Err(error) => CallToolResult::structured_error(
                json!({"success": false, "apply": apply, "error": error}),
            ),
        }
    }

    pub fn output_root(&self) -> &Path {
        self.output_root.as_path()
    }

    pub fn validate(&self, scheme: SchemeSpec) -> Result<(ScenarioProject, String), String> {
        let project = build_project(scheme)?;
        let yaml = project_yaml(&project)?;
        Ok((project, yaml))
    }

    pub fn create(
        &self,
        scheme: SchemeSpec,
        output_path: Option<&str>,
    ) -> Result<CreatedScheme, String> {
        let project = build_project(scheme)?;
        let yaml = project_yaml(&project)?;
        let requested_path = output_path
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| default_output_name(&project.id));
        let relative = validate_output_path(&requested_path)?;
        let target = self.output_root().join(&relative);
        let warnings = write_new_file(self.output_dir.as_ref(), &relative, yaml.as_bytes())?;

        Ok(CreatedScheme {
            path: target,
            project_id: project.id,
            scenario_count: count_scenarios(&project.entries),
            step_count: count_steps(&project.entries),
            bytes_written: yaml.len(),
            warnings,
        })
    }
}

#[derive(Debug)]
pub struct CreatedScheme {
    pub path: PathBuf,
    pub project_id: String,
    pub scenario_count: usize,
    pub step_count: usize,
    pub bytes_written: usize,
    pub warnings: Vec<String>,
}

#[tool_router]
impl PpdusterMcp {
    #[tool(
        description = "List repositories from the signed-in GitHub CLI account for authoring. Caches the preview in this MCP session. Returns IDs to select with create_github_scheme; private, archived, and empty repositories are marked unavailable. No scenario is run.",
        annotations(read_only_hint = true)
    )]
    fn list_github_repositories(&self) -> CallToolResult {
        match ppduster::github::get_account_repositories() {
            Ok(account) => {
                let repositories = account
                    .repositories
                    .iter()
                    .map(|repo| GithubRepositoryInput {
                        id: repo.id.clone(),
                        owner: repo.owner.clone(),
                        name: repo.name.clone(),
                        full_name: repo.name_with_owner.clone(),
                        https_url: repo.url.clone(),
                        ssh_url: repo.ssh_url.clone(),
                        default_branch: repo.default_branch.clone(),
                        private: repo.is_private,
                        archived: repo.is_archived,
                    })
                    .collect::<Vec<_>>();
                let items = repositories.iter().map(|repo| json!({
                    "id": repo.id, "full_name": repo.full_name,
                    "private": repo.private, "archived": repo.archived,
                    "default_branch": repo.default_branch,
                    "selectable": !repo.private && !repo.archived && repo.default_branch.is_some(),
                })).collect::<Vec<_>>();
                *self.github_preview.lock().unwrap() = Some(repositories);
                CallToolResult::structured(
                    json!({"repositories": items, "selection_is_saved_before_run": true}),
                )
            }
            Err(error) => tool_error(format!("GitHub authoring preview failed: {error:#}")),
        }
    }

    #[tool(
        description = "Save a GitHub clone-or-fetch scenario using the exact shared desktop recipe. First call list_github_repositories, select repository_ids, then call this with the destination folder and a new YAML output_path. Runtime uses only the persisted selection. Does not clone or fetch."
    )]
    fn create_github_scheme(
        &self,
        Parameters(request): Parameters<CreateGithubSchemeRequest>,
    ) -> CallToolResult {
        let result = (|| {
            let preview = self.github_preview.lock().map_err(|e| e.to_string())?;
            let preview = preview
                .as_ref()
                .ok_or("call list_github_repositories first in this MCP session")?;
            if request.repository_ids.is_empty() {
                return Err("select at least one repository".to_owned());
            }
            let mut unique = BTreeSet::new();
            let mut selected = Vec::new();
            for id in &request.repository_ids {
                if !unique.insert(id) {
                    return Err(format!("duplicate selected repository ID: {id}"));
                }
                let repository = preview.iter().find(|repo| &repo.id == id).ok_or_else(|| {
                    format!("selected repository ID is absent from the authoring preview: {id}")
                })?;
                if repository.private || repository.archived || repository.default_branch.is_none()
                {
                    return Err(format!("repository {id} is unavailable: this snapshot recipe supports public, active repositories with a default branch"));
                }
                selected.push(repository.clone());
            }
            let task = ppduster::automation::recipes::github_repository_task(
                1,
                selected,
                &request.destination_root,
            )?;
            self.create(
                SchemeSpec {
                    id: "github-repositories".into(),
                    name: task.name.clone(),
                    description: task.description.clone(),
                    scenarios: vec![ScenarioSpec {
                        id: task.id.clone(),
                        name: task.name,
                        description: task.description,
                        platform: SchemePlatform::Macos,
                        group_path: Vec::new(),
                        steps: Vec::new(),
                        workflow_graph: Some(
                            serde_json::to_value(task.graph.unwrap()).map_err(|e| e.to_string())?,
                        ),
                    }],
                },
                Some(&request.output_path),
            )
        })();
        match result {
            Ok(created) => CallToolResult::structured(
                json!({"created": true, "path": created.path, "relative_path": request.output_path, "scenario_id": "github-repositories-1", "warnings": created.warnings}),
            ),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Read and validate a saved project YAML below output_root, returning its canonical project and scenario IDs. Uses the same project loader as the desktop UI.",
        annotations(read_only_hint = true)
    )]
    fn read_scheme(&self, Parameters(request): Parameters<ReadSchemeRequest>) -> CallToolResult {
        let result = (|| {
            let yaml = self.read_document(&request.path)?;
            let project = load_project_yaml(&yaml).map_err(|e| format!("load project: {e:#}"))?;
            ppduster::automation::project::validate_project_for_editing(&project)?;
            let validation_error = validate_project(&project).err();
            // Task serialization deliberately rejects unfinished graphs. Report
            // the source document so reading a draft never panics or repairs it.
            let source: Value = serde_yaml::from_str(&yaml).map_err(|e| e.to_string())?;
            Ok::<_, String>(json!({
                "valid": validation_error.is_none(), "validation_error": validation_error,
                "project": source.get("project").unwrap_or(&source),
            }))
        })();
        match result {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Dry-run a scenario from its saved YAML with the real ppduster executor. Reports runtime binding and policy errors, exact per-repository plans, and step status. No clone, fetch, or other mutations are applied; GitHub selection is not refreshed.",
        annotations(read_only_hint = true)
    )]
    fn plan_scheme(&self, Parameters(request): Parameters<RunSchemeRequest>) -> CallToolResult {
        self.execute_saved(request, false)
    }

    #[tool(
        description = "Execute a scenario from saved YAML using the real ppduster executor; may clone/fetch repositories or perform other declared actions. Requires server --allow-apply. Run plan_scheme first and call only when the user authorized those actions. Shell, elevation, and protected-folder approvals remain disabled. GitHub selection is not refreshed."
    )]
    fn run_scheme(&self, Parameters(request): Parameters<RunSchemeRequest>) -> CallToolResult {
        self.execute_saved(request, true)
    }

    #[tool(
        description = "List ppduster block kinds and their versioned input/output contracts. Call this before composing step objects.",
        annotations(read_only_hint = true)
    )]
    fn list_blocks(&self, Parameters(request): Parameters<ListBlocksRequest>) -> CallToolResult {
        let mut definitions = block_definitions();
        if let Some(kind) = request.kind.as_deref() {
            definitions.retain(|definition| definition.kind.id() == kind);
            if definitions.is_empty() {
                return tool_error(format!(
                    "unknown block kind {kind:?}; call list_blocks without filters to see valid kinds"
                ));
            }
        }
        if let Some(category) = request.category.as_deref() {
            definitions.retain(|definition| definition.category.eq_ignore_ascii_case(category));
            if definitions.is_empty() {
                return tool_error(format!(
                    "no blocks found in category {category:?}; call list_blocks without filters to see valid categories"
                ));
            }
        }

        CallToolResult::structured(json!({
            "scheme_format_version": SCHEME_FORMAT_VERSION,
            "output_root": self.output_root(),
            "apply_enabled": self.allow_apply,
            "step_shape": {
                "required_common_fields": ["id", "name", "type"],
                "optional_common_fields": [
                    "auth", "check", "dangerous", "allow_elevation", "when", "require"
                ],
                "action_fields": "Add the fields declared by the selected block's input_schema at the same object level as type."
            },
            "execution_semantics": "Ordered block declarations are compiled into an explicit WorkflowGraph v3. Canvas positions are presentation metadata only; graph edges are the sole execution topology.",
            "safety": "create_scheme only writes files; plan_scheme performs a dry run; run_scheme requires server --allow-apply. Shell and elevation are disabled, and existing project files are never replaced.",
            "example_step": {
                "id": "create-workspace",
                "name": "Create workspace directory",
                "type": "create-directory",
                "path": "$HOME/Developer"
            },
            "blocks": definitions,
        }))
    }

    #[tool(
        description = "Build and validate a ppduster UI scheme without writing a file. Returns normalized project JSON and YAML preview.",
        annotations(read_only_hint = true)
    )]
    fn validate_scheme(
        &self,
        Parameters(request): Parameters<ValidateSchemeRequest>,
    ) -> CallToolResult {
        match self.validate(request.scheme) {
            Ok((project, yaml)) => CallToolResult::structured(json!({
                "valid": true,
                "project": project,
                "yaml": yaml,
            })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Create a validated .ppduster.yaml scheme below the configured output directory. The operation is create-only: parent directories must already exist and existing files are never overwritten."
    )]
    fn create_scheme(
        &self,
        Parameters(request): Parameters<CreateSchemeRequest>,
    ) -> CallToolResult {
        match self.create(request.scheme, request.output_path.as_deref()) {
            Ok(created) => CallToolResult::structured(json!({
                "created": true,
                "path": created.path,
                "relative_path": created.path.strip_prefix(self.output_root()).ok(),
                "project_id": created.project_id,
                "scenario_count": created.scenario_count,
                "step_count": created.step_count,
                "bytes_written": created.bytes_written,
                "warnings": created.warnings,
            })),
            Err(error) => tool_error(error),
        }
    }
}

#[tool_handler(
    name = "ppduster-schemes",
    version = "0.1.0",
    instructions = "Create ppduster Scenario Flow projects. Start with list_blocks, compose ordered block declarations from those contracts, validate with validate_scheme, then persist with create_scheme. The server emits WorkflowGraph v3; canvas positions never define execution topology. Use list_github_repositories and create_github_scheme for the desktop GitHub recipe, then read_scheme, plan_scheme, and explicitly authorized run_scheme to reproduce the saved scenario lifecycle."
)]
impl ServerHandler for PpdusterMcp {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }
}

pub fn build_project(spec: SchemeSpec) -> Result<ScenarioProject, String> {
    let spec_size = serde_json::to_vec(&spec)
        .map_err(|error| format!("scheme cannot be measured: {error}"))?
        .len();
    if spec_size > MAX_SPEC_BYTES {
        return Err(format!(
            "scheme input is {spec_size} bytes; maximum is {MAX_SPEC_BYTES}"
        ));
    }
    if spec.scenarios.is_empty() {
        return Err("scheme requires at least one scenario".into());
    }
    if spec.scenarios.len() > MAX_SCENARIOS {
        return Err(format!(
            "scheme contains {} scenarios; maximum is {MAX_SCENARIOS}",
            spec.scenarios.len()
        ));
    }

    let mut entries = Vec::new();
    let mut canvases = BTreeMap::new();
    let mut scenario_ids = BTreeSet::new();
    let mut total_steps = 0usize;

    for scenario in spec.scenarios {
        if !scenario_ids.insert(scenario.id.clone()) {
            return Err(format!(
                "scheme contains duplicate scenario id {}",
                scenario.id
            ));
        }
        if scenario.group_path.len() > MAX_GROUP_DEPTH {
            return Err(format!(
                "scenario {} group depth {} exceeds maximum {MAX_GROUP_DEPTH}",
                scenario.id,
                scenario.group_path.len()
            ));
        }
        if scenario.steps.len() > MAX_STEPS_PER_SCENARIO {
            return Err(format!(
                "scenario {} contains {} steps; maximum is {MAX_STEPS_PER_SCENARIO}",
                scenario.id,
                scenario.steps.len()
            ));
        }

        let mut steps = Vec::with_capacity(scenario.steps.len());
        for (index, value) in scenario.steps.into_iter().enumerate() {
            let step = serde_json::from_value::<Step>(value).map_err(|error| {
                format!(
                    "scenario {} step {} is not a valid ppduster block: {error}",
                    scenario.id,
                    index + 1
                )
            })?;
            if RESERVED_CANVAS_IDS.contains(&step.id.as_str()) {
                return Err(format!(
                    "scenario {} step {} uses reserved canvas id {:?}",
                    scenario.id,
                    index + 1,
                    step.id
                ));
            }
            if step.name.trim().is_empty() {
                return Err(format!(
                    "scenario {} step {} name must not be empty",
                    scenario.id,
                    index + 1
                ));
            }
            steps.push(step);
        }

        if scenario.workflow_graph.is_some() && !steps.is_empty() {
            return Err(format!(
                "scenario {} must supply either steps or workflow_graph, not both",
                scenario.id
            ));
        }
        let explicit_graph = scenario
            .workflow_graph
            .map(serde_json::from_value::<WorkflowGraph>)
            .transpose()
            .map_err(|e| format!("scenario {} workflow_graph: {e}", scenario.id))?;
        let canvas = linear_canvas(&steps);
        let task_id = scenario.id.clone();
        let task = Task {
            id: scenario.id,
            name: scenario.name,
            description: scenario.description,
            platform: scenario.platform.into(),
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps,
            graph: explicit_graph,
        }
        .into_v3()
        .map_err(|error| format!("scenario {task_id} cannot be compiled to graph v3: {error}"))?;
        task.validate()
            .map_err(|error| format!("scenario {} is invalid: {error}", task.id))?;
        let node_count = task.graph.as_ref().map(count_graph_nodes).unwrap_or(0);
        if node_count > MAX_STEPS_PER_SCENARIO {
            return Err(format!("scenario {} contains {node_count} graph nodes; maximum is {MAX_STEPS_PER_SCENARIO}", task.id));
        }
        total_steps = total_steps.saturating_add(node_count);
        if total_steps > MAX_TOTAL_STEPS {
            return Err(format!(
                "scheme contains more than {MAX_TOTAL_STEPS} total graph nodes"
            ));
        }
        canvases.insert(task.id.clone(), canvas);
        insert_scenario(&mut entries, &scenario.group_path, task)?;
    }

    let project = ScenarioProject {
        id: spec.id,
        name: spec.name,
        description: spec.description,
        entries,
        canvases,
    };
    validate_project(&project)?;
    Ok(project)
}

fn find_task<'a>(entries: &'a [ProjectEntry], id: &str) -> Option<&'a Task> {
    entries.iter().find_map(|entry| match entry {
        ProjectEntry::Scenario { task } if task.id == id => Some(task.as_ref()),
        ProjectEntry::Group { entries, .. } => find_task(entries, id),
        _ => None,
    })
}

pub fn project_yaml(project: &ScenarioProject) -> Result<String, String> {
    serde_yaml::to_string(&ScenarioProjectFile {
        project: project.clone(),
    })
    .map_err(|error| format!("cannot serialize project YAML: {error}"))
}

fn linear_canvas(steps: &[Step]) -> ComposerCanvas {
    let mut positions = BTreeMap::from([("start".to_owned(), CanvasPoint { x: 80.0, y: 250.0 })]);
    for (index, step) in steps.iter().enumerate() {
        positions.insert(
            step.id.clone(),
            CanvasPoint {
                x: 80.0 + 286.0 * (index + 1) as f32,
                y: 250.0,
            },
        );
    }
    ComposerCanvas {
        positions,
        ..ComposerCanvas::default()
    }
}

fn insert_scenario(
    entries: &mut Vec<ProjectEntry>,
    group_path: &[GroupSpec],
    task: Task,
) -> Result<(), String> {
    let Some((group, remaining)) = group_path.split_first() else {
        if entries.iter().any(|entry| entry_id(entry) == task.id) {
            return Err(format!(
                "project entry id {} is already used at this group level",
                task.id
            ));
        }
        entries.push(ProjectEntry::Scenario {
            task: Box::new(task),
        });
        return Ok(());
    };

    if group.id.trim().is_empty() || group.name.trim().is_empty() {
        return Err("project groups require non-empty id and name".into());
    }
    if let Some(index) = entries.iter().position(|entry| entry_id(entry) == group.id) {
        return match &mut entries[index] {
            ProjectEntry::Group { name, entries, .. } if name == &group.name => {
                insert_scenario(entries, remaining, task)
            }
            ProjectEntry::Group { name, .. } => Err(format!(
                "group {} is named both {:?} and {:?} at the same level",
                group.id, name, group.name
            )),
            ProjectEntry::Scenario { .. } => Err(format!(
                "project entry id {} is already used by a scenario at this group level",
                group.id
            )),
        };
    }

    entries.push(ProjectEntry::Group {
        id: group.id.clone(),
        name: group.name.clone(),
        entries: Vec::new(),
    });
    let ProjectEntry::Group { entries, .. } = entries.last_mut().expect("group was just inserted")
    else {
        unreachable!()
    };
    insert_scenario(entries, remaining, task)
}

fn entry_id(entry: &ProjectEntry) -> &str {
    match entry {
        ProjectEntry::Group { id, .. } => id,
        ProjectEntry::Scenario { task } => &task.id,
    }
}

fn count_scenarios(entries: &[ProjectEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match entry {
            ProjectEntry::Group { entries, .. } => count_scenarios(entries),
            ProjectEntry::Scenario { .. } => 1,
        })
        .sum()
}

fn count_steps(entries: &[ProjectEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match entry {
            ProjectEntry::Group { entries, .. } => count_steps(entries),
            ProjectEntry::Scenario { task } => task
                .graph
                .as_ref()
                .map(count_graph_nodes)
                .unwrap_or(task.steps.len()),
        })
        .sum()
}

fn count_graph_nodes(graph: &WorkflowGraph) -> usize {
    graph
        .nodes
        .iter()
        .map(|node| {
            1 + match node {
                GraphNode::ForEach(node) => count_graph_nodes(&node.body),
                GraphNode::If(node) => {
                    count_graph_nodes(&node.then_graph)
                        + node
                            .else_graph
                            .as_deref()
                            .map(count_graph_nodes)
                            .unwrap_or_default()
                }
                GraphNode::Switch(node) => {
                    node.cases
                        .iter()
                        .map(|case| count_graph_nodes(&case.graph))
                        .sum::<usize>()
                        + node
                            .default
                            .as_deref()
                            .map(count_graph_nodes)
                            .unwrap_or_default()
                }
                GraphNode::Action(_) | GraphNode::Join(_) => 0,
            }
        })
        .sum()
}

fn default_output_name(project_id: &str) -> String {
    let mut slug = String::with_capacity(project_id.len());
    let mut previous_dash = false;
    for character in project_id.chars() {
        if character.is_alphanumeric() || matches!(character, '-' | '_') {
            slug.push(character);
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "scheme" } else { slug };
    format!("{slug}.ppduster.yaml")
}

fn validate_output_path(requested: &str) -> Result<PathBuf, String> {
    if requested.trim().is_empty() {
        return Err("output_path must not be empty".into());
    }
    let relative = Path::new(requested);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(
            "output_path must be a clean relative path without '.', '..', or a root".into(),
        );
    }
    let extension = relative
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(extension.as_deref(), Some("yaml" | "yml")) {
        return Err("output_path must end in .yaml or .yml".into());
    }
    Ok(relative.to_path_buf())
}

fn write_new_file(root: &Dir, relative: &Path, bytes: &[u8]) -> Result<Vec<String>, String> {
    let parent_path = relative
        .parent()
        .ok_or_else(|| "scheme output has no parent directory".to_owned())?;
    let parent = if parent_path.as_os_str().is_empty() {
        root.try_clone()
    } else {
        root.open_dir(parent_path)
    }
    .map_err(|error| {
        format!(
            "output parent directory {} must already exist below the configured root: {error}",
            parent_path.display()
        )
    })?;
    let file_name = relative
        .file_name()
        .ok_or_else(|| "output_path must name a file".to_owned())?;

    if parent.try_exists(file_name).map_err(|error| {
        format!(
            "cannot inspect scheme destination {}: {error}",
            relative.display()
        )
    })? {
        return Err(format!(
            "refusing to replace existing scheme {}",
            relative.display()
        ));
    }

    let (temporary_name, mut temporary) = create_temporary_file(&parent)?;
    let persist_result = (|| {
        temporary
            .write_all(bytes)
            .map_err(|error| format!("cannot write scheme data: {error}"))?;
        temporary
            .sync_all()
            .map_err(|error| format!("cannot flush scheme data: {error}"))?;
        match parent.hard_link(&temporary_name, &parent, file_name) {
            Ok(()) => Ok(Vec::new()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
                "refusing to replace existing scheme {}",
                relative.display()
            )),
            Err(hard_link_error) => {
                write_direct_new(&parent, file_name, relative, bytes).map_err(|direct_error| {
                    format!(
                        "cannot create scheme {} (atomic publication failed: {hard_link_error}; secure direct creation failed: {direct_error})",
                        relative.display()
                    )
                })?;
                Ok(vec![
                    "The output filesystem does not support atomic hard-link publication; the scheme was securely created without overwrite using a direct write."
                        .to_owned(),
                ])
            }
        }
    })();
    drop(temporary);
    let cleanup_result = parent.remove_file(&temporary_name);
    let mut warnings = persist_result?;
    if let Err(error) = cleanup_result {
        warnings.push(format!(
            "The scheme was created, but temporary cleanup failed: {error}"
        ));
    }
    if let Err(error) = sync_directory(&parent) {
        warnings.push(format!(
            "The scheme was created, but its directory metadata could not be synchronized: {error}"
        ));
    }
    Ok(warnings)
}

fn write_direct_new(
    parent: &Dir,
    file_name: &std::ffi::OsStr,
    relative: &Path,
    bytes: &[u8],
) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = parent.open_with(file_name, &options).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            format!("refusing to replace existing scheme {}", relative.display())
        } else {
            error.to_string()
        }
    })?;
    file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            format!(
                "cannot write and flush scheme data: {error}; the partially created file may remain and is never removed by pathname"
            )
        })
}

fn create_temporary_file(parent: &Dir) -> Result<(String, cap_std::fs::File), String> {
    for _ in 0..128 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(".ppduster-mcp-{}-{sequence}.tmp", std::process::id());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match parent.open_with(&name, &options) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot create temporary scheme: {error}")),
        }
    }
    Err("cannot allocate a unique temporary scheme name".into())
}

#[cfg(unix)]
fn sync_directory(directory: &Dir) -> std::io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Dir) -> std::io::Result<()> {
    Ok(())
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": message.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn basic_scheme() -> SchemeSpec {
        SchemeSpec {
            id: "developer-workstation".into(),
            name: "Developer workstation".into(),
            description: "A generated project.".into(),
            scenarios: vec![ScenarioSpec {
                id: "prepare-workspace".into(),
                name: "Prepare workspace".into(),
                description: "Create and inspect a development workspace.".into(),
                platform: SchemePlatform::Any,
                workflow_graph: None,
                group_path: vec![GroupSpec {
                    id: "development".into(),
                    name: "Development".into(),
                }],
                steps: vec![
                    json!({
                        "id": "create-workspace",
                        "name": "Create workspace",
                        "type": "create-directory",
                        "path": "$HOME/Developer"
                    }),
                    json!({
                        "id": "inspect-workspace",
                        "name": "Inspect workspace",
                        "type": "inspect-path",
                        "path": "$HOME/Developer"
                    }),
                ],
            }],
        }
    }

    #[test]
    fn builds_nested_project_with_graph_v3_and_deterministic_canvas() {
        let project = build_project(basic_scheme()).unwrap();
        let canvas = &project.canvases["prepare-workspace"];

        assert!(canvas.parents.is_empty());
        assert_eq!(canvas.view, Default::default());
        assert_eq!(canvas.positions["start"].x, 80.0);
        assert_eq!(canvas.positions["inspect-workspace"].x, 652.0);
        let task = project.scenario(&[0, 0]).unwrap();
        assert!(task.steps.is_empty());
        let graph = task.workflow_graph().unwrap();
        assert_eq!(graph.version, ppduster::automation::WORKFLOW_GRAPH_VERSION);
        assert_eq!(graph.entries, ["create-workspace"]);
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from.node, "create-workspace");
        assert_eq!(graph.edges[0].to.node, "inspect-workspace");
        assert!(matches!(
            project.entries.first(),
            Some(ProjectEntry::Group { id, entries, .. })
                if id == "development" && entries.len() == 1
        ));
        let yaml = project_yaml(&project).unwrap();
        assert!(yaml.contains("external-allowed"));
        assert!(yaml.contains("workflow_graph:"));
        assert!(!yaml.contains("\n      steps:"));
        assert!(!yaml.contains("parents:"));
    }

    #[test]
    fn rejects_invalid_blocks_with_scenario_and_index_context() {
        let mut scheme = basic_scheme();
        scheme.scenarios[0].steps[0] = json!({
            "id": "broken",
            "name": "Broken",
            "type": "create-directory"
        });

        let error = build_project(scheme).unwrap_err();
        assert!(error.contains("prepare-workspace step 1"));
        assert!(error.contains("invalid action"));
    }

    #[test]
    fn rejects_reserved_canvas_id_and_blank_step_name() {
        let mut reserved = basic_scheme();
        reserved.scenarios[0].steps[0]["id"] = json!("start");
        let error = build_project(reserved).unwrap_err();
        assert!(error.contains("reserved canvas id \"start\""));

        let mut unnamed = basic_scheme();
        unnamed.scenarios[0].steps[0]["name"] = json!("  ");
        let error = build_project(unnamed).unwrap_err();
        assert!(error.contains("step 1 name must not be empty"));
    }

    #[test]
    fn creates_once_and_refuses_overwrite_or_traversal() {
        let output = TempDir::new().unwrap();
        std::fs::create_dir(output.path().join("nested")).unwrap();
        let server = PpdusterMcp::new(output.path()).unwrap();
        let created = server
            .create(basic_scheme(), Some("nested/generated.ppduster.yaml"))
            .unwrap();

        assert!(created.path.is_file());
        assert_eq!(created.scenario_count, 1);
        assert_eq!(created.step_count, 2);
        assert!(created.warnings.is_empty());
        let saved = std::fs::read_to_string(&created.path).unwrap();
        let reopened = ppduster::automation::load_project_yaml(&saved).unwrap();
        assert_eq!(reopened.id, "developer-workstation");
        assert_eq!(reopened.canvases.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&created.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::read_dir(output.path().join("nested"))
                .unwrap()
                .count(),
            1
        );
        let overwrite_error = server
            .create(basic_scheme(), Some("nested/generated.ppduster.yaml"))
            .unwrap_err();
        assert!(overwrite_error.contains("refusing to replace"));
        assert!(validate_output_path("../escape.yaml").is_err());
        assert!(validate_output_path("/tmp/escape.yaml").is_err());
        assert!(validate_output_path("scheme.json").is_err());
    }

    #[test]
    fn direct_publication_fallback_is_create_only() {
        let output = TempDir::new().unwrap();
        let directory = Dir::open_ambient_dir(output.path(), ambient_authority()).unwrap();
        let relative = Path::new("direct.ppduster.yaml");

        write_direct_new(&directory, relative.as_os_str(), relative, b"project: {}\n").unwrap();
        assert_eq!(
            std::fs::read(output.path().join(relative)).unwrap(),
            b"project: {}\n"
        );
        let error = write_direct_new(&directory, relative.as_os_str(), relative, b"replacement")
            .unwrap_err();
        assert!(error.contains("refusing to replace"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_parent_that_escapes_output_root() {
        use std::os::unix::fs::symlink;

        let output = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        symlink(outside.path(), output.path().join("outside-link")).unwrap();
        let server = PpdusterMcp::new(output.path()).unwrap();

        let error = server
            .create(basic_scheme(), Some("outside-link/scheme.yaml"))
            .unwrap_err();
        assert!(error.contains("below the configured root"));
        assert!(!outside.path().join("scheme.yaml").exists());
    }
}

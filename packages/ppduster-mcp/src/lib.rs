#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use ppduster::automation::{
    block_definitions, validate_project, CanvasPoint, ComposerCanvas, ProjectEntry,
    ScenarioProject, ScenarioProjectFile, Step, Task, TrustRequirement,
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
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
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
        description = "Ordered ppduster Step objects. Each object needs id, name, type, and the inputs declared by list_blocks. Array order is execution order."
    )]
    #[schemars(schema_with = "steps_input_schema")]
    pub steps: Vec<Value>,
}

fn steps_input_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let action_kinds = block_definitions()
        .into_iter()
        .map(|definition| definition.kind.id())
        .collect::<Vec<_>>();
    schemars::json_schema!({
        "type": "array",
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
    #[schemars(description = "One or more linear scenarios to place in the project tree")]
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

#[derive(Debug, Clone)]
pub struct PpdusterMcp {
    output_root: Arc<PathBuf>,
    output_dir: Arc<Dir>,
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
        })
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
        description = "List ppduster block kinds and their versioned input/output contracts. Call this before composing step objects."
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
            "step_shape": {
                "required_common_fields": ["id", "name", "type"],
                "optional_common_fields": [
                    "auth", "check", "dangerous", "allow_elevation", "when", "require"
                ],
                "action_fields": "Add the fields declared by the selected block's input_schema at the same object level as type."
            },
            "execution_semantics": "Scenario steps execute in array order. Generated canvas parents mirror that order and are presentation metadata only.",
            "safety": "This server validates and writes project files only. It never plans or executes a scenario, never elevates privileges, and never replaces an existing file.",
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
        description = "Build and validate a ppduster UI scheme without writing a file. Returns normalized project JSON and YAML preview."
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
    instructions = "Create ppduster Scenario Flow projects. Start with list_blocks, compose ordered steps from those contracts, validate with validate_scheme, then persist with create_scheme. Canvas links are visual metadata; step array order is runtime order."
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
        total_steps = total_steps.saturating_add(scenario.steps.len());
        if total_steps > MAX_TOTAL_STEPS {
            return Err(format!(
                "scheme contains more than {MAX_TOTAL_STEPS} total steps"
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

        let canvas = linear_canvas(&steps);
        let task = Task {
            id: scenario.id,
            name: scenario.name,
            description: scenario.description,
            platform: scenario.platform.into(),
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps,
            graph: None,
        };
        task.validate()
            .map_err(|error| format!("scenario {} is invalid: {error}", task.id))?;
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

pub fn project_yaml(project: &ScenarioProject) -> Result<String, String> {
    serde_yaml::to_string(&ScenarioProjectFile {
        project: project.clone(),
    })
    .map_err(|error| format!("cannot serialize project YAML: {error}"))
}

fn linear_canvas(steps: &[Step]) -> ComposerCanvas {
    let mut positions = BTreeMap::from([("start".to_owned(), CanvasPoint { x: 80.0, y: 250.0 })]);
    let mut parents = BTreeMap::new();
    let mut previous = "start".to_owned();
    for (index, step) in steps.iter().enumerate() {
        positions.insert(
            step.id.clone(),
            CanvasPoint {
                x: 80.0 + 286.0 * (index + 1) as f32,
                y: 250.0,
            },
        );
        parents.insert(step.id.clone(), previous);
        previous = step.id.clone();
    }
    ComposerCanvas { positions, parents }
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
            ProjectEntry::Scenario { task } => task.steps.len(),
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
    fn builds_nested_project_with_deterministic_linear_canvas() {
        let project = build_project(basic_scheme()).unwrap();
        let canvas = &project.canvases["prepare-workspace"];

        assert_eq!(canvas.parents["create-workspace"], "start");
        assert_eq!(canvas.parents["inspect-workspace"], "create-workspace");
        assert_eq!(canvas.positions["start"].x, 80.0);
        assert_eq!(canvas.positions["inspect-workspace"].x, 652.0);
        assert!(matches!(
            project.entries.first(),
            Some(ProjectEntry::Group { id, entries, .. })
                if id == "development" && entries.len() == 1
        ));
        assert!(project_yaml(&project).unwrap().contains("external-allowed"));
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

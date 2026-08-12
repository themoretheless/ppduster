use crate::automation::context::{ContextType, FieldSchema, ObjectSchema, SemanticFormat};
use crate::automation::task::{
    Action, ActivateLicenseAction, AppStoreInstallAction, AppStoreOperation, ArchiveFormat,
    AuthPolicy, BambuStudioReleaseAction, Checksum, CopyPathAction, CreateDirectoryAction,
    ElevationPolicy, EncryptedSecretsSpec, InspectPathAction, LicenseMethod, LicenseProvider,
    NpmRegistryFileSpec, NugetRegistryFileSpec, ReleaseChannel, RemovePathAction,
    ScriptInterpreter, ShellMode, Step, WriteConflictPolicy, WriteFileAction,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stable identity for every executable block. Display names and canvas
/// positions may change without invalidating bindings or stored schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    GithubListRepositories,
    ForEach,
    ForEachGitCloneIfMissing,
    CreateDirectory,
    InspectPath,
    CopyPath,
    WriteFile,
    RemovePath,
    GitClone,
    GitInspect,
    GitCloneIfMissing,
    GitFetch,
    GitFastForward,
    BrewInstall,
    RunCommand,
    RunScript,
    ConfigurePackageRegistryFiles,
    DownloadFile,
    ExtractArchive,
    InstallDmg,
    InstallPkg,
    MacosRequirements,
    AppStoreInstall,
    BambuStudioRelease,
    ActivateLicense,
}

impl ActionKind {
    pub const ALL: [Self; 25] = [
        Self::GithubListRepositories,
        Self::ForEach,
        Self::ForEachGitCloneIfMissing,
        Self::CreateDirectory,
        Self::InspectPath,
        Self::CopyPath,
        Self::WriteFile,
        Self::RemovePath,
        Self::GitClone,
        Self::GitInspect,
        Self::GitCloneIfMissing,
        Self::GitFetch,
        Self::GitFastForward,
        Self::BrewInstall,
        Self::RunCommand,
        Self::RunScript,
        Self::ConfigurePackageRegistryFiles,
        Self::DownloadFile,
        Self::ExtractArchive,
        Self::InstallDmg,
        Self::InstallPkg,
        Self::MacosRequirements,
        Self::AppStoreInstall,
        Self::BambuStudioRelease,
        Self::ActivateLicense,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::GithubListRepositories => "github-list-repositories",
            Self::ForEach => "for-each",
            Self::ForEachGitCloneIfMissing => "for-each-git-clone-if-missing",
            Self::CreateDirectory => "create-directory",
            Self::InspectPath => "inspect-path",
            Self::CopyPath => "copy-path",
            Self::WriteFile => "write-file",
            Self::RemovePath => "remove-path",
            Self::GitClone => "git-clone",
            Self::GitInspect => "git-inspect",
            Self::GitCloneIfMissing => "git-clone-if-missing",
            Self::GitFetch => "git-fetch",
            Self::GitFastForward => "git-fast-forward",
            Self::BrewInstall => "brew-install",
            Self::RunCommand => "run-command",
            Self::RunScript => "run-script",
            Self::ConfigurePackageRegistryFiles => "configure-package-registry-files",
            Self::DownloadFile => "download-file",
            Self::ExtractArchive => "extract-archive",
            Self::InstallDmg => "install-dmg",
            Self::InstallPkg => "install-pkg",
            Self::MacosRequirements => "macos-requirements",
            Self::AppStoreInstall => "app-store-install",
            Self::BambuStudioRelease => "bambu-studio-release",
            Self::ActivateLicense => "activate-license",
        }
    }

    /// Whether this registry entry is represented by an executable graph
    /// action node. Legacy foreach entries remain readable for imports, but
    /// graph v3 represents loops structurally with `GraphNode::ForEach`.
    pub const fn is_graph_action(self) -> bool {
        !matches!(self, Self::ForEach | Self::ForEachGitCloneIfMissing)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockDefinition {
    pub kind: ActionKind,
    pub schema_version: u32,
    pub title: String,
    pub category: String,
    pub input_schema: ObjectSchema,
    pub output_schema: ObjectSchema,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub may_use_secrets: bool,
    /// Policies that the visual editor may attach to this block.
    ///
    /// Action validation remains the final authority, but publishing the
    /// capability contract beside the input/output schemas prevents a
    /// schema-driven editor from offering combinations that are known to be
    /// invalid (for example GitHub repository discovery with sudo).
    #[serde(default)]
    pub policy: BlockPolicyCapabilities,
}

impl BlockDefinition {
    pub fn output_schema_id(&self) -> Option<&str> {
        self.output_schema.id.as_deref()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyRequirement {
    #[default]
    Forbidden,
    Optional,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockPolicyCapabilities {
    #[serde(default)]
    pub allow_git_credentials: bool,
    #[serde(default)]
    pub allow_sudo: bool,
    #[serde(default)]
    pub allow_elevation: bool,
    #[serde(default)]
    pub dangerous: PolicyRequirement,
}

impl Default for BlockPolicyCapabilities {
    fn default() -> Self {
        Self {
            allow_git_credentials: false,
            allow_sudo: false,
            allow_elevation: false,
            dangerous: PolicyRequirement::Forbidden,
        }
    }
}

impl BlockPolicyCapabilities {
    pub const fn allows_auth(&self, auth: AuthPolicy) -> bool {
        match auth {
            AuthPolicy::None => true,
            AuthPolicy::GitCredential => self.allow_git_credentials,
            AuthPolicy::Sudo => self.allow_sudo,
        }
    }

    pub const fn allows_dangerous(&self, dangerous: bool) -> bool {
        match self.dangerous {
            PolicyRequirement::Forbidden => !dangerous,
            PolicyRequirement::Optional => true,
            PolicyRequirement::Required => dangerous,
        }
    }

    pub const fn accepts(&self, step: &Step) -> bool {
        self.allows_auth(step.auth)
            && (self.allow_elevation || matches!(step.allow_elevation, ElevationPolicy::Forbidden))
            && self.allows_dangerous(step.dangerous)
    }
}

impl Action {
    pub const fn kind(&self) -> ActionKind {
        match self {
            Self::GithubListRepositories => ActionKind::GithubListRepositories,
            Self::ForEach { .. } => ActionKind::ForEach,
            Self::ForEachGitCloneIfMissing { .. } => ActionKind::ForEachGitCloneIfMissing,
            Self::CreateDirectory(_) => ActionKind::CreateDirectory,
            Self::InspectPath(_) => ActionKind::InspectPath,
            Self::CopyPath(_) => ActionKind::CopyPath,
            Self::WriteFile(_) => ActionKind::WriteFile,
            Self::RemovePath(_) => ActionKind::RemovePath,
            Self::GitClone { .. } => ActionKind::GitClone,
            Self::GitInspect { .. } => ActionKind::GitInspect,
            Self::GitCloneIfMissing { .. } => ActionKind::GitCloneIfMissing,
            Self::GitFetch { .. } => ActionKind::GitFetch,
            Self::GitFastForward { .. } => ActionKind::GitFastForward,
            Self::BrewInstall { .. } => ActionKind::BrewInstall,
            Self::RunCommand { .. } => ActionKind::RunCommand,
            Self::RunScript { .. } => ActionKind::RunScript,
            Self::ConfigurePackageRegistryFiles { .. } => ActionKind::ConfigurePackageRegistryFiles,
            Self::DownloadFile { .. } => ActionKind::DownloadFile,
            Self::ExtractArchive { .. } => ActionKind::ExtractArchive,
            Self::InstallDmg { .. } => ActionKind::InstallDmg,
            Self::InstallPkg { .. } => ActionKind::InstallPkg,
            Self::MacosRequirements { .. } => ActionKind::MacosRequirements,
            Self::AppStoreInstall(_) => ActionKind::AppStoreInstall,
            Self::BambuStudioRelease(_) => ActionKind::BambuStudioRelease,
            Self::ActivateLicense(_) => ActionKind::ActivateLicense,
        }
    }
}

pub fn block_definitions() -> Vec<BlockDefinition> {
    ActionKind::ALL.into_iter().map(block_definition).collect()
}

pub fn definition_for_action(action: &Action) -> BlockDefinition {
    block_definition(action.kind())
}

/// Construct a schema-valid editable prototype for an executable block.
///
/// The visual graph editor uses this registry entry when a block is added and
/// then edits its inputs through [`BlockDefinition::input_schema`]. Keeping
/// prototypes beside the schema registry prevents UI code from growing a
/// second action-specific constructor table. Legacy linear control actions are
/// deliberately excluded: graph v3 represents control flow with `GraphNode`
/// variants instead of executable `Action` values.
pub fn default_action(kind: ActionKind) -> Result<Action, &'static str> {
    let repository = "https://github.com/owner/repository.git".to_owned();
    let destination = "$HOME/Developer/owner/repository".to_owned();
    Ok(match kind {
        ActionKind::GithubListRepositories => Action::GithubListRepositories,
        ActionKind::ForEach | ActionKind::ForEachGitCloneIfMissing => {
            return Err("legacy foreach actions are not graph-v3 action blocks")
        }
        ActionKind::CreateDirectory => Action::CreateDirectory(CreateDirectoryAction {
            path: "$HOME/Developer/project".into(),
        }),
        ActionKind::InspectPath => Action::InspectPath(InspectPathAction {
            path: "$HOME/Developer/project".into(),
            recursive_size: false,
            sha256: false,
            expect: None,
        }),
        ActionKind::CopyPath => Action::CopyPath(CopyPathAction {
            src: "$HOME/Developer/source".into(),
            dest: "$HOME/Developer/destination".into(),
        }),
        ActionKind::WriteFile => Action::WriteFile(WriteFileAction {
            path: "$HOME/Developer/project/example.txt".into(),
            content: String::new(),
            on_conflict: WriteConflictPolicy::Fail,
        }),
        ActionKind::RemovePath => Action::RemovePath(RemovePathAction {
            path: "$HOME/Library/Caches/example".into(),
        }),
        ActionKind::GitClone => Action::GitClone {
            repo: repository,
            dest: destination,
            branch: Some("main".into()),
        },
        ActionKind::GitInspect => Action::GitInspect {
            repo: repository,
            dest: destination,
        },
        ActionKind::GitCloneIfMissing => Action::GitCloneIfMissing {
            repo: repository,
            dest: destination,
            branch: Some("main".into()),
        },
        ActionKind::GitFetch => Action::GitFetch {
            repo: repository,
            dest: destination,
            branch: "main".into(),
        },
        ActionKind::GitFastForward => Action::GitFastForward {
            repo: repository,
            dest: destination,
            branch: "main".into(),
        },
        ActionKind::BrewInstall => Action::BrewInstall {
            package: "ripgrep".into(),
            cask: false,
        },
        ActionKind::RunCommand => Action::RunCommand {
            program: "true".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            shell: ShellMode::Forbidden,
        },
        ActionKind::RunScript => Action::RunScript {
            interpreter: ScriptInterpreter::Sh,
            script: "$HOME/Developer/script.sh".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            success_exit_codes: vec![0],
        },
        ActionKind::ConfigurePackageRegistryFiles => Action::ConfigurePackageRegistryFiles {
            secrets: EncryptedSecretsSpec {
                profile: "company".into(),
                username_env: "PPDUSTER_REGISTRY_USERNAME".into(),
                token_env: "PPDUSTER_REGISTRY_TOKEN".into(),
            },
            npm: NpmRegistryFileSpec {
                scope: "@company".into(),
                registry: "https://registry.example.com/npm/".into(),
            },
            nuget: NugetRegistryFileSpec {
                public_source_name: "nuget.org".into(),
                public_source: "https://api.nuget.org/v3/index.json".into(),
                source_name: "company".into(),
                source: "https://registry.example.com/nuget/v3/index.json".into(),
                package_patterns: vec!["Company.*".into()],
            },
        },
        ActionKind::DownloadFile => Action::DownloadFile {
            url: "https://example.com/archive.zip".into(),
            dest: "$HOME/Downloads/archive.zip".into(),
            checksum: Checksum {
                sha256: "0".repeat(64),
            },
        },
        ActionKind::ExtractArchive => Action::ExtractArchive {
            src: "$HOME/Downloads/archive.zip".into(),
            dest: "$HOME/Developer/archive".into(),
            format: ArchiveFormat::Auto,
            max_unpacked_bytes: 10 * 1024 * 1024 * 1024,
        },
        ActionKind::InstallDmg => Action::InstallDmg {
            dmg: "$HOME/Downloads/Application.dmg".into(),
            // Keep the editable prototype internally consistent when the
            // optional identity object is enabled as one atomic input group.
            app_name: Some("Application.app".into()),
            target: None,
            identity: None,
        },
        ActionKind::InstallPkg => Action::InstallPkg {
            pkg: "$HOME/Downloads/Installer.pkg".into(),
            target: None,
        },
        ActionKind::MacosRequirements => Action::MacosRequirements {
            minimum_version: "13.0".into(),
            require_rosetta_on_apple_silicon: false,
        },
        ActionKind::AppStoreInstall => Action::AppStoreInstall(AppStoreInstallAction {
            app_id: 1,
            operation: AppStoreOperation::Install,
        }),
        ActionKind::BambuStudioRelease => Action::BambuStudioRelease(BambuStudioReleaseAction {
            channel: ReleaseChannel::Release,
        }),
        ActionKind::ActivateLicense => Action::ActivateLicense(ActivateLicenseAction {
            provider: LicenseProvider::LightBurn,
            method: LicenseMethod::VendorUi,
        }),
    })
}

/// Construct a complete action step for a newly-created graph node.
///
/// Common policy defaults live here as part of the block registry contract,
/// rather than being duplicated by every editor or importer.
pub fn default_step(kind: ActionKind, id: impl Into<String>) -> Result<Step, &'static str> {
    let definition = block_definition(kind);
    Ok(Step {
        id: id.into(),
        name: definition.title,
        bindings: BTreeMap::new(),
        auth: AuthPolicy::None,
        check: None,
        dangerous: matches!(kind, ActionKind::RunScript),
        allow_elevation: ElevationPolicy::Forbidden,
        when: None,
        require: None,
        action: default_action(kind)?,
    })
}

pub const fn block_policy_capabilities(kind: ActionKind) -> BlockPolicyCapabilities {
    match kind {
        ActionKind::GithubListRepositories
        | ActionKind::ForEach
        | ActionKind::ForEachGitCloneIfMissing
        | ActionKind::CreateDirectory
        | ActionKind::InspectPath
        | ActionKind::CopyPath
        | ActionKind::WriteFile
        | ActionKind::RemovePath => BlockPolicyCapabilities {
            allow_git_credentials: false,
            allow_sudo: false,
            allow_elevation: false,
            dangerous: PolicyRequirement::Forbidden,
        },
        ActionKind::AppStoreInstall => BlockPolicyCapabilities {
            allow_git_credentials: false,
            allow_sudo: false,
            allow_elevation: false,
            dangerous: PolicyRequirement::Forbidden,
        },
        ActionKind::RunScript => BlockPolicyCapabilities {
            allow_git_credentials: true,
            allow_sudo: true,
            allow_elevation: true,
            dangerous: PolicyRequirement::Required,
        },
        _ => BlockPolicyCapabilities {
            allow_git_credentials: true,
            allow_sudo: true,
            allow_elevation: true,
            dangerous: PolicyRequirement::Optional,
        },
    }
}

pub fn block_definition(kind: ActionKind) -> BlockDefinition {
    let (title, category, inputs, outputs, read_only, may_use_secrets) = match kind {
        ActionKind::GithubListRepositories => (
            "Получить репозитории аккаунта",
            "GitHub",
            schema("ppduster.github.repositories.inputs@1", []),
            github_repositories_schema(),
            true,
            false,
        ),
        ActionKind::ForEach => (
            "Для каждого элемента",
            "Логика",
            schema(
                "ppduster.control.for-each.inputs@1",
                [
                    ("source_step", req(identifier())),
                    ("array_path", req(ContextType::STRING)),
                    ("item", req(identifier())),
                    ("fields", opt(ContextType::array(ContextType::STRING))),
                ],
            ),
            for_each_schema("ppduster.control.for-each@1"),
            true,
            false,
        ),
        ActionKind::ForEachGitCloneIfMissing => (
            "Клонировать каждый репозиторий",
            "Логика",
            git_inputs(true).with_field("loop_step", req(identifier())),
            for_each_results_schema(),
            false,
            false,
        ),
        ActionKind::CreateDirectory => (
            "Создать папку",
            "Файлы",
            one_path_input("ppduster.filesystem.create-directory.inputs@1", "path"),
            create_directory_schema(),
            false,
            false,
        ),
        ActionKind::InspectPath => (
            "Проверить путь",
            "Файлы",
            schema(
                "ppduster.path.metadata.inputs@1",
                [
                    ("path", req(path())),
                    ("recursive_size", opt(ContextType::Boolean)),
                    ("sha256", opt(ContextType::Boolean)),
                    ("expect", nullable(path_expectation_type())),
                ],
            ),
            path_metadata_schema(),
            true,
            false,
        ),
        ActionKind::CopyPath => (
            "Копировать путь",
            "Файлы",
            schema(
                "ppduster.filesystem.copy-path.inputs@1",
                [("src", req(path())), ("dest", req(path()))],
            ),
            copy_path_schema(),
            false,
            false,
        ),
        ActionKind::WriteFile => (
            "Записать файл",
            "Файлы",
            schema(
                "ppduster.filesystem.write-file.inputs@1",
                [
                    ("path", req(file_path())),
                    ("content", req(ContextType::STRING)),
                    (
                        "on_conflict",
                        opt(ContextType::STRING).with_allowed_values(["fail", "replace"]),
                    ),
                ],
            ),
            write_file_schema(),
            false,
            false,
        ),
        ActionKind::RemovePath => (
            "Переместить в корзину",
            "Файлы",
            one_path_input("ppduster.filesystem.remove-path.inputs@1", "path"),
            remove_path_schema(),
            false,
            false,
        ),
        ActionKind::GitClone
        | ActionKind::GitInspect
        | ActionKind::GitCloneIfMissing
        | ActionKind::GitFetch
        | ActionKind::GitFastForward => {
            let output_id = match kind {
                ActionKind::GitClone => "ppduster.git.clone@1",
                ActionKind::GitInspect => "ppduster.git.inspect@1",
                ActionKind::GitCloneIfMissing => "ppduster.git.clone-if-missing@1",
                ActionKind::GitFetch => "ppduster.git.fetch@1",
                ActionKind::GitFastForward => "ppduster.git.fast-forward@1",
                _ => unreachable!(),
            };
            let title = match kind {
                ActionKind::GitClone => "Клонировать или синхронизировать",
                ActionKind::GitInspect => "Проверить Git-репозиторий",
                ActionKind::GitCloneIfMissing => "Клонировать, если отсутствует",
                ActionKind::GitFetch => "Получить remote-ветку",
                ActionKind::GitFastForward => "Актуализировать ветку",
                _ => unreachable!(),
            };
            let inputs = match kind {
                ActionKind::GitInspect => git_inputs(false),
                ActionKind::GitFetch | ActionKind::GitFastForward => {
                    git_inputs(false).with_field("branch", req(git_ref()))
                }
                ActionKind::GitClone | ActionKind::GitCloneIfMissing => git_inputs(true),
                _ => unreachable!(),
            };
            (
                title,
                "Git",
                inputs,
                repository_schema(output_id),
                matches!(kind, ActionKind::GitInspect),
                false,
            )
        }
        ActionKind::BrewInstall => (
            "Установить Homebrew-пакет",
            "Пакеты",
            schema(
                "ppduster.package.brew-install.inputs@1",
                [
                    ("package", req(identifier())),
                    ("cask", opt(ContextType::Boolean)),
                ],
            ),
            package_schema(),
            false,
            false,
        ),
        ActionKind::RunCommand | ActionKind::RunScript => (
            if matches!(kind, ActionKind::RunCommand) {
                "Выполнить команду"
            } else {
                "Выполнить скрипт"
            },
            "Процессы",
            process_inputs(kind),
            process_exit_schema(),
            false,
            true,
        ),
        ActionKind::ConfigurePackageRegistryFiles => (
            "Настроить package registries",
            "Конфигурация",
            package_registry_inputs(),
            configuration_schema(),
            false,
            true,
        ),
        ActionKind::DownloadFile => (
            "Скачать файл",
            "Артефакты",
            schema(
                "ppduster.artifact.download.inputs@1",
                [
                    ("url", req(url())),
                    ("dest", req(file_path())),
                    (
                        "checksum",
                        req(ContextType::object(schema(
                            "ppduster.artifact.checksum@1",
                            [("sha256", req(sha256()))],
                        ))),
                    ),
                ],
            ),
            download_schema(),
            false,
            false,
        ),
        ActionKind::ExtractArchive => (
            "Распаковать архив",
            "Артефакты",
            schema(
                "ppduster.artifact.extract.inputs@1",
                [
                    ("src", req(file_path())),
                    ("dest", req(directory_path())),
                    (
                        "format",
                        opt(ContextType::STRING).with_allowed_values([
                            "auto", "zip", "tar", "tar-gz", "tar-bz2", "tar-xz",
                        ]),
                    ),
                    ("max_unpacked_bytes", opt(ContextType::Integer)),
                ],
            ),
            extract_schema(),
            false,
            false,
        ),
        ActionKind::InstallDmg
        | ActionKind::InstallPkg
        | ActionKind::AppStoreInstall
        | ActionKind::BambuStudioRelease => (
            match kind {
                ActionKind::InstallDmg => "Установить DMG",
                ActionKind::InstallPkg => "Установить PKG",
                ActionKind::AppStoreInstall => "Установить из App Store",
                ActionKind::BambuStudioRelease => "Установить Bambu Studio",
                _ => unreachable!(),
            },
            "Установка",
            installation_inputs(kind),
            installation_schema(kind),
            false,
            false,
        ),
        ActionKind::MacosRequirements => (
            "Проверить требования macOS",
            "Система",
            schema(
                "ppduster.system.macos-requirements.inputs@1",
                [
                    ("minimum_version", req(ContextType::STRING)),
                    (
                        "require_rosetta_on_apple_silicon",
                        opt(ContextType::Boolean),
                    ),
                ],
            ),
            system_schema(),
            true,
            false,
        ),
        ActionKind::ActivateLicense => (
            "Активировать лицензию",
            "Лицензия",
            schema(
                "ppduster.license.activation.inputs@1",
                [
                    (
                        "provider",
                        req(ContextType::STRING).with_allowed_values(["light-burn"]),
                    ),
                    (
                        "method",
                        req(ContextType::STRING).with_allowed_values(["vendor-ui"]),
                    ),
                ],
            ),
            license_schema(),
            false,
            true,
        ),
    };
    BlockDefinition {
        kind,
        schema_version: 1,
        title: title.into(),
        category: category.into(),
        input_schema: inputs,
        output_schema: outputs,
        read_only,
        may_use_secrets,
        policy: block_policy_capabilities(kind),
    }
}

fn schema<const N: usize>(id: impl Into<String>, fields: [(&str, FieldSchema); N]) -> ObjectSchema {
    fields
        .into_iter()
        .fold(ObjectSchema::new(id), |schema, (name, field)| {
            schema.with_field(name, field)
        })
}

fn req(value_type: ContextType) -> FieldSchema {
    FieldSchema::required(value_type)
}

fn opt(value_type: ContextType) -> FieldSchema {
    FieldSchema::optional(value_type)
}

fn nullable(value_type: ContextType) -> FieldSchema {
    FieldSchema::optional(value_type).nullable()
}

fn string(format: SemanticFormat) -> ContextType {
    ContextType::string(format)
}

fn path() -> ContextType {
    string(SemanticFormat::Path)
}

fn file_path() -> ContextType {
    string(SemanticFormat::FilePath)
}

fn directory_path() -> ContextType {
    string(SemanticFormat::DirectoryPath)
}

fn url() -> ContextType {
    string(SemanticFormat::Url)
}

fn git_url() -> ContextType {
    string(SemanticFormat::GitUrl)
}

fn git_ref() -> ContextType {
    string(SemanticFormat::GitRef)
}

fn sha256() -> ContextType {
    string(SemanticFormat::Sha256)
}

fn identifier() -> ContextType {
    string(SemanticFormat::Identifier)
}

fn secret_ref() -> ContextType {
    string(SemanticFormat::SecretRef)
}

fn path_expectation_type() -> ContextType {
    ContextType::object(schema(
        "ppduster.path.expectation@1",
        [
            ("exists", nullable(ContextType::Boolean)),
            (
                "kind",
                nullable(ContextType::STRING).with_allowed_values([
                    "file",
                    "directory",
                    "symlink",
                    "other",
                ]),
            ),
            ("empty", nullable(ContextType::Boolean)),
            ("min_size_bytes", nullable(ContextType::Integer)),
            ("max_size_bytes", nullable(ContextType::Integer)),
            (
                "modified_at_or_after",
                nullable(string(SemanticFormat::DateTime)),
            ),
            (
                "modified_at_or_before",
                nullable(string(SemanticFormat::DateTime)),
            ),
            ("sha256", nullable(sha256())),
        ],
    ))
}

fn string_map_type(id: &str) -> ContextType {
    ContextType::object(ObjectSchema::new(id).with_additional_fields(ContextType::STRING))
}

fn process_inputs(kind: ActionKind) -> ObjectSchema {
    match kind {
        ActionKind::RunCommand => schema(
            "ppduster.run-command.inputs@1",
            [
                ("program", req(ContextType::STRING)),
                ("args", opt(ContextType::array(ContextType::STRING))),
                ("cwd", nullable(directory_path())),
                (
                    "env",
                    opt(string_map_type("ppduster.process.environment@1")),
                ),
                (
                    "shell",
                    opt(ContextType::STRING).with_allowed_values(["forbidden", "allow"]),
                ),
            ],
        ),
        ActionKind::RunScript => schema(
            "ppduster.run-script.inputs@1",
            [
                (
                    "interpreter",
                    req(ContextType::STRING).with_allowed_values(["sh", "bash", "powershell"]),
                ),
                ("script", req(file_path())),
                ("args", opt(ContextType::array(ContextType::STRING))),
                ("cwd", nullable(directory_path())),
                (
                    "env",
                    opt(string_map_type("ppduster.process.environment@1")),
                ),
                (
                    "success_exit_codes",
                    opt(ContextType::array(ContextType::Integer)),
                ),
            ],
        ),
        _ => unreachable!(),
    }
}

fn package_registry_inputs() -> ObjectSchema {
    let secrets = ContextType::object(schema(
        "ppduster.configuration.secret-profile@1",
        [
            ("profile", req(secret_ref())),
            ("username_env", req(identifier())),
            ("token_env", req(identifier())),
        ],
    ));
    let npm = ContextType::object(schema(
        "ppduster.configuration.npm@1",
        [
            ("scope", req(ContextType::STRING)),
            ("registry", req(url())),
        ],
    ));
    let nuget = ContextType::object(schema(
        "ppduster.configuration.nuget@1",
        [
            ("public_source_name", req(identifier())),
            ("public_source", req(url())),
            ("source_name", req(identifier())),
            ("source", req(url())),
            (
                "package_patterns",
                req(ContextType::array(ContextType::STRING)),
            ),
        ],
    ));
    schema(
        "ppduster.configuration.package-registries.inputs@1",
        [
            ("secrets", req(secrets)),
            ("npm", req(npm)),
            ("nuget", req(nuget)),
        ],
    )
}

fn installation_inputs(kind: ActionKind) -> ObjectSchema {
    match kind {
        ActionKind::InstallDmg => {
            let identity = ContextType::object(schema(
                "ppduster.installation.app-identity@1",
                [
                    ("bundle_identifier", req(identifier())),
                    ("team_identifier", req(identifier())),
                    ("version", req(ContextType::STRING)),
                ],
            ));
            schema(
                "ppduster.install-dmg.inputs@1",
                [
                    ("dmg", req(file_path())),
                    ("app_name", nullable(ContextType::STRING)),
                    ("target", nullable(directory_path())),
                    ("identity", nullable(identity)),
                ],
            )
        }
        ActionKind::InstallPkg => schema(
            "ppduster.install-pkg.inputs@1",
            [
                ("pkg", req(file_path())),
                ("target", nullable(directory_path())),
            ],
        ),
        ActionKind::AppStoreInstall => schema(
            "ppduster.app-store-install.inputs@1",
            [
                ("app_id", req(ContextType::Integer)),
                (
                    "operation",
                    opt(ContextType::STRING).with_allowed_values(["install", "get"]),
                ),
            ],
        ),
        ActionKind::BambuStudioRelease => schema(
            "ppduster.bambu-studio-release.inputs@1",
            [(
                "channel",
                opt(ContextType::STRING).with_allowed_values(["release", "beta"]),
            )],
        ),
        _ => unreachable!(),
    }
}

fn one_path_input(id: &str, name: &str) -> ObjectSchema {
    schema(id, [(name, req(path()))])
}

fn git_inputs(branch: bool) -> ObjectSchema {
    let mut result = schema(
        "ppduster.git.inputs@1",
        [("repo", req(git_url())), ("dest", req(directory_path()))],
    );
    if branch {
        result = result.with_field("branch", nullable(git_ref()));
    }
    result
}

fn github_repository_type() -> ContextType {
    ContextType::object(schema(
        "ppduster.github.repository@1",
        [
            // GitHub node IDs are opaque values. Older repositories can still
            // expose the legacy Base64 form (for example, `MDEwOl...=`), which
            // is intentionally broader than ppduster's local Identifier
            // contract for step IDs and aliases.
            ("id", req(string(SemanticFormat::OpaqueIdentifier))),
            ("owner", req(string(SemanticFormat::RepositoryName))),
            ("name", req(string(SemanticFormat::RepositoryName))),
            ("full_name", req(string(SemanticFormat::RepositoryName))),
            ("https_url", req(git_url())),
            ("ssh_url", req(git_url())),
            ("default_branch", nullable(git_ref())),
            ("private", req(ContextType::Boolean)),
            ("archived", req(ContextType::Boolean)),
        ],
    ))
}

fn github_repositories_schema() -> ObjectSchema {
    let account = ContextType::object(schema(
        "ppduster.github.account@1",
        [("login", req(identifier()))],
    ));
    let github = ContextType::object(schema(
        "ppduster.github.context@1",
        [
            ("account", req(account)),
            (
                "repositories",
                req(ContextType::array(github_repository_type())),
            ),
        ],
    ));
    schema("ppduster.github.repositories@1", [("github", req(github))])
}

fn for_each_schema(id: &str) -> ObjectSchema {
    let loop_value = ContextType::object(schema(
        "ppduster.control.loop@1",
        [
            ("source_step", req(identifier())),
            ("array_path", req(ContextType::STRING)),
            ("item_alias", req(identifier())),
            ("count", req(ContextType::Integer)),
            ("items", req(ContextType::array(ContextType::Any))),
        ],
    ));
    schema(id, [("loop", req(loop_value))])
}

fn for_each_results_schema() -> ObjectSchema {
    let loop_value = ContextType::object(schema(
        "ppduster.control.loop-results@1",
        [
            ("source_step", req(identifier())),
            ("count", req(ContextType::Integer)),
            ("applied", req(ContextType::Integer)),
            ("satisfied", req(ContextType::Integer)),
            ("failed", req(ContextType::Boolean)),
            ("error", nullable(ContextType::STRING)),
        ],
    ));
    schema(
        "ppduster.control.for-each-results@1",
        [("loop", req(loop_value))],
    )
}

fn create_directory_schema() -> ObjectSchema {
    let path_value = ContextType::object(schema(
        "ppduster.filesystem.created-path@1",
        [
            ("value", req(directory_path())),
            ("exists", req(ContextType::Boolean)),
            ("kind", req(ContextType::STRING)),
            ("created", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    schema(
        "ppduster.filesystem.create-directory@1",
        [("path", req(path_value))],
    )
}

fn path_metadata_schema() -> ObjectSchema {
    schema(
        "ppduster.path.metadata@1",
        [
            ("path", req(path())),
            ("exists", req(ContextType::Boolean)),
            ("kind", nullable(ContextType::STRING)),
            ("size_bytes", nullable(ContextType::Integer)),
            ("empty", nullable(ContextType::Boolean)),
            ("entry_count", nullable(ContextType::Integer)),
            ("modified_at", nullable(string(SemanticFormat::DateTime))),
            ("created_at", nullable(string(SemanticFormat::DateTime))),
            ("sha256", nullable(sha256())),
        ],
    )
}

fn copy_path_schema() -> ObjectSchema {
    let path_value = ContextType::object(schema(
        "ppduster.filesystem.copy-result@1",
        [
            ("source", req(path())),
            ("destination", req(path())),
            ("copied", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    schema(
        "ppduster.filesystem.copy-path@1",
        [("path", req(path_value))],
    )
}

fn write_file_schema() -> ObjectSchema {
    let file = ContextType::object(schema(
        "ppduster.filesystem.file-result@1",
        [
            ("path", req(file_path())),
            ("bytes", req(ContextType::Integer)),
            ("sha256", req(sha256())),
            ("created", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    schema("ppduster.filesystem.write-file@1", [("file", req(file))])
}

fn remove_path_schema() -> ObjectSchema {
    let path_value = ContextType::object(schema(
        "ppduster.filesystem.removed-path@1",
        [
            ("value", req(path())),
            ("exists", req(ContextType::Boolean)),
            ("removed", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    schema(
        "ppduster.filesystem.remove-path@1",
        [("path", req(path_value))],
    )
}

fn repository_schema(id: &str) -> ObjectSchema {
    let repository = ContextType::object(schema(
        "ppduster.git.repository-result@1",
        [
            ("path", req(directory_path())),
            ("remote_url", req(git_url())),
            ("branch", nullable(git_ref())),
            ("exists", req(ContextType::Boolean)),
            ("operation", req(ContextType::STRING)),
            ("cloned", req(ContextType::Boolean)),
            ("fetched", req(ContextType::Boolean)),
            ("updated", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    schema(id, [("repository", req(repository))])
}

fn package_schema() -> ObjectSchema {
    let package = ContextType::object(schema(
        "ppduster.package.result@1",
        [
            ("name", req(identifier())),
            ("cask", req(ContextType::Boolean)),
            ("installed", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    schema(
        "ppduster.package.brew-install@1",
        [("package", req(package))],
    )
}

fn process_exit_schema() -> ObjectSchema {
    schema(
        "ppduster.process.exit@1",
        [
            ("exit_code", nullable(ContextType::Integer)),
            ("termination_signal", nullable(ContextType::Integer)),
            ("accepted", req(ContextType::Boolean)),
            (
                "success_exit_codes",
                req(ContextType::array(ContextType::Integer)),
            ),
        ],
    )
}

fn configuration_schema() -> ObjectSchema {
    let configuration = ContextType::object(schema(
        "ppduster.configuration.result@1",
        [
            ("npm_scope", req(ContextType::STRING)),
            ("npm_registry", req(url())),
            ("nuget_public_source", req(url())),
            ("nuget_private_source", req(url())),
            ("changed", req(ContextType::Boolean)),
            ("secrets_redacted", req(ContextType::Boolean)),
        ],
    ));
    schema(
        "ppduster.configuration.package-registries@1",
        [("configuration", req(configuration))],
    )
}

fn download_schema() -> ObjectSchema {
    let artifact = ContextType::object(schema(
        "ppduster.artifact.download-result@1",
        [
            ("url", req(url())),
            ("path", req(file_path())),
            ("sha256", req(sha256())),
            ("downloaded", req(ContextType::Boolean)),
            ("verified", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    schema(
        "ppduster.artifact.download@1",
        [("artifact", req(artifact))],
    )
}

fn extract_schema() -> ObjectSchema {
    let archive = ContextType::object(schema(
        "ppduster.artifact.extract-result@1",
        [
            ("source", req(file_path())),
            ("destination", req(directory_path())),
            ("format", req(ContextType::STRING)),
            ("extracted", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    schema("ppduster.artifact.extract@1", [("archive", req(archive))])
}

fn installation_schema(kind: ActionKind) -> ObjectSchema {
    let id = match kind {
        ActionKind::InstallDmg => "ppduster.installation.dmg@1",
        ActionKind::InstallPkg => "ppduster.installation.pkg@1",
        ActionKind::AppStoreInstall => "ppduster.installation.app-store@1",
        ActionKind::BambuStudioRelease => "ppduster.installation.bambu-studio@1",
        _ => unreachable!(),
    };
    let installation = ContextType::object(schema(
        "ppduster.installation.result@1",
        [
            ("source", nullable(path())),
            ("target", nullable(path())),
            ("app_name", nullable(ContextType::STRING)),
            ("name", nullable(ContextType::STRING)),
            ("id", nullable(ContextType::Integer)),
            ("operation", nullable(ContextType::STRING)),
            ("channel", nullable(ContextType::STRING)),
            ("bundle_identifier", nullable(identifier())),
            ("version", nullable(ContextType::STRING)),
            ("installed", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    // The concrete runners publish either `installation` or `application`.
    // Keeping both optional makes the contract explicit without conflating
    // package files and application identities.
    schema(
        id,
        [
            ("installation", opt(installation.clone())),
            ("application", opt(installation)),
        ],
    )
}

fn system_schema() -> ObjectSchema {
    let system = ContextType::object(schema(
        "ppduster.system.result@1",
        [
            ("platform", req(ContextType::STRING)),
            ("minimum_version", req(ContextType::STRING)),
            ("rosetta_required", req(ContextType::Boolean)),
            ("satisfied", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
        ],
    ));
    schema(
        "ppduster.system.macos-requirements@1",
        [("system", req(system))],
    )
}

fn license_schema() -> ObjectSchema {
    let license = ContextType::object(schema(
        "ppduster.license.result@1",
        [
            ("provider", req(ContextType::STRING)),
            ("method", req(ContextType::STRING)),
            ("activated", req(ContextType::Boolean)),
            ("changed", req(ContextType::Boolean)),
            ("secret_exposed", req(ContextType::Boolean)),
        ],
    ));
    schema("ppduster.license.activation@1", [("license", req(license))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const AUTH_POLICIES: [AuthPolicy; 3] = [
        AuthPolicy::None,
        AuthPolicy::GitCredential,
        AuthPolicy::Sudo,
    ];
    const ELEVATION_POLICIES: [ElevationPolicy; 2] =
        [ElevationPolicy::Forbidden, ElevationPolicy::Allow];
    const DANGEROUS_POLICIES: [bool; 2] = [false, true];

    fn executable_action_kinds() -> impl Iterator<Item = ActionKind> {
        ActionKind::ALL
            .into_iter()
            .filter(|kind| kind.is_graph_action())
    }

    #[test]
    fn registry_covers_every_action_kind_with_versioned_schemas() {
        let definitions = block_definitions();
        assert_eq!(definitions.len(), ActionKind::ALL.len());
        for (kind, definition) in ActionKind::ALL.into_iter().zip(definitions) {
            assert_eq!(definition.kind, kind);
            assert_eq!(definition.schema_version, 1);
            assert!(definition.input_schema.id.is_some());
            assert!(definition.output_schema.id.is_some());
        }
    }

    #[test]
    fn graph_editor_prototypes_cover_every_executable_action_kind() {
        for kind in ActionKind::ALL {
            match kind {
                ActionKind::ForEach | ActionKind::ForEachGitCloneIfMissing => {
                    assert!(default_action(kind).is_err());
                }
                _ => {
                    let action = default_action(kind).expect("executable action prototype");
                    assert_eq!(action.kind(), kind);
                    let encoded = serde_json::to_value(&action).unwrap();
                    let decoded: Action = serde_json::from_value(encoded).unwrap();
                    assert_eq!(decoded.kind(), kind);

                    let step = default_step(kind, format!("{}-1", kind.id())).unwrap();
                    assert!(
                        block_definition(kind).policy.accepts(&step),
                        "default policy for {} must match the registry contract",
                        kind.id()
                    );
                    step.validate().unwrap_or_else(|error| {
                        panic!("default step for {} must validate: {error}", kind.id())
                    });
                }
            }
        }
    }

    #[test]
    fn every_executable_action_has_a_valid_default_policy() {
        for kind in executable_action_kinds() {
            let step = default_step(kind, format!("{}-default", kind.id()))
                .expect("every executable action must have a default step");
            let capabilities = block_policy_capabilities(kind);

            assert!(
                capabilities.accepts(&step),
                "default policy for {} is outside its advertised capabilities: {:?}",
                kind.id(),
                step
            );
            step.validate().unwrap_or_else(|error| {
                panic!("default policy for {} must validate: {error}", kind.id())
            });
        }
    }

    #[test]
    fn policy_capability_matrix_matches_step_validation_for_every_executable_action() {
        let mut combinations = 0;

        for kind in executable_action_kinds() {
            let capabilities = block_policy_capabilities(kind);
            for auth in AUTH_POLICIES {
                for allow_elevation in ELEVATION_POLICIES {
                    for dangerous in DANGEROUS_POLICIES {
                        combinations += 1;
                        let mut step = default_step(kind, format!("{}-matrix", kind.id()))
                            .expect("executable action prototype");
                        step.auth = auth;
                        step.allow_elevation = allow_elevation;
                        step.dangerous = dangerous;

                        let advertised = capabilities.accepts(&step);
                        let validation = step.validate();
                        assert_eq!(
                            validation.is_ok(),
                            advertised,
                            "policy contract mismatch for {} with auth={auth:?}, elevation={allow_elevation:?}, dangerous={dangerous}: capabilities={}, validation={:?}",
                            kind.id(),
                            advertised,
                            validation
                        );
                    }
                }
            }
        }

        assert_eq!(
            combinations,
            executable_action_kinds().count()
                * AUTH_POLICIES.len()
                * ELEVATION_POLICIES.len()
                * DANGEROUS_POLICIES.len(),
            "the generated policy matrix must cover every combination"
        );
    }

    #[test]
    fn policy_capabilities_survive_serde_roundtrip_for_every_action_kind() {
        for kind in ActionKind::ALL {
            let capabilities = block_policy_capabilities(kind);
            assert_eq!(
                block_definition(kind).policy,
                capabilities,
                "block definition for {} must publish the canonical policy contract",
                kind.id()
            );

            let encoded = serde_json::to_value(&capabilities).unwrap_or_else(|error| {
                panic!(
                    "policy capabilities for {} must serialize: {error}",
                    kind.id()
                )
            });
            let decoded: BlockPolicyCapabilities =
                serde_json::from_value(encoded).unwrap_or_else(|error| {
                    panic!(
                        "policy capabilities for {} must deserialize: {error}",
                        kind.id()
                    )
                });
            assert_eq!(
                decoded,
                capabilities,
                "policy capabilities for {} changed during serde roundtrip",
                kind.id()
            );
        }
    }

    #[test]
    fn github_repository_discovery_exposes_only_its_valid_safe_policy() {
        let definition = block_definition(ActionKind::GithubListRepositories);
        let mut step = default_step(ActionKind::GithubListRepositories, "repositories").unwrap();
        assert!(definition.policy.accepts(&step));
        assert!(!definition.policy.allow_git_credentials);
        assert!(!definition.policy.allow_sudo);
        assert!(!definition.policy.allow_elevation);
        assert_eq!(definition.policy.dangerous, PolicyRequirement::Forbidden);

        step.auth = AuthPolicy::GitCredential;
        assert!(!definition.policy.accepts(&step));
        assert!(step.validate().is_err());
        step.auth = AuthPolicy::None;
        step.allow_elevation = ElevationPolicy::Allow;
        assert!(!definition.policy.accepts(&step));
        assert!(step.validate().is_err());
        step.allow_elevation = ElevationPolicy::Forbidden;
        step.dangerous = true;
        assert!(!definition.policy.accepts(&step));
        assert!(step.validate().is_err());
    }

    #[test]
    fn clone_url_input_accepts_both_github_url_fields() {
        let list = block_definition(ActionKind::GithubListRepositories);
        let repositories = list
            .output_schema
            .resolve(&[
                crate::automation::context::ContextPathSegment::field("github"),
                crate::automation::context::ContextPathSegment::field("repositories"),
                crate::automation::context::ContextPathSegment::index(0),
                crate::automation::context::ContextPathSegment::field("https_url"),
            ])
            .unwrap();
        let clone = block_definition(ActionKind::GitCloneIfMissing);
        let expected = clone.input_schema.field("repo").unwrap();
        assert!(expected
            .value_type
            .is_assignable_from(repositories.value_type));
    }

    #[test]
    fn fetch_and_fast_forward_require_a_non_nullable_branch() {
        for kind in [ActionKind::GitFetch, ActionKind::GitFastForward] {
            let branch = block_definition(kind)
                .input_schema
                .field("branch")
                .cloned()
                .expect("git update block has a branch input");
            assert!(branch.required);
            assert!(!branch.nullable);
            assert_eq!(branch.value_type, git_ref());
        }

        for kind in [ActionKind::GitClone, ActionKind::GitCloneIfMissing] {
            let branch = block_definition(kind)
                .input_schema
                .field("branch")
                .cloned()
                .expect("clone block has a branch input");
            assert!(!branch.required);
            assert!(branch.nullable);
        }
    }

    #[test]
    fn registry_never_exposes_secret_contents() {
        for definition in block_definitions() {
            for field in definition.output_schema.fields.values() {
                assert_ne!(
                    field.sensitivity,
                    crate::automation::context::Sensitivity::Secret
                );
            }
        }
    }

    #[test]
    fn command_and_script_environments_are_string_maps() {
        for kind in [ActionKind::RunCommand, ActionKind::RunScript] {
            let definition = block_definition(kind);
            let ContextType::Object { schema } = &definition
                .input_schema
                .field("env")
                .expect("process input has env")
                .value_type
            else {
                panic!("process env must be an object")
            };

            schema
                .validate_value(&json!({
                    "PATH": "/usr/local/bin:/usr/bin",
                    "LANG": "ru_RU.UTF-8"
                }))
                .unwrap();
            let resolved = schema
                .resolve(&[crate::automation::context::ContextPathSegment::field(
                    "DYNAMIC_ENV_KEY",
                )])
                .unwrap();
            assert_eq!(resolved.value_type, &ContextType::STRING);

            for invalid in [json!({ "DEBUG": true }), json!({ "CONFIG": {} })] {
                let error = schema.validate_value(&invalid).unwrap_err();
                assert!(matches!(
                    error.kind,
                    crate::automation::context::SchemaValidationErrorKind::TypeMismatch {
                        expected: crate::automation::context::ContextTypeName::String,
                        ..
                    }
                ));
            }
        }
    }
}

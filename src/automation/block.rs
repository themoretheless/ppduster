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
    GithubPreviewRepositories,
    GithubSelectRepositories,
    SelectArrayItems,
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
    pub const ALL: [Self; 28] = [
        Self::GithubListRepositories,
        Self::GithubPreviewRepositories,
        Self::GithubSelectRepositories,
        Self::SelectArrayItems,
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
            Self::GithubPreviewRepositories => "github-preview-repositories",
            Self::GithubSelectRepositories => "github-select-repositories",
            Self::SelectArrayItems => "select-array-items",
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
    /// Stable action ID followed by curated English and Russian search aliases.
    ///
    /// This metadata is descriptive only: it helps catalog clients find a
    /// block and never changes the persisted task or its execution semantics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search_terms: Vec<String>,
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
    #[serde(default)]
    pub catalog: BlockCatalog,
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

/// How a block is presented in pickers and catalogs.
///
/// Specialized blocks stay executable and searchable. They are hidden from
/// the default picker so vendor-specific and composite actions do not bury
/// the everyday filesystem / git / brew set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockCatalog {
    #[default]
    Core,
    Specialized,
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
            Self::GithubPreviewRepositories { .. } => ActionKind::GithubPreviewRepositories,
            Self::GithubSelectRepositories { .. } => ActionKind::GithubSelectRepositories,
            Self::SelectArrayItems { .. } => ActionKind::SelectArrayItems,
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
    match action {
        Action::SelectArrayItems { item_type, .. } => array_selection_definition(item_type.clone()),
        Action::GithubPreviewRepositories { .. } => github_snapshot_definition(),
        _ => block_definition(action.kind()),
    }
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
        ActionKind::GithubPreviewRepositories => Action::GithubPreviewRepositories {
            selected_repositories: Vec::new(),
        },
        ActionKind::GithubSelectRepositories => Action::GithubSelectRepositories {
            github: crate::automation::task::GithubContextInput {
                account: crate::automation::task::GithubAccountInput {
                    login: "github-user".into(),
                },
                repositories: Vec::new(),
            },
            expected_account_login: "github-user".into(),
            repository_ids: Vec::new(),
        },
        ActionKind::SelectArrayItems => Action::SelectArrayItems {
            source: None,
            item_type: ContextType::STRING,
            selected_items: Vec::new(),
        },
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

pub const fn block_catalog(kind: ActionKind) -> BlockCatalog {
    match kind {
        ActionKind::BambuStudioRelease
        | ActionKind::ActivateLicense
        | ActionKind::ConfigurePackageRegistryFiles
        | ActionKind::GitClone
        | ActionKind::ForEachGitCloneIfMissing => BlockCatalog::Specialized,
        _ => BlockCatalog::Core,
    }
}

pub const fn block_policy_capabilities(kind: ActionKind) -> BlockPolicyCapabilities {
    match kind {
        ActionKind::GithubListRepositories
        | ActionKind::GithubPreviewRepositories
        | ActionKind::GithubSelectRepositories
        | ActionKind::SelectArrayItems
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
        ActionKind::GithubPreviewRepositories => {
            return github_snapshot_definition();
        }
        ActionKind::GithubSelectRepositories => (
            "Выбрать репозитории GitHub",
            "GitHub",
            schema(
                "ppduster.github.select-repositories.inputs@1",
                [("github", req(github_context_type()))],
            ),
            github_repositories_schema(),
            true,
            false,
        ),
        ActionKind::SelectArrayItems => {
            let definition = array_selection_definition(ContextType::Any);
            return definition;
        }
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
                    ("package", req(package_name())),
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
        search_terms: block_search_terms(kind),
        input_schema: inputs,
        output_schema: outputs,
        read_only,
        may_use_secrets,
        policy: block_policy_capabilities(kind),
        catalog: block_catalog(kind),
    }
}

fn block_search_terms(kind: ActionKind) -> Vec<String> {
    let aliases: &[&str] = match kind {
        ActionKind::GithubListRepositories => &[
            "github",
            "гитхаб",
            "гит",
            "list repositories",
            "account repositories",
            "репозитории github",
            "получить репозитории",
            "репозитории аккаунта",
        ],
        ActionKind::GithubPreviewRepositories => &[
            "github",
            "гитхаб",
            "repository snapshot",
            "saved repositories",
            "выбрать репозитории",
            "сохраненные репозитории",
            "снимок репозиториев",
        ],
        ActionKind::GithubSelectRepositories => &[
            "github",
            "гитхаб",
            "гит",
            "select repositories",
            "repository picker",
            "выбрать репозитории",
            "выбор репозиториев",
            "репозитории github",
        ],
        ActionKind::SelectArrayItems => &[
            "array",
            "data selection",
            "select array items",
            "массив",
            "выбрать элементы массива",
            "выбор данных",
        ],
        ActionKind::ForEach => &[
            "for each",
            "foreach",
            "loop",
            "для каждого",
            "цикл",
            "перебор",
        ],
        ActionKind::ForEachGitCloneIfMissing => &[
            "гит",
            "clone every repository",
            "git repository loop",
            "clone if missing loop",
            "клонировать каждый репозиторий",
            "цикл репозиториев git",
            "клонировать отсутствующие репозитории",
        ],
        ActionKind::CreateDirectory => &[
            "create directory",
            "create folder",
            "filesystem directory",
            "создать папку",
            "создать каталог",
            "файловая система",
        ],
        ActionKind::InspectPath => &[
            "inspect path",
            "path metadata",
            "filesystem check",
            "проверить путь",
            "метаданные пути",
            "проверка файла или папки",
        ],
        ActionKind::CopyPath => &[
            "copy path",
            "copy file",
            "copy directory",
            "копировать путь",
            "копировать файл",
            "копировать папку",
        ],
        ActionKind::WriteFile => &[
            "write file",
            "file content",
            "create text file",
            "записать файл",
            "содержимое файла",
            "создать текстовый файл",
        ],
        ActionKind::RemovePath => &[
            "remove path",
            "move to trash",
            "delete file",
            "удалить путь",
            "переместить в корзину",
            "удалить файл или папку",
        ],
        ActionKind::GitClone => &[
            "гит",
            "git clone",
            "clone repository",
            "sync repository",
            "клонировать git",
            "клонировать репозиторий",
            "синхронизировать репозиторий",
        ],
        ActionKind::GitInspect => &[
            "гит",
            "git inspect",
            "repository status",
            "inspect repository",
            "проверить git",
            "состояние репозитория",
            "проверить репозиторий",
        ],
        ActionKind::GitCloneIfMissing => &[
            "гит",
            "git clone if missing",
            "conditional clone",
            "clone absent repository",
            "клонировать если отсутствует",
            "условное клонирование",
            "клонировать отсутствующий репозиторий",
        ],
        ActionKind::GitFetch => &[
            "гит",
            "git fetch",
            "fetch remote branch",
            "download git refs",
            "получить remote ветку",
            "загрузить ветку git",
            "получить изменения репозитория",
        ],
        ActionKind::GitFastForward => &[
            "гит",
            "git fast forward",
            "update branch",
            "fast forward branch",
            "актуализировать ветку",
            "обновить ветку git",
            "перемотать ветку вперед",
        ],
        ActionKind::BrewInstall => &[
            "homebrew",
            "brew install",
            "install package",
            "установить homebrew пакет",
            "установить пакет brew",
            "пакетный менеджер",
        ],
        ActionKind::RunCommand => &[
            "run command",
            "execute command",
            "shell process",
            "выполнить команду",
            "запустить процесс",
            "командная оболочка",
        ],
        ActionKind::RunScript => &[
            "run script",
            "execute script",
            "shell script",
            "выполнить скрипт",
            "запустить скрипт",
            "сценарий командной оболочки",
        ],
        ActionKind::ConfigurePackageRegistryFiles => &[
            "package registry",
            "npm registry",
            "nuget registry",
            "реестр пакетов",
            "настроить npm",
            "настроить nuget",
        ],
        ActionKind::DownloadFile => &[
            "download file",
            "http download",
            "download artifact",
            "скачать файл",
            "загрузка по url",
            "скачать артефакт",
        ],
        ActionKind::ExtractArchive => &[
            "extract archive",
            "unpack zip",
            "unpack tar",
            "распаковать архив",
            "извлечь архив",
            "распаковать zip или tar",
        ],
        ActionKind::InstallDmg => &[
            "install dmg",
            "macos disk image",
            "dmg installer",
            "установить dmg",
            "образ диска macos",
            "установщик dmg",
        ],
        ActionKind::InstallPkg => &[
            "install pkg",
            "macos installer package",
            "pkg installer",
            "установить pkg",
            "пакет установщика macos",
            "установщик pkg",
        ],
        ActionKind::MacosRequirements => &[
            "macos requirements",
            "system check",
            "rosetta requirement",
            "требования macos",
            "проверка системы",
            "требование rosetta",
        ],
        ActionKind::AppStoreInstall => &[
            "app store install",
            "mac app store",
            "install store app",
            "установить из app store",
            "магазин приложений mac",
            "установить приложение из магазина",
        ],
        ActionKind::BambuStudioRelease => &[
            "bambu studio",
            "install bambu studio",
            "update bambu studio",
            "установить bambu studio",
            "обновить bambu studio",
            "релиз bambu studio",
        ],
        ActionKind::ActivateLicense => &[
            "activate license",
            "license activation",
            "vendor license ui",
            "активировать лицензию",
            "активация лицензии",
            "интерфейс лицензии поставщика",
        ],
    };

    std::iter::once(kind.id())
        .chain(aliases.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn github_snapshot_definition() -> BlockDefinition {
    let mut repository = github_repository_type();
    let ContextType::Object {
        schema: repository_schema,
    } = &mut repository
    else {
        unreachable!("GitHub repository type is an object")
    };
    repository_schema
        .fields
        .get_mut("private")
        .expect("GitHub repository has private")
        .allowed_values = vec![serde_json::json!(false)];
    BlockDefinition {
        kind: ActionKind::GithubPreviewRepositories,
        schema_version: 2,
        title: "Выбрать репозитории GitHub".into(),
        category: "GitHub".into(),
        search_terms: block_search_terms(ActionKind::GithubPreviewRepositories),
        input_schema: schema("ppduster.github.preview-repositories.inputs@1", []),
        output_schema: schema(
            "ppduster.github.selected-repositories@1",
            [("repositories", req(ContextType::array(repository)))],
        ),
        read_only: true,
        may_use_secrets: false,
        policy: block_policy_capabilities(ActionKind::GithubPreviewRepositories),
        catalog: block_catalog(ActionKind::GithubPreviewRepositories),
    }
}

fn array_selection_definition(item_type: ContextType) -> BlockDefinition {
    BlockDefinition {
        kind: ActionKind::SelectArrayItems,
        schema_version: 1,
        title: "Выбрать элементы массива".into(),
        category: "Данные".into(),
        search_terms: block_search_terms(ActionKind::SelectArrayItems),
        input_schema: schema("ppduster.array.selection.inputs@1", []),
        output_schema: schema(
            "ppduster.array.selection@1",
            [("items", req(ContextType::array(item_type)))],
        ),
        read_only: true,
        may_use_secrets: false,
        policy: block_policy_capabilities(ActionKind::SelectArrayItems),
        catalog: block_catalog(ActionKind::SelectArrayItems),
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

fn package_name() -> ContextType {
    string(SemanticFormat::PackageName)
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
            // GitHub GraphQL node IDs are opaque strings. In particular,
            // legacy IDs can be base64 values containing `=` padding.
            ("id", req(string(SemanticFormat::OpaqueId))),
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

fn github_context_type() -> ContextType {
    let account = ContextType::object(schema(
        "ppduster.github.account@1",
        [("login", req(identifier()))],
    ));
    ContextType::object(schema(
        "ppduster.github.context@1",
        [
            ("account", req(account)),
            (
                "repositories",
                req(ContextType::array(github_repository_type())),
            ),
        ],
    ))
}

fn github_repositories_schema() -> ObjectSchema {
    schema(
        "ppduster.github.repositories@1",
        [("github", req(github_context_type()))],
    )
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
            ("name", req(package_name())),
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
    fn specialized_catalog_covers_vendor_and_composite_blocks_only() {
        assert_eq!(
            block_catalog(ActionKind::BambuStudioRelease),
            BlockCatalog::Specialized
        );
        assert_eq!(
            block_catalog(ActionKind::ActivateLicense),
            BlockCatalog::Specialized
        );
        assert_eq!(
            block_catalog(ActionKind::ConfigurePackageRegistryFiles),
            BlockCatalog::Specialized
        );
        assert_eq!(block_catalog(ActionKind::GitClone), BlockCatalog::Specialized);
        assert_eq!(
            block_catalog(ActionKind::ForEachGitCloneIfMissing),
            BlockCatalog::Specialized
        );
        assert_eq!(block_catalog(ActionKind::CreateDirectory), BlockCatalog::Core);
        assert_eq!(block_catalog(ActionKind::BrewInstall), BlockCatalog::Core);
        assert_eq!(
            block_definition(ActionKind::BambuStudioRelease).catalog,
            BlockCatalog::Specialized
        );
        assert_eq!(
            block_definition(ActionKind::InspectPath).catalog,
            BlockCatalog::Core
        );
        let specialized = [
            ActionKind::BambuStudioRelease,
            ActionKind::ActivateLicense,
            ActionKind::ConfigurePackageRegistryFiles,
            ActionKind::GitClone,
            ActionKind::ForEachGitCloneIfMissing,
        ];
        for kind in ActionKind::ALL {
            let expected = if specialized.contains(&kind) {
                BlockCatalog::Specialized
            } else {
                BlockCatalog::Core
            };
            assert_eq!(block_catalog(kind), expected, "{}", kind.id());
            assert_eq!(block_definition(kind).catalog, expected, "{}", kind.id());
        }
    }

    #[test]
    fn registry_covers_every_action_kind_with_versioned_schemas() {
        let definitions = block_definitions();
        assert_eq!(definitions.len(), ActionKind::ALL.len());
        for (kind, definition) in ActionKind::ALL.into_iter().zip(definitions) {
            assert_eq!(definition.kind, kind);
            let expected_schema_version = match kind {
                ActionKind::GithubPreviewRepositories => 2,
                _ => 1,
            };
            assert_eq!(
                definition.schema_version,
                expected_schema_version,
                "registry schema version changed for {} without updating its explicit contract",
                kind.id()
            );
            assert!(definition.input_schema.id.is_some());
            assert!(definition.output_schema.id.is_some());
            assert_eq!(
                definition.search_terms.first().map(String::as_str),
                Some(kind.id()),
                "{} must publish its stable action ID as the first search term",
                kind.id()
            );
            assert!(
                definition.search_terms[1..].iter().any(|term| term
                    .chars()
                    .any(|character| character.is_ascii_alphabetic())),
                "{} must publish an English search alias",
                kind.id()
            );
            assert!(
                definition.search_terms[1..].iter().any(|term| term
                    .chars()
                    .any(|character| matches!(character, '\u{0400}'..='\u{04ff}'))),
                "{} must publish a Russian search alias",
                kind.id()
            );
            assert!(
                definition.search_terms.iter().all(|term| !term.is_empty()
                    && term.trim() == term
                    && term.to_lowercase() == *term),
                "{} search terms must be non-empty, trimmed, and lowercase",
                kind.id()
            );
            let unique = definition
                .search_terms
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                unique.len(),
                definition.search_terms.len(),
                "{} search terms must not contain duplicates",
                kind.id()
            );
        }
    }

    #[test]
    fn block_search_terms_are_serde_compatible_and_optional_on_legacy_input() {
        let definition = block_definition(ActionKind::GitInspect);
        let encoded = serde_json::to_value(&definition).unwrap();

        assert_eq!(
            encoded["search_terms"][0],
            serde_json::Value::String(ActionKind::GitInspect.id().into())
        );
        assert_eq!(
            serde_json::from_value::<BlockDefinition>(encoded.clone()).unwrap(),
            definition
        );

        let mut legacy = encoded;
        legacy.as_object_mut().unwrap().remove("search_terms");
        let decoded = serde_json::from_value::<BlockDefinition>(legacy).unwrap();
        assert!(decoded.search_terms.is_empty());
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
        let repository_id = definition
            .output_schema
            .resolve(&[
                crate::automation::context::ContextPathSegment::field("github"),
                crate::automation::context::ContextPathSegment::field("repositories"),
                crate::automation::context::ContextPathSegment::index(0),
                crate::automation::context::ContextPathSegment::field("id"),
            ])
            .unwrap();
        assert_eq!(
            repository_id.value_type,
            &ContextType::string(SemanticFormat::OpaqueId)
        );
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
    fn github_repository_selection_keeps_authored_policy_out_of_binding_schema() {
        let definition = block_definition(ActionKind::GithubSelectRepositories);
        assert_eq!(
            definition
                .input_schema
                .fields
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["github"]
        );
        assert!(definition
            .input_schema
            .field("expected_account_login")
            .is_none());
        assert!(definition.input_schema.field("repository_ids").is_none());
        assert_eq!(
            definition.output_schema.id,
            block_definition(ActionKind::GithubListRepositories)
                .output_schema
                .id
        );

        let mut action = default_action(ActionKind::GithubSelectRepositories).unwrap();
        let Action::GithubSelectRepositories {
            github,
            expected_account_login,
            repository_ids,
        } = &mut action
        else {
            unreachable!()
        };
        assert_eq!(github.account.login, *expected_account_login);
        assert!(github.repositories.is_empty());
        assert!(repository_ids.is_empty());

        github
            .repositories
            .push(crate::automation::task::GithubRepositoryInput {
                id: "R_private".into(),
                owner: "private-owner".into(),
                name: "private-repository".into(),
                full_name: "private-owner/private-repository".into(),
                https_url: "https://github.com/private-owner/private-repository".into(),
                ssh_url: "git@github.com:private-owner/private-repository.git".into(),
                default_branch: Some("main".into()),
                private: true,
                archived: false,
            });
        let encoded = serde_json::to_value(&action).unwrap();
        assert!(encoded["github"].get("repositories").is_none());
        let decoded: Action = serde_json::from_value(encoded).unwrap();
        let Action::GithubSelectRepositories { github, .. } = decoded else {
            unreachable!()
        };
        assert!(github.repositories.is_empty());
    }

    #[test]
    fn github_preview_publishes_the_persisted_public_selection() {
        let preview = block_definition(ActionKind::GithubPreviewRepositories);
        assert!(preview.read_only);
        assert!(preview.input_schema.fields.is_empty());
        assert!(preview.output_schema.field("repositories").is_some());
        assert_eq!(
            preview.output_schema.id.as_deref(),
            Some("ppduster.github.selected-repositories@1")
        );
    }

    #[test]
    fn array_selection_definition_preserves_the_declared_item_type() {
        let item_type = ContextType::object(
            ObjectSchema::new("example.item@1")
                .with_field("name", FieldSchema::required(ContextType::STRING)),
        );
        let action = Action::SelectArrayItems {
            source: None,
            item_type: item_type.clone(),
            selected_items: vec![serde_json::json!({ "name": "alpha" })],
        };
        let definition = definition_for_action(&action);
        assert!(definition.read_only);
        assert!(definition.input_schema.fields.is_empty());
        assert_eq!(
            definition.output_schema.id.as_deref(),
            Some("ppduster.array.selection@1")
        );
        assert_eq!(
            definition.output_schema.field("items").unwrap().value_type,
            ContextType::array(item_type)
        );
    }

    #[test]
    fn array_selection_default_step_is_a_valid_empty_string_snapshot() {
        let step = default_step(ActionKind::SelectArrayItems, "select-items").unwrap();
        let Action::SelectArrayItems {
            source,
            item_type,
            selected_items,
        } = &step.action
        else {
            unreachable!()
        };
        assert!(source.is_none());
        assert_eq!(item_type, &ContextType::STRING);
        assert!(selected_items.is_empty());
        step.validate().unwrap();
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

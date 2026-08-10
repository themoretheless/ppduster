use crate::automation::context::{ContextType, FieldSchema, ObjectSchema, SemanticFormat};
use crate::automation::task::Action;
use serde::{Deserialize, Serialize};

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
}

impl BlockDefinition {
    pub fn output_schema_id(&self) -> Option<&str> {
        self.output_schema.id.as_deref()
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
                    ("on_conflict", opt(ContextType::STRING)),
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
                    ("format", opt(ContextType::STRING)),
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
                    ("provider", req(ContextType::STRING)),
                    ("method", req(ContextType::STRING)),
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
            ("kind", nullable(ContextType::STRING)),
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
                ("shell", opt(ContextType::STRING)),
            ],
        ),
        ActionKind::RunScript => schema(
            "ppduster.run-script.inputs@1",
            [
                ("interpreter", req(ContextType::STRING)),
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
                ("operation", opt(ContextType::STRING)),
            ],
        ),
        ActionKind::BambuStudioRelease => schema(
            "ppduster.bambu-studio-release.inputs@1",
            [("channel", opt(ContextType::STRING))],
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
            ("id", req(identifier())),
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

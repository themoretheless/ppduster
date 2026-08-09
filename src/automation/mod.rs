pub mod loader;
mod package_registry;
pub mod package_secrets;
pub mod runner;
pub mod task;

pub use loader::{PackTrust, TaskPack, TaskSource};
pub use runner::{
    describe_step, run_task, ActionOutcome, ActionPlan, AutomationError, PathMetadataOutput,
    ProcessExitOutput, RunOptions, RunReport, StepLogEntry, StepOutput, StepReport, StepStatus,
};
pub use task::{
    Action, ActivateLicenseAction, AppBundleIdentity, AppStoreInstallAction, AppStoreOperation,
    ArchiveFormat, AuthPolicy, BambuStudioReleaseAction, Check, Checksum, CopyPathAction,
    CreateDirectoryAction, ElevationPolicy, EncryptedSecretsSpec, InspectPathAction, LicenseMethod,
    LicenseProvider, NpmRegistryFileSpec, NugetRegistryFileSpec, PathExpectation, PathKind,
    ReleaseChannel, RemovePathAction, ScriptInterpreter, ShellMode, Step, StepCondition, Task,
    TaskFile, TrustRequirement, WriteConflictPolicy, WriteFileAction,
};

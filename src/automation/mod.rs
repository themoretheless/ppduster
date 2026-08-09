pub mod loader;
pub mod runner;
pub mod task;

pub use loader::{PackTrust, TaskPack, TaskSource};
pub use runner::{
    describe_step, run_task, ActionOutcome, ActionPlan, AutomationError, PathMetadataOutput,
    RunOptions, RunReport, StepLogEntry, StepOutput, StepReport, StepStatus,
};
pub use task::{
    Action, ActivateLicenseAction, AppBundleIdentity, AppStoreInstallAction, AppStoreOperation,
    ArchiveFormat, AuthPolicy, BambuStudioReleaseAction, Check, Checksum, CreateDirectoryAction,
    ElevationPolicy, InspectPathAction, LicenseMethod, LicenseProvider, PathExpectation, PathKind,
    ReleaseChannel, ScriptInterpreter, ShellMode, Step, Task, TaskFile, TrustRequirement,
};

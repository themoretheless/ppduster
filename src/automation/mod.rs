pub mod loader;
pub mod runner;
pub mod task;

pub use loader::{PackTrust, TaskPack, TaskSource};
pub use runner::{
    run_task, ActionOutcome, ActionPlan, AutomationError, RunOptions, RunReport, StepLogEntry,
    StepReport, StepStatus,
};
pub use task::{
    Action, ActivateLicenseAction, AppBundleIdentity, AppStoreInstallAction, AppStoreOperation,
    AuthPolicy, BambuStudioReleaseAction, Check, Checksum, ElevationPolicy, LicenseMethod,
    LicenseProvider, ReleaseChannel, ShellMode, Step, Task, TaskFile, TrustRequirement,
};

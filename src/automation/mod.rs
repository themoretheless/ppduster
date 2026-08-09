pub mod loader;
mod package_registry;
pub mod package_secrets;
pub mod runner;
pub mod task;

pub use loader::{PackTrust, TaskPack, TaskSource};
pub use runner::{
    run_task, ActionOutcome, ActionPlan, AutomationError, RunOptions, RunReport, StepLogEntry,
    StepReport, StepStatus,
};
pub use task::{
    Action, AuthPolicy, Check, Checksum, ElevationPolicy, EncryptedSecretsSpec,
    NpmRegistryFileSpec, NugetRegistryFileSpec, ShellMode, Step, Task, TaskFile, TrustRequirement,
};

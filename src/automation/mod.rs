pub mod loader;
pub mod runner;
pub mod task;

pub use loader::{PackTrust, TaskPack, TaskSource};
pub use runner::{run_task, ActionOutcome, ActionPlan, AutomationError, RunOptions, RunReport};
pub use task::{
    Action, Check, Checksum, ElevationPolicy, ShellMode, Step, Task, TaskFile, TrustRequirement,
};

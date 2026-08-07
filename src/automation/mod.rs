//! Automation subsystem — declarative task runner.
//!
//! Separate from the cleanup/scan subsystem. Loads and executes
//! [`task::Task`] definitions (YAML) with full dry-run support.
//!
//! # Public surface
//! - [`task`] — data model (Task, Step, StepKind, ...)
//! - [`runner`] — RunContext, RunMode, RunReport, run_task()

pub mod runner;
pub mod task;

pub use runner::{run_task, AutomationError, RunContext, RunMode, RunOptions, RunReport, StepOutcome, StepResult};
pub use task::{Step, StepKind, Task};

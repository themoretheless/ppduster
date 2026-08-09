//! Standalone Mac App Store client used by the `ppstore` binary.

pub mod app_store;
pub mod app_store_cli;
pub mod app_store_installer;

use clap::ValueEnum;

/// Machine-readable or human-readable CLI output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}

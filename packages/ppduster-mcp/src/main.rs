use anyhow::Result;
use clap::Parser;
use ppduster_mcp::PpdusterMcp;
use rmcp::{transport::stdio, ServiceExt};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "ppduster-mcp",
    version,
    about = "Create validated ppduster Scenario Flow schemes over MCP"
)]
struct Cli {
    /// Existing directory below which create_scheme may create YAML files.
    #[arg(long, value_name = "DIRECTORY", default_value = ".")]
    output_dir: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let server = PpdusterMcp::new(cli.output_dir)?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

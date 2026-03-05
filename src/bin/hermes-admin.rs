use clap::Parser;
use hermes_rs::admin::{run_admin_command, AdminCli};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = AdminCli::parse();
    run_admin_command(cli.command).await
}

use crate::config::Config;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

#[derive(Parser)]
#[command(name = "hermes-admin")]
#[command(about = "Administrative tools for Hermes-RS")]
pub struct AdminCli {
    #[command(subcommand)]
    pub command: AdminCommands,
}

#[derive(Subcommand)]
pub enum AdminCommands {
    /// Validate configuration file
    ValidateConfig {
        /// Path to configuration file
        #[arg(short, long, default_value = "config.yml")]
        config: PathBuf,
    },
    /// Test webhook template rendering
    TestTemplate {
        /// Path to configuration file
        #[arg(short, long, default_value = "config.yml")]
        config: PathBuf,
        /// Endpoint to test
        #[arg(short, long)]
        endpoint: String,
        /// JSON payload to test with
        #[arg(short, long)]
        payload: String,
    },
    /// List all registered endpoints
    ListEndpoints {
        /// Path to configuration file
        #[arg(short, long, default_value = "config.yml")]
        config: PathBuf,
    },
}

pub async fn run_admin_command(cmd: AdminCommands) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        AdminCommands::ValidateConfig { config } => {
            validate_config(&config).await?;
            println!("✅ Configuration is valid");
        }
        AdminCommands::TestTemplate {
            config,
            endpoint,
            payload,
        } => {
            test_template(&config, &endpoint, &payload).await?;
        }
        AdminCommands::ListEndpoints { config } => {
            list_endpoints(&config).await?;
        }
    }
    Ok(())
}

async fn validate_config(config_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config_path).await?;

    info!("Validating {} webhook registers", config.registers.len());
    config.validate()?;

    Ok(())
}

async fn test_template(
    config_path: &PathBuf,
    endpoint: &str,
    payload: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config_path).await?;

    // Find the register for this endpoint
    let register = config
        .registers
        .iter()
        .find(|r| r.endpoint == endpoint)
        .ok_or_else(|| format!("Endpoint '{}' not found", endpoint))?;

    // Parse the payload
    let payload_json: serde_json::Value = serde_json::from_str(payload)?;
    let template_data = crate::json_to_template_data(&payload_json);

    // Render the register template, then every per-target override.
    let mut to_render: Vec<(String, &str)> = vec![("default template".to_string(), &register.template)];
    for (j, t) in register.all_targets().enumerate() {
        if let Some(tpl) = &t.template {
            to_render.push((format!("target '{}' ({})", t.label(j), t.type_name()), tpl));
        }
    }

    let hbs = crate::template::new_handlebars();
    for (label, tpl) in to_render {
        let rendered = hbs.render_template(tpl, &template_data)?;

        println!("📝 {} rendered successfully:", label);
        println!("{}", rendered);

        // Validate that rendered output is valid JSON
        let _: serde_json::Value = serde_json::from_str(&rendered)?;
        println!("✅ Rendered output is valid JSON");
    }

    Ok(())
}

async fn list_endpoints(config_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config_path).await?;

    println!("📋 Registered webhook endpoints:");
    println!(
        "{:<8} {:<28} {:<10} {:<14} {}",
        "METHOD", "ENDPOINT", "TYPE", "TARGET", "DESTINATION"
    );
    println!("{}", "-".repeat(100));

    for register in &config.registers {
        for (j, target) in register.all_targets().enumerate() {
            println!(
                "{:<8} {:<28} {:<10} {:<14} {}",
                register.method,
                register.endpoint,
                target.type_name(),
                target.label(j),
                target.describe()
            );
        }
        if register.all_targets().count() > 1 {
            println!("{:<8} {:<28} ↳ broadcast, delivery={:?}", "", "", register.delivery);
        }
    }

    Ok(())
}

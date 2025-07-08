use clap::{Parser, Subcommand};
use crate::config::Config;
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
        AdminCommands::TestTemplate { config, endpoint, payload } => {
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
    
    // Validate each register
    for (i, register) in config.registers.iter().enumerate() {
        // Check endpoint format
        if !register.endpoint.starts_with('/') {
            return Err(format!("Register {}: endpoint must start with '/'", i).into());
        }
        
        // Check HTTP method
        let valid_methods = ["GET", "POST", "PUT", "DELETE", "PATCH"];
        if !valid_methods.contains(&register.method.to_uppercase().as_str()) {
            return Err(format!("Register {}: invalid HTTP method '{}'", i, register.method).into());
        }
        
        // Check target URL
        if register.target.url.is_empty() {
            return Err(format!("Register {}: target URL cannot be empty", i).into());
        }
        
        // Validate template by trying to compile it
        let mut handlebars = handlebars::Handlebars::new();
        handlebars.register_template_string("test", &register.template)
            .map_err(|e| format!("Register {}: template error: {}", i, e))?;
    }
    
    Ok(())
}

async fn test_template(
    config_path: &PathBuf, 
    endpoint: &str, 
    payload: &str
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config_path).await?;
    
    // Find the register for this endpoint
    let register = config.registers.iter()
        .find(|r| r.endpoint == endpoint)
        .ok_or_else(|| format!("Endpoint '{}' not found", endpoint))?;
    
    // Parse the payload
    let payload_json: serde_json::Value = serde_json::from_str(payload)?;
    
    // Create template data
    let template_data = crate::json_to_template_data(&payload_json);
    
    // Render template
    let mut handlebars = handlebars::Handlebars::new();
    handlebars.register_template_string("test", &register.template)?;
    let rendered = handlebars.render("test", &template_data)?;
    
    println!("📝 Template rendered successfully:");
    println!("{}", rendered);
    
    // Validate that rendered output is valid JSON
    let _: serde_json::Value = serde_json::from_str(&rendered)?;
    println!("✅ Rendered output is valid JSON");
    
    Ok(())
}

async fn list_endpoints(config_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config_path).await?;
    
    println!("📋 Registered webhook endpoints:");
    println!("{:<8} {:<30} {:<8} {}", "METHOD", "ENDPOINT", "TARGET", "URL");
    println!("{}", "-".repeat(80));
    
    for register in &config.registers {
        println!(
            "{:<8} {:<30} {:<8} {}",
            register.method,
            register.endpoint,
            register.target.method,
            register.target.url
        );
    }
    
    Ok(())
}




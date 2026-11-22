use crate::config::Config;
use crate::outbox::{now_ms, JobState, Selector, Store};
use clap::{Args, Parser, Subcommand};
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
    /// Inspect and manage the durable outbox used by `mode: async`
    Outbox(OutboxArgs),
}

#[derive(Args)]
pub struct OutboxArgs {
    /// Path to configuration file (locates the outbox database; the
    /// HERMES_OUTBOX_PATH env var overrides it, as for the server)
    #[arg(short, long, default_value = "config.yml", global = true)]
    pub config: PathBuf,
    #[command(subcommand)]
    pub action: OutboxAction,
}

#[derive(Subcommand)]
pub enum OutboxAction {
    /// Job counts per state and the age of the oldest waiting job
    Stats,
    /// List jobs, newest first
    List {
        /// Only jobs in this state: pending | in_flight | delivered | dead
        #[arg(long)]
        state: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Re-queue dead-lettered jobs with a fresh attempt budget
    Replay(Select),
    /// Permanently delete dead-lettered jobs
    Purge(Select),
}

/// Which dead letters to act on. One of the three is required, so a bare
/// `replay`/`purge` can never touch everything by accident.
#[derive(Args)]
pub struct Select {
    /// A single job id (see `outbox list`)
    #[arg(long, conflicts_with_all = ["endpoint", "all"])]
    pub id: Option<i64>,
    /// Every dead job of this endpoint, e.g. /webhook/github
    #[arg(long, conflicts_with = "all")]
    pub endpoint: Option<String>,
    /// Every dead job
    #[arg(long)]
    pub all: bool,
}

impl Select {
    fn selector(&self) -> Result<Selector, String> {
        match (self.id, &self.endpoint, self.all) {
            (Some(id), None, false) => Ok(Selector::Id(id)),
            (None, Some(e), false) => Ok(Selector::Endpoint(e.clone())),
            (None, None, true) => Ok(Selector::All),
            _ => Err("specify exactly one of --id <ID>, --endpoint <PATH> or --all".to_string()),
        }
    }
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
        AdminCommands::Outbox(args) => {
            run_outbox(args).await?;
        }
    }
    Ok(())
}

async fn run_outbox(args: OutboxArgs) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(&args.config).await?;
    let path = config.outbox_path();
    if !path.exists() {
        return Err(format!(
            "outbox database not found at {} (nothing has been queued yet, or the path differs from the server's)",
            path.display()
        )
        .into());
    }
    // Safe alongside a running server: the database is in WAL mode.
    let store = Store::open(&path)?;
    let now = now_ms();

    match args.action {
        OutboxAction::Stats => {
            let s = store.stats(now)?;
            println!("📬 Outbox {}", path.display());
            println!("   pending    {}", s.pending);
            println!("   in_flight  {}", s.in_flight);
            println!("   delivered  {}", s.delivered);
            println!("   dead       {}{}", s.dead, if s.dead > 0 { "   ← see `outbox list --state dead`" } else { "" });
            if let Some(age) = s.oldest_pending_age_ms {
                println!("   oldest waiting job: {}", human_age(age));
            }
        }
        OutboxAction::List { state, limit } => {
            let filter = match state.as_deref() {
                None => None,
                Some(s) => Some(JobState::parse(s).ok_or("state must be pending, in_flight, delivered or dead")?),
            };
            let rows = store.list(filter, limit)?;
            println!(
                "{:<7} {:<10} {:<9} {:<26} {:<16} {:<9} {}",
                "ID", "STATE", "ATTEMPTS", "ENDPOINT", "TARGET", "AGE", "LAST ERROR"
            );
            println!("{}", "-".repeat(110));
            for r in &rows {
                let err = r.last_error.as_deref().unwrap_or("");
                let err: String = err.chars().take(60).collect();
                println!(
                    "{:<7} {:<10} {:<9} {:<26} {:<16} {:<9} {}",
                    r.id, r.state, r.attempts, r.endpoint, r.target_label, human_age(now - r.created_at), err
                );
            }
            println!("({} shown)", rows.len());
        }
        OutboxAction::Replay(sel) => {
            let n = store.replay(&sel.selector()?, now)?;
            println!("♻️  Re-queued {n} dead job(s). A running server picks them up within its poll interval.");
        }
        OutboxAction::Purge(sel) => {
            let n = store.purge_dead(&sel.selector()?)?;
            println!("🗑️  Deleted {n} dead job(s).");
        }
    }
    Ok(())
}

fn human_age(ms: i64) -> String {
    let s = (ms.max(0) / 1000) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
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
        "{:<8} {:<36} {:<10} {:<14} {}",
        "METHOD", "ENDPOINT", "TYPE", "TARGET", "DESTINATION"
    );
    println!("{}", "-".repeat(100));

    for register in &config.registers {
        for (j, target) in register.all_targets().enumerate() {
            println!(
                "{:<8} {:<36} {:<10} {:<14} {}",
                register.method,
                {
                    let mut tags = Vec::new();
                    if register.mode == crate::config::DispatchMode::Async {
                        tags.push("async".to_string());
                    }
                    tags.push(match &register.auth {
                        Some(a) => format!("auth:{}", a.kind()),
                        None => "OPEN".to_string(),
                    });
                    format!("{} ({})", register.endpoint, tags.join(", "))
                },
                target.type_name(),
                target.label(j),
                target.describe()
            );
        }
        if register.all_targets().count() > 1 {
            if register.mode == crate::config::DispatchMode::Async {
                println!("{:<8} {:<36} ↳ broadcast, async (targets delivered independently)", "", "");
            } else {
                println!("{:<8} {:<36} ↳ broadcast, delivery={:?}", "", "", register.delivery);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_requires_exactly_one_criterion() {
        let none = Select { id: None, endpoint: None, all: false };
        assert!(none.selector().is_err(), "bare replay/purge must be refused");
        assert!(matches!(Select { id: Some(3), endpoint: None, all: false }.selector(), Ok(Selector::Id(3))));
        assert!(matches!(Select { id: None, endpoint: Some("/x".into()), all: false }.selector(), Ok(Selector::Endpoint(_))));
        assert!(matches!(Select { id: None, endpoint: None, all: true }.selector(), Ok(Selector::All)));
    }

    #[test]
    fn ages_are_human_readable() {
        assert_eq!(human_age(5_000), "5s");
        assert_eq!(human_age(120_000), "2m");
        assert_eq!(human_age(7_200_000), "2h");
        assert_eq!(human_age(3 * 86_400_000), "3d");
        assert_eq!(human_age(-5), "0s");
    }
}

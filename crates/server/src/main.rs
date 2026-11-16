use anyhow::Context;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Personal agent hub: memory, calendar and tools over MCP"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the hub.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Write a minimal native configuration and its secrets, then exit.
    Init {
        /// Where to write the configuration; the secrets go next to it.
        #[arg(long, default_value = "fronde.toml")]
        config: PathBuf,
        /// Replace existing files (this makes a new token and password).
        #[arg(long)]
        force: bool,
    },
    /// Validate the configuration, its secrets and the data directory, then exit.
    Check {
        #[arg(long)]
        config: PathBuf,
    },
    /// Exit successfully when the server configured in `config` answers on /health.
    Healthcheck {
        #[arg(long)]
        config: PathBuf,
    },
}

/// A warning, not an error: the agent only starts at the first chat message.
fn warn_if_agent_missing(config: &fronde_hub::config::Config) {
    if config.web.is_none() {
        return;
    }
    let Some(program) = config.agent.command.first() else {
        return;
    };
    let env: Vec<(String, String)> = config.agent.env.clone().into_iter().collect();
    if !fronde_agent::acp::installed(program, &env) {
        eprintln!(
            "warning: {program} was not found on PATH; run `mise run agent` in the Fronde checkout \
             and start Fronde with `mise exec -- fronde serve …`, or set [agent] command to the \
             adapter's full path"
        );
    }
}

fn check(path: &std::path::Path) -> anyhow::Result<()> {
    let config = fronde_hub::config::load(path)?;
    fronde_hub::private::prepare_data_dir(&config.data.dir)?;
    warn_if_agent_missing(&config);
    fronde::check_token(&config.server.token.resolve()?)?;
    if let Some(web) = &config.web {
        fronde::check_password(&web.password.resolve().context("web.password")?)?;
    }
    if let Some(c) = &config.calendar {
        c.password.resolve()?;
    }
    for m in &config.mcp {
        if let Some(t) = &m.token {
            t.resolve()?;
        }
    }
    Ok(())
}

/// The adapter `mise run agent` installed in the current directory, as an absolute path.
fn local_adapter() -> Option<PathBuf> {
    let path = std::env::current_dir()
        .ok()?
        .join("agent/node_modules/.bin/claude-agent-acp");
    path.is_file().then_some(path)
}

fn init(config: &std::path::Path, force: bool) -> anyhow::Result<()> {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "me".to_string());
    let timezone = fronde::init::system_timezone();
    let adapter = local_adapter();
    let created = fronde::init::init(&fronde::init::Init {
        config,
        force,
        user: &user,
        timezone: &timezone,
        adapter: adapter.as_deref(),
    })?;
    let loaded = fronde_hub::config::load(&created.config)?;
    println!("Wrote {}", created.config.display());
    println!("Wrote {}", created.token_file.display());
    println!("Wrote {}", created.password_file.display());
    println!("Data will be kept in {}", loaded.data.dir.display());
    println!();
    println!(
        "Chat password (shown once, also in the password file): {}",
        created.password
    );
    println!();
    println!("Next:");
    if adapter.is_none() {
        println!("  mise run agent   (installs the Claude adapter in agent/node_modules)");
    }
    println!("  fronde serve --config {}", created.config.display());
    println!("  then open http://{}", loaded.server.listen);
    Ok(())
}

async fn healthcheck(path: &std::path::Path) -> anyhow::Result<()> {
    let config = fronde_hub::config::load(path)?;
    fronde::healthcheck(config.server.listen).await
}

async fn serve(path: &std::path::Path) -> anyhow::Result<()> {
    let config = fronde_hub::config::load(path)?;
    let (addr, server) = fronde::start(config).await?;
    tracing::info!("listening on http://{addr} (MCP at /mcp)");
    tokio::select! {
        _ = server => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("stopping"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Init { config, force } => init(config, *force),
        Command::Check { config } => check(config).map(|()| println!("config OK")),
        Command::Serve { config } => serve(config).await,
        Command::Healthcheck { config } => healthcheck(config).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

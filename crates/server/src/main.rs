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
    /// Validate the configuration and its secrets, then exit.
    Check {
        #[arg(long)]
        config: PathBuf,
    },
}

fn check(path: &std::path::Path) -> anyhow::Result<()> {
    let config = jarvis_hub::config::load(path)?;
    jarvis::check_token(&config.server.token.resolve()?)?;
    if let Some(web) = &config.web {
        jarvis::check_password(&web.password.resolve().context("web.password")?)?;
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

async fn serve(path: &std::path::Path) -> anyhow::Result<()> {
    let config = jarvis_hub::config::load(path)?;
    let (addr, server) = jarvis::start(config).await?;
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
        Command::Check { config } => check(config).map(|()| println!("config OK")),
        Command::Serve { config } => serve(config).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

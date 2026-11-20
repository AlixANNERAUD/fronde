use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

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
    /// Manage the systemd user service.
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    /// Store, list and remove the secrets kept encrypted in the database; rotate or move the
    /// master key that seals them.
    Secret {
        #[command(subcommand)]
        action: SecretCommand,
    },
    /// Change the threads while Fronde is stopped.
    Threads {
        #[command(subcommand)]
        action: ThreadsCommand,
    },
    /// Internal: how Fronde starts its agent. Applies the policy to itself, then runs the command.
    #[command(name = "__confine", hide = true)]
    Confine {
        /// The policy, as JSON: names and paths only, never a value.
        #[arg(long)]
        policy: String,
        /// The agent's program and arguments, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<std::ffi::OsString>,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Store a secret for one origin, reading its value from standard input (never a terminal).
    Set {
        /// The secret's name: letters, digits, '-', '_' or '.'.
        name: String,
        /// The only origin the secret is for, such as https://cloud.example.
        #[arg(long, required_unless_present = "unexpected")]
        origin: Option<String>,
        #[arg(long, required_unless_present = "unexpected")]
        config: Option<PathBuf>,
        /// Anything else, refused without being shown: clap's own error for an unexpected
        /// argument would quote it, and that argument may well be the secret. Once it takes a
        /// value it takes the rest, options included, so nothing is required then.
        #[arg(hide = true, num_args = 0.., allow_hyphen_values = true)]
        unexpected: Vec<String>,
    },
    /// List the stored secrets' names and origins, never their values.
    List {
        #[arg(long)]
        config: PathBuf,
    },
    /// Remove a stored secret.
    Remove {
        name: String,
        #[arg(long)]
        config: PathBuf,
    },
    /// Replace the master key with a new one and seal every stored secret again with it.
    /// Fronde must be stopped.
    RotateKey {
        #[arg(long)]
        config: PathBuf,
    },
    /// Keep the master key in another backend (file or keychain): the same key, removed from
    /// the old backend once the new one holds it. Fronde must be stopped.
    MoveKey {
        /// The backend to keep the key in: file, keychain or systemd (which only explains how).
        #[arg(long)]
        to: String,
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum ThreadsCommand {
    /// Give every conversation of one person, with its sub-agents, to another configured user.
    /// Fronde must be stopped.
    Reassign {
        /// Whose conversations: a name, or removed:<name> for someone no longer configured.
        #[arg(long)]
        from: String,
        /// The configured user who gets them.
        #[arg(long)]
        to: String,
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Install and start Fronde as a systemd user service.
    Install {
        /// Path to the configuration file.
        #[arg(long, default_value = "fronde.toml")]
        config: PathBuf,
        /// Write the unit file and reload systemd, without enabling or starting it.
        #[arg(long)]
        no_start: bool,
        /// Replace an existing unit file if its content differs.
        #[arg(long)]
        force: bool,
    },
    /// Stop, disable and remove the systemd user service.
    Uninstall,
    /// Show the status of the systemd user service.
    Status,
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

async fn check(path: &std::path::Path) -> anyhow::Result<()> {
    fronde::refuse_this_testing_build()?;
    let config = fronde_hub::config::load(path)?;
    fronde_hub::private::prepare_data_dir(&config.data.dir)?;
    warn_if_agent_missing(&config);
    fronde::check::check(&config, &mut std::io::stdout()).await
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
    // The image puts the adapter on PATH: only a checkout without it needs installing.
    if adapter.is_none() && !fronde_agent::acp::installed("claude-agent-acp", &[]) {
        println!("  mise run agent   (installs the Claude adapter in agent/node_modules)");
    }
    println!("  fronde serve --config {}", created.config.display());
    println!("  then open http://{}", loaded.server.listen);
    Ok(())
}

/// The exit status of a command line that cannot be used, as clap gives it.
const USAGE: u8 = 2;

async fn secret(action: &SecretCommand) -> anyhow::Result<ExitCode> {
    use std::io::IsTerminal as _;
    // Before any secret or the master key is read.
    fronde::hardening::harden()?;
    let mut out = std::io::stdout();
    match action {
        SecretCommand::Set { unexpected, .. } if !unexpected.is_empty() => {
            eprintln!(
                "error: fronde secret set takes the name, --origin and --config only, and the \
                 value on standard input; it got {} more argument(s), not shown here in case one \
                 is the secret. If it was, it may be in your shell history and was visible in \
                 the process list: change it where it comes from",
                unexpected.len()
            );
            return Ok(ExitCode::from(USAGE));
        }
        SecretCommand::Set {
            name,
            origin: Some(origin),
            config: Some(config),
            ..
        } => {
            let config = fronde_hub::config::load(config)?;
            let stdin = std::io::stdin();
            let terminal = stdin.is_terminal();
            let value = fronde::secret::read_value(stdin.lock(), terminal)?;
            fronde::secret::set(&config, name, origin, &value, &mut out).await?;
        }
        SecretCommand::Set { .. } => unreachable!("clap requires --origin and --config"),
        SecretCommand::List { config } => {
            fronde::secret::list(&fronde_hub::config::load(config)?, &mut out).await?;
        }
        SecretCommand::Remove { name, config } => {
            fronde::secret::remove(&fronde_hub::config::load(config)?, name, &mut out).await?;
        }
        SecretCommand::RotateKey { config } => {
            fronde::secret::rotate_key(&fronde_hub::config::load(config)?, &mut out).await?;
        }
        SecretCommand::MoveKey { to, config } => {
            fronde::secret::move_key(&fronde_hub::config::load(config)?, to, &mut out).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn healthcheck(path: &std::path::Path) -> anyhow::Result<()> {
    let config = fronde_hub::config::load(path)?;
    fronde::healthcheck(config.server.listen).await
}

async fn serve(path: &std::path::Path) -> anyhow::Result<()> {
    fronde::refuse_this_testing_build()?;
    fronde::hardening::harden()?;
    let config = fronde_hub::config::load(path)?;
    let (addr, server) = fronde::start(config).await?;
    tracing::info!("listening on http://{addr} (MCP at /mcp)");
    tokio::select! {
        _ = server => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("stopping"),
    }
    Ok(())
}

/// How long the runtime waits, once the command is done, for blocking threads still running. A
/// keychain call stuck behind an unlock prompt cannot be cancelled: Fronde exits without it.
const SHUTDOWN: std::time::Duration = std::time::Duration::from_secs(3);

/// The exit status of a helper that could not start the agent, as a shell reports a command it
/// cannot run.
const CANNOT_RUN: u8 = 127;

/// `fronde __confine`: never returns when the agent starts, since the agent replaces it.
fn confine(policy: &str, command: &[std::ffi::OsString]) -> ExitCode {
    let started = fronde_agent::confine::Policy::from_arg(policy).and_then(|policy| {
        let (program, args) = command.split_first().expect("clap requires the command");
        fronde_agent::confine::run_helper(&policy, program, args)
    });
    let Err(error) = started;
    eprintln!("fronde __confine: {error:#}");
    ExitCode::from(CANNOT_RUN)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The helper runs single-threaded, before any logging, configuration or runtime.
    if let Command::Confine { policy, command } = &cli.command {
        return confine(policy, command);
    }
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(fronde::log_filter()),
        )
        .init();
    // Built by hand rather than with `#[tokio::main]`, whose runtime waits on drop for every
    // blocking thread, however long.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = match runtime.block_on(run(&cli.command)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    };
    runtime.shutdown_timeout(SHUTDOWN);
    code
}

async fn run(command: &Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Init { config, force } => init(config, *force).map(|()| ExitCode::SUCCESS),
        Command::Check { config } => check(config).await.map(|()| {
            println!("config OK");
            ExitCode::SUCCESS
        }),
        Command::Serve { config } => serve(config).await.map(|()| ExitCode::SUCCESS),
        Command::Healthcheck { config } => healthcheck(config).await.map(|()| ExitCode::SUCCESS),
        Command::Service { action } => match action {
            ServiceCommand::Install {
                config,
                no_start,
                force,
            } => fronde::service::install(
                config,
                fronde::service::InstallOptions {
                    no_start: *no_start,
                    force: *force,
                },
            )
            .map(|()| ExitCode::SUCCESS),
            ServiceCommand::Uninstall => fronde::service::uninstall().map(|()| ExitCode::SUCCESS),
            ServiceCommand::Status => fronde::service::status(),
        },
        Command::Secret { action } => secret(action).await,
        Command::Threads {
            action: ThreadsCommand::Reassign { from, to, config },
        } => {
            let config = fronde_hub::config::load(config)?;
            fronde::threads_cmd::reassign(&config, from, to, &mut std::io::stdout()).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Confine { .. } => unreachable!("handled before the runtime starts"),
    }
}

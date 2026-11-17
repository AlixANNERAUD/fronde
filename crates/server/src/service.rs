//! `fronde service`: install, uninstall and inspect Fronde as a systemd user service.

use anyhow::{Context, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const UNIT_NAME: &str = "fronde.service";
const NO_SYSTEMD_MSG: &str = "systemd user services are not available here; run `fronde serve` under your own supervisor (Docker, a process manager)";

#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    pub no_start: bool,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status_code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait Systemctl {
    /// Verify that `systemctl` is installed and a systemd user instance is running.
    fn check_available(&self) -> anyhow::Result<()>;

    /// Run `systemctl` with `args`, capturing its output.
    fn run(&self, args: &[&str]) -> anyhow::Result<CommandOutput>;

    /// Run `systemctl` with `args`, inheriting standard streams, returning its exit code.
    fn passthrough(&self, args: &[&str]) -> anyhow::Result<i32>;
}

pub struct RealSystemctl;

impl Systemctl for RealSystemctl {
    fn check_available(&self) -> anyhow::Result<()> {
        let output = std::process::Command::new("systemctl")
            .args(["--user", "show-environment"])
            .output();
        match output {
            Ok(out) if out.status.success() => Ok(()),
            Ok(_) => bail!("{NO_SYSTEMD_MSG}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{NO_SYSTEMD_MSG}"),
            Err(e) => Err(e).context("cannot run systemctl"),
        }
    }

    fn run(&self, args: &[&str]) -> anyhow::Result<CommandOutput> {
        let output = std::process::Command::new("systemctl")
            .args(args)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!("{NO_SYSTEMD_MSG}")
                } else {
                    anyhow::Error::new(e)
                        .context(format!("cannot run systemctl {}", args.join(" ")))
                }
            })?;
        Ok(CommandOutput {
            status_code: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn passthrough(&self, args: &[&str]) -> anyhow::Result<i32> {
        let status = std::process::Command::new("systemctl")
            .args(args)
            .status()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!("{NO_SYSTEMD_MSG}")
                } else {
                    anyhow::Error::new(e)
                        .context(format!("cannot run systemctl {}", args.join(" ")))
                }
            })?;
        Ok(status.code().unwrap_or(1))
    }
}

/// The XDG rule: an absolute `XDG_CONFIG_HOME` wins, then `HOME/.config` (an absolute home only).
pub fn xdg_config_dir(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    match xdg_config_home {
        Some(p) if p.is_absolute() => Some(p),
        _ => home.filter(|h| h.is_absolute()).map(|h| h.join(".config")),
    }
}

/// Path to the user systemd unit file: `$XDG_CONFIG_HOME/systemd/user/fronde.service`, falling
/// back to `~/.config/systemd/user/fronde.service`.
pub fn unit_path() -> anyhow::Result<PathBuf> {
    let config_dir = xdg_config_dir(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
    .context("cannot work out the systemd user config directory; set XDG_CONFIG_HOME or HOME")?;
    Ok(config_dir.join("systemd/user").join(UNIT_NAME))
}

/// Quote and escape a single argument for systemd's `ExecStart=`.
///
/// Specifiers (`%`) are always doubled (`%%`). Arguments that are empty or contain whitespace,
/// quotes, or backslashes are wrapped in double quotes with `\` and `"` escaped.
fn quote_systemd_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty()
        || arg
            .chars()
            .any(|c| c.is_ascii_whitespace() || matches!(c, '"' | '\'' | '\\'));
    let mut out = String::with_capacity(arg.len() + if needs_quotes { 2 } else { 0 });
    if needs_quotes {
        out.push('"');
    }
    for c in arg.chars() {
        match c {
            '%' => out.push_str("%%"),
            // systemd expands $NAME and ${NAME} in ExecStart.
            '$' => out.push_str("$$"),
            '\\' if needs_quotes => out.push_str("\\\\"),
            '"' if needs_quotes => out.push_str("\\\""),
            '\n' if needs_quotes => out.push_str("\\n"),
            '\r' if needs_quotes => out.push_str("\\r"),
            '\t' if needs_quotes => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    if needs_quotes {
        out.push('"');
    }
    out
}

/// Render the `fronde.service` systemd user unit for `binary` and `config`.
pub fn render_unit(binary: &Path, config: &Path) -> anyhow::Result<String> {
    let binary = binary
        .to_str()
        .with_context(|| format!("binary path {} is not valid UTF-8", binary.display()))?;
    let config = config
        .to_str()
        .with_context(|| format!("config path {} is not valid UTF-8", config.display()))?;
    let binary = quote_systemd_arg(binary);
    let config = quote_systemd_arg(config);
    Ok(format!(
        "\
[Unit]
Description=Fronde agent hub
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=simple
WorkingDirectory=%h
ExecStart={binary} serve --config {config}
KillMode=mixed
Restart=always
RestartSec=5
CPUWeight=1000
IOWeight=1000
MemoryLow=512M
OOMScoreAdjust=-100
ManagedOOMPreference=avoid
OOMPolicy=continue

[Install]
WantedBy=default.target
"
    ))
}

fn resolve_config(config: &Path) -> anyhow::Result<PathBuf> {
    if !config.exists() {
        let abs = std::path::absolute(config).unwrap_or_else(|_| config.to_path_buf());
        bail!(
            "configuration file {} does not exist; run `fronde init --config {}` first",
            abs.display(),
            config.display()
        );
    }
    if !config.is_file() {
        bail!("configuration path {} is not a file", config.display());
    }
    config
        .canonicalize()
        .with_context(|| format!("cannot resolve configuration path {}", config.display()))
}

fn run_checked(systemctl: &dyn Systemctl, args: &[&str]) -> anyhow::Result<CommandOutput> {
    let out = systemctl.run(args)?;
    if out.status_code != 0 {
        let detail = out.stderr.trim();
        if detail.is_empty() {
            bail!(
                "systemctl {} exited with status {}",
                args.join(" "),
                out.status_code
            );
        } else {
            bail!("systemctl {} failed: {detail}", args.join(" "));
        }
    }
    Ok(out)
}

fn is_not_loaded_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("not loaded")
        || lower.contains("does not exist")
        || lower.contains("could not be found")
        || lower.contains("not found")
        || lower.contains("no such file or directory")
}

/// Install Fronde as a systemd user service.
pub fn install(config: &Path, opts: InstallOptions) -> anyhow::Result<()> {
    let binary = std::env::current_exe()
        .context("cannot locate the fronde executable")?
        .canonicalize()
        .context("cannot canonicalise the fronde executable path")?;
    let unit = unit_path()?;
    let mut stdout = std::io::stdout();
    install_at(&unit, &binary, config, opts, &RealSystemctl, &mut stdout)
}

pub fn install_at(
    unit_path: &Path,
    binary: &Path,
    config: &Path,
    opts: InstallOptions,
    systemctl: &dyn Systemctl,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let config = resolve_config(config)?;
    systemctl.check_available()?;
    let rendered = render_unit(binary, &config)?;

    match std::fs::read_to_string(unit_path) {
        Ok(existing) if existing == rendered => {
            writeln!(
                out,
                "Unit file {} is already up to date",
                unit_path.display()
            )?;
        }
        Ok(_) if !opts.force => {
            bail!(
                "{} already exists with different content; pass --force to replace it",
                unit_path.display()
            );
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound && !opts.force => {
            return Err(e).with_context(|| format!("cannot read {}", unit_path.display()));
        }
        Ok(_) | Err(_) => {
            if let Some(parent) = unit_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            fronde_hub::private::write_atomic_mode(unit_path, rendered.as_bytes(), 0o644)
                .with_context(|| format!("cannot write {}", unit_path.display()))?;
            writeln!(out, "Wrote {}", unit_path.display())?;
        }
    }

    run_checked(systemctl, &["--user", "daemon-reload"])?;
    writeln!(out, "Reloaded systemd user daemon")?;

    if opts.no_start {
        writeln!(
            out,
            "Service not started (--no-start). To enable and start it later:\n  systemctl --user enable --now {UNIT_NAME}"
        )?;
    } else {
        run_checked(systemctl, &["--user", "enable", "--now", UNIT_NAME])?;
        writeln!(out, "Enabled and started {UNIT_NAME}")?;
    }

    writeln!(out)?;
    writeln!(
        out,
        "To start the service at boot without an active login session, run:"
    )?;
    writeln!(out, "  loginctl enable-linger $USER")?;
    Ok(())
}

/// Stop, disable and remove the Fronde systemd user service.
pub fn uninstall() -> anyhow::Result<()> {
    let unit = unit_path()?;
    let mut stdout = std::io::stdout();
    uninstall_at(&unit, &RealSystemctl, &mut stdout)
}

pub fn uninstall_at(
    unit_path: &Path,
    systemctl: &dyn Systemctl,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    systemctl.check_available()?;

    let disable = systemctl.run(&["--user", "disable", "--now", UNIT_NAME])?;
    if disable.status_code == 0 {
        writeln!(out, "Disabled and stopped {UNIT_NAME}")?;
    } else if is_not_loaded_error(&disable.stderr) {
        writeln!(out, "{UNIT_NAME} was not loaded")?;
    } else {
        let detail = disable.stderr.trim();
        if detail.is_empty() {
            bail!(
                "systemctl --user disable --now {UNIT_NAME} exited with status {}",
                disable.status_code
            );
        } else {
            bail!("systemctl --user disable --now {UNIT_NAME} failed: {detail}");
        }
    }

    match std::fs::remove_file(unit_path) {
        Ok(()) => writeln!(out, "Removed {}", unit_path.display())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            writeln!(out, "No unit file at {}", unit_path.display())?;
        }
        Err(e) => return Err(e).with_context(|| format!("cannot remove {}", unit_path.display())),
    }

    run_checked(systemctl, &["--user", "daemon-reload"])?;
    writeln!(out, "Reloaded systemd user daemon")?;
    writeln!(out, "Configuration and data files were kept.")?;
    Ok(())
}

/// Show `systemctl --user status fronde.service --no-pager` and pass its exit code through.
pub fn status() -> anyhow::Result<ExitCode> {
    status_with(&RealSystemctl)
}

pub fn status_with(systemctl: &dyn Systemctl) -> anyhow::Result<ExitCode> {
    systemctl.check_available()?;
    let code = systemctl.passthrough(&["--user", "status", UNIT_NAME, "--no-pager"])?;
    let exit = u8::try_from(code).unwrap_or(1);
    Ok(ExitCode::from(exit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn cache_tempdir() -> tempfile::TempDir {
        let base =
            PathBuf::from(std::env::var_os("HOME").expect("HOME must be set")).join(".cache/ft");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("fronde-service-test-")
            .tempdir_in(&base)
            .unwrap()
    }

    struct FakeSystemctl {
        available: bool,
        calls: RefCell<Vec<Vec<String>>>,
        responses: RefCell<VecDeque<CommandOutput>>,
        passthrough_code: i32,
    }

    impl FakeSystemctl {
        fn new() -> Self {
            Self {
                available: true,
                calls: RefCell::new(Vec::new()),
                responses: RefCell::new(VecDeque::new()),
                passthrough_code: 0,
            }
        }

        fn unavailable() -> Self {
            Self {
                available: false,
                ..Self::new()
            }
        }

        fn with_responses(responses: Vec<CommandOutput>) -> Self {
            Self {
                responses: RefCell::new(responses.into()),
                ..Self::new()
            }
        }

        fn with_passthrough_code(code: i32) -> Self {
            Self {
                passthrough_code: code,
                ..Self::new()
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    impl Systemctl for FakeSystemctl {
        fn check_available(&self) -> anyhow::Result<()> {
            if self.available {
                Ok(())
            } else {
                bail!("{NO_SYSTEMD_MSG}");
            }
        }

        fn run(&self, args: &[&str]) -> anyhow::Result<CommandOutput> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| (*s).to_string()).collect());
            Ok(self
                .responses
                .borrow_mut()
                .pop_front()
                .unwrap_or(CommandOutput {
                    status_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }))
        }

        fn passthrough(&self, args: &[&str]) -> anyhow::Result<i32> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| (*s).to_string()).collect());
            Ok(self.passthrough_code)
        }
    }

    #[test]
    fn renders_expected_unit_for_plain_paths() {
        let unit = render_unit(
            Path::new("/usr/local/bin/fronde"),
            Path::new("/home/alix/.config/fronde/fronde.toml"),
        )
        .unwrap();
        assert_eq!(
            unit,
            "\
[Unit]
Description=Fronde agent hub
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=simple
WorkingDirectory=%h
ExecStart=/usr/local/bin/fronde serve --config /home/alix/.config/fronde/fronde.toml
KillMode=mixed
Restart=always
RestartSec=5
CPUWeight=1000
IOWeight=1000
MemoryLow=512M
OOMScoreAdjust=-100
ManagedOOMPreference=avoid
OOMPolicy=continue

[Install]
WantedBy=default.target
"
        );
    }

    #[test]
    fn renders_unit_quoting_spaces_and_escaping_percent() {
        let unit = render_unit(
            Path::new("/opt/my bin/100%/fronde"),
            Path::new("/home/alix/fronde 50%/fronde.toml"),
        )
        .unwrap();
        assert!(
            unit.contains(
                "ExecStart=\"/opt/my bin/100%%/fronde\" serve --config \"/home/alix/fronde 50%%/fronde.toml\""
            ),
            "{unit}"
        );
        // systemd would expand $HOME: a literal dollar is doubled.
        let unit_dollar = render_unit(
            Path::new("/opt/$HOME/fronde"),
            Path::new("/home/alix/fronde.toml"),
        )
        .unwrap();
        assert!(
            unit_dollar.contains("ExecStart=/opt/$$HOME/fronde serve"),
            "{unit_dollar}"
        );
        // Unquoted path containing % still escapes %.
        let unit_percent_only = render_unit(
            Path::new("/opt/100%/fronde"),
            Path::new("/home/alix/fronde.toml"),
        )
        .unwrap();
        assert!(
            unit_percent_only
                .contains("ExecStart=/opt/100%%/fronde serve --config /home/alix/fronde.toml"),
            "{unit_percent_only}"
        );
    }

    #[test]
    fn xdg_config_dir_prefers_absolute_xdg_then_home_dot_config() {
        assert_eq!(
            xdg_config_dir(
                Some(PathBuf::from("/xdg/cfg")),
                Some(PathBuf::from("/home/alix")),
            ),
            Some(PathBuf::from("/xdg/cfg"))
        );
        assert_eq!(
            xdg_config_dir(
                Some(PathBuf::from("relative/cfg")),
                Some(PathBuf::from("/home/alix")),
            ),
            Some(PathBuf::from("/home/alix/.config"))
        );
        assert_eq!(
            xdg_config_dir(None, Some(PathBuf::from("/home/alix"))),
            Some(PathBuf::from("/home/alix/.config"))
        );
        assert_eq!(xdg_config_dir(None, Some(PathBuf::from("rel"))), None);
        assert_eq!(xdg_config_dir(None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn install_writes_unit_0644_reloads_and_enables() {
        use std::os::unix::fs::PermissionsExt;

        let dir = cache_tempdir();
        let xdg = dir.path().join("xdg");
        let cfg = dir.path().join("fronde.toml");
        std::fs::write(&cfg, "").unwrap();
        let unit = xdg.join("systemd/user/fronde.service");
        let bin = dir.path().join("fronde");
        let systemctl = FakeSystemctl::new();
        let mut out = Vec::new();

        install_at(
            &unit,
            &bin,
            &cfg,
            InstallOptions::default(),
            &systemctl,
            &mut out,
        )
        .unwrap();

        let mode = std::fs::metadata(&unit).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        let content = std::fs::read_to_string(&unit).unwrap();
        assert_eq!(
            content,
            render_unit(&bin, &cfg.canonicalize().unwrap()).unwrap()
        );
        assert_eq!(
            systemctl.calls(),
            vec![
                vec!["--user", "daemon-reload"],
                vec!["--user", "enable", "--now", "fronde.service"],
            ]
        );
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("loginctl enable-linger $USER"), "{stdout}");
        assert!(
            stdout.contains("Enabled and started fronde.service"),
            "{stdout}"
        );
    }

    #[test]
    fn install_no_start_reloads_without_enabling() {
        let dir = cache_tempdir();
        let cfg = dir.path().join("fronde.toml");
        std::fs::write(&cfg, "").unwrap();
        let unit = dir.path().join("xdg/systemd/user/fronde.service");
        let bin = dir.path().join("fronde");
        let systemctl = FakeSystemctl::new();
        let mut out = Vec::new();

        install_at(
            &unit,
            &bin,
            &cfg,
            InstallOptions {
                no_start: true,
                force: false,
            },
            &systemctl,
            &mut out,
        )
        .unwrap();

        assert!(unit.is_file());
        assert_eq!(systemctl.calls(), vec![vec!["--user", "daemon-reload"]]);
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("--no-start"), "{stdout}");
        assert!(stdout.contains("loginctl enable-linger $USER"), "{stdout}");
    }

    #[test]
    fn install_handles_existing_unit_identical_or_different() {
        let dir = cache_tempdir();
        let cfg = dir.path().join("fronde.toml");
        std::fs::write(&cfg, "").unwrap();
        let unit = dir.path().join("xdg/systemd/user/fronde.service");
        let bin = dir.path().join("fronde");

        // First install.
        let s1 = FakeSystemctl::new();
        install_at(
            &unit,
            &bin,
            &cfg,
            InstallOptions::default(),
            &s1,
            &mut Vec::new(),
        )
        .unwrap();

        // Identical content without --force succeeds and still ensures enabled.
        let s2 = FakeSystemctl::new();
        install_at(
            &unit,
            &bin,
            &cfg,
            InstallOptions::default(),
            &s2,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            s2.calls(),
            vec![
                vec!["--user", "daemon-reload"],
                vec!["--user", "enable", "--now", "fronde.service"],
            ]
        );

        // Different content without --force refuses.
        std::fs::write(&unit, "[Unit]\nDescription=old\n").unwrap();
        let s3 = FakeSystemctl::new();
        let err = install_at(
            &unit,
            &bin,
            &cfg,
            InstallOptions::default(),
            &s3,
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--force"), "{err}");
        assert!(s3.calls().is_empty());

        // Different content with --force replaces it.
        let s4 = FakeSystemctl::new();
        install_at(
            &unit,
            &bin,
            &cfg,
            InstallOptions {
                no_start: false,
                force: true,
            },
            &s4,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&unit).unwrap(),
            render_unit(&bin, &cfg.canonicalize().unwrap()).unwrap()
        );
    }

    #[test]
    fn install_refuses_missing_config_and_suggests_init() {
        let dir = cache_tempdir();
        let cfg = dir.path().join("missing.toml");
        let unit = dir.path().join("xdg/systemd/user/fronde.service");
        let systemctl = FakeSystemctl::new();
        let err = install_at(
            &unit,
            &dir.path().join("fronde"),
            &cfg,
            InstallOptions::default(),
            &systemctl,
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("does not exist") && err.contains("fronde init"),
            "{err}"
        );
        assert!(!unit.exists());
        assert!(systemctl.calls().is_empty());
    }

    #[test]
    fn unavailable_systemd_prints_clear_fallback_message() {
        let dir = cache_tempdir();
        let cfg = dir.path().join("fronde.toml");
        std::fs::write(&cfg, "").unwrap();
        let unit = dir.path().join("xdg/systemd/user/fronde.service");
        let systemctl = FakeSystemctl::unavailable();

        for err in [
            install_at(
                &unit,
                &dir.path().join("fronde"),
                &cfg,
                InstallOptions::default(),
                &systemctl,
                &mut Vec::new(),
            )
            .unwrap_err(),
            uninstall_at(&unit, &systemctl, &mut Vec::new()).unwrap_err(),
            status_with(&systemctl).unwrap_err(),
        ] {
            assert_eq!(err.to_string(), NO_SYSTEMD_MSG);
        }
        assert!(!unit.exists());
    }

    #[test]
    fn uninstall_disables_removes_unit_reloads_and_keeps_data() {
        let dir = cache_tempdir();
        let unit = dir.path().join("xdg/systemd/user/fronde.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "[Unit]\n").unwrap();

        let systemctl = FakeSystemctl::new();
        let mut out = Vec::new();
        uninstall_at(&unit, &systemctl, &mut out).unwrap();

        assert!(!unit.exists());
        assert_eq!(
            systemctl.calls(),
            vec![
                vec!["--user", "disable", "--now", "fronde.service"],
                vec!["--user", "daemon-reload"],
            ]
        );
        let stdout = String::from_utf8(out).unwrap();
        assert!(
            stdout.contains("Configuration and data files were kept"),
            "{stdout}"
        );
    }

    #[test]
    fn uninstall_ignores_not_loaded_and_missing_unit_file() {
        let dir = cache_tempdir();
        let unit = dir.path().join("xdg/systemd/user/fronde.service");
        let systemctl = FakeSystemctl::with_responses(vec![
            CommandOutput {
                status_code: 1,
                stdout: String::new(),
                stderr: "Unit fronde.service not loaded.\n".to_string(),
            },
            CommandOutput {
                status_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);
        let mut out = Vec::new();
        uninstall_at(&unit, &systemctl, &mut out).unwrap();
        assert_eq!(
            systemctl.calls(),
            vec![
                vec!["--user", "disable", "--now", "fronde.service"],
                vec!["--user", "daemon-reload"],
            ]
        );
    }

    #[test]
    fn status_runs_systemctl_and_passes_exit_code_through() {
        for expected in [0u8, 3, 4] {
            let systemctl = FakeSystemctl::with_passthrough_code(i32::from(expected));
            let code = status_with(&systemctl).unwrap();
            assert_eq!(code, ExitCode::from(expected));
            assert_eq!(
                systemctl.calls(),
                vec![vec!["--user", "status", "fronde.service", "--no-pager"]]
            );
        }
    }
}

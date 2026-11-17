# Run as a systemd service

`fronde service` installs Fronde as a systemd user service (`fronde.service`), with no root access needed.

## 1. Prepare the configuration

Write a configuration first if you do not have one yet, and check it:

```sh
fronde init
fronde check --config fronde.toml
```

When run in a checkout where `mise run agent` installed the Claude adapter, `fronde init` writes the adapter's absolute path into `[agent] command`, so the service finds it without extra `PATH` entries. If you run the web interface built in a checkout, set `FRONDE_SITE_ROOT` to its `public` directory (for example through a systemd drop-in file) or place the binary where it can find the files.

## 2. Install and start the service

```sh
fronde service install --config fronde.toml
```

`--config` defaults to `fronde.toml` in the current directory. `install` resolves both the running `fronde` binary and the configuration file to absolute paths, writes `$XDG_CONFIG_HOME/systemd/user/fronde.service` (or `~/.config/systemd/user/fronde.service`) with mode 0644, reloads the systemd user daemon, and enables and starts `fronde.service`.

Pass `--no-start` to write the unit file and reload systemd without enabling or starting the service yet.

If `fronde.service` already exists with identical content, `install` keeps it and makes sure it is enabled. If it exists with different content, `install` refuses unless `--force` is given.

## 3. Keep running without a login session

By default, systemd user services stop when your last login session closes and do not start at boot until you log in. To start `fronde.service` at boot and keep it running across logouts, enable lingering once:

```sh
loginctl enable-linger $USER
```

## 4. Check status and logs

```sh
fronde service status
journalctl --user -u fronde
```

Add `-f` to `journalctl --user -u fronde` to follow new log lines as they arrive. `fronde service status` runs `systemctl --user status fronde.service --no-pager` and exits with its status code.

## 5. Upgrade

After replacing or rebuilding the `fronde` binary, re-run `install` with `--force` so the unit file is updated if the binary path changed:

```sh
fronde service install --config path/to/fronde.toml --force
```

If the binary was replaced at the same path and the unit file did not change, restart the service so it runs the new binary:

```sh
systemctl --user restart fronde.service
```

## 6. Uninstall

```sh
fronde service uninstall
```

This stops and disables `fronde.service`, removes the unit file, and reloads systemd. Your configuration, secret files and data directory are kept.

## Systems without systemd

If `systemctl` is missing or no systemd user instance is running (for example inside a container or under another init system), `fronde service` exits with an error. Run `fronde serve --config fronde.toml` under your own supervisor instead, or see [Run in Docker](run-in-docker.md).

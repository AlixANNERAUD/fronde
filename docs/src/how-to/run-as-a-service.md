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

`--config` defaults to `fronde.toml` in the current directory. `install` resolves both the running `fronde` binary and the configuration file to absolute paths, writes `$XDG_CONFIG_HOME/systemd/user/fronde.service` (or `~/.config/systemd/user/fronde.service`) with mode 0644, reloads the systemd user daemon, and enables and starts `fronde.service`. The generated unit sets `LimitCORE=0` so a crash writes no core dump containing secrets.

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

## The agent's scope

The service, like any Fronde run in a session with a systemd user manager, starts its agent in
a scope of its own, `fronde-agent-<id>.scope`, under `fronde-agents.slice` (itself under
`fronde.slice`), outside `fronde.service`. The scope caps the agent's memory, CPU time and
tasks with the limits in [`[agent.limits]`](../reference/configuration.md#agentlimits): 2 GiB, 2
CPUs and 512 tasks by default. It only caps resources: what the agent may read, write and reach
is the [sandbox](../explanation/security.md#where-the-agent-runs)'s doing, with or without
systemd. See the agent and what it uses with:

```sh
systemctl --user status fronde-agents.slice
systemd-cgtop --user fronde.slice
```

When Fronde stops the agent, it stops its scope too. If Fronde is killed (`kill -9`, a crash),
the agent's input closes, so the agent and Claude Code exit, and the scope goes with them. A
process that outlived them would keep the scope until Fronde next starts, which stops every
scope its data directory's agents left, once, before it accepts requests (starting an agent
never stops another one that runs); `systemctl --user stop 'fronde-agent-*'` stops them by
hand. A scope's name is `fronde-agent-<id>-t<thread>-<random>.scope`, and a scope whose main
process the kernel OOM-killed goes as a whole (`OOMPolicy=stop`). At each start of the agent, Fronde logs `agent limits: …` with the scope's
name, or why there is none.

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

## Keep the master key in a systemd credential

If you [store secrets](store-a-secret.md), the service needs their master key when it starts,
often at boot, before any desktop session has unlocked a keychain. A systemd credential fits:
the key is encrypted on disk with the machine's key (and its TPM when there is one), so the
`.cred` file is useless on another machine, and systemd decrypts it each time the service starts
into memory that is never swapped out. This needs systemd 256 or later, for credentials in user
services. `fronde service install` leaves the unit as it is; you add the credential with a
drop-in file.

It does not hide the key from other programs running as your user: while the service runs, they
can read the decrypted key in `$XDG_RUNTIME_DIR/credentials/fronde.service/`, and they can
decrypt the `.cred` file with `systemd-creds --user decrypt`. A key file or the keychain is no
better there; see [Security model](../explanation/security.md#stored-secrets).

Do this before storing the first secret. Make a key and encrypt it, without ever writing it in
clear:

```sh
{ printf 'fronde-key-v1:'; head -c 32 /dev/urandom | basenc --base64url | tr -d '=\n'; echo; } \
  | systemd-creds --user encrypt --name=fronde-master-key - ~/.config/fronde/master-key.cred
```

To keep a key you already have in `master.key` instead, encrypt that file:
`systemd-creds --user encrypt --name=fronde-master-key ~/.config/fronde/master.key
~/.config/fronde/master-key.cred`, and delete `master.key` once Fronde has started with the
credential.

Load it in the unit, with `systemctl --user edit fronde.service`:

```ini
[Service]
LoadCredentialEncrypted=fronde-master-key:%h/.config/fronde/master-key.cred
```

and name the backend in the configuration, so that Fronde looks for the key nowhere else:

```toml
[secrets]
backend = "systemd"
```

Then `systemctl --user restart fronde.service`. Fronde reads the key from
`$CREDENTIALS_DIRECTORY/fronde-master-key`, which only exists while the service runs. A
`fronde secret set` or `fronde check` typed in a shell has no such directory: run it under the
same credential with `systemd-run`, the value still on standard input:

```sh
systemd-run --user --pipe --wait --quiet \
  -p LoadCredentialEncrypted=fronde-master-key:$HOME/.config/fronde/master-key.cred \
  "$(command -v fronde)" secret set caldav --origin https://cloud.example \
  --config $HOME/.config/fronde/fronde.toml < caldav-password
```

`fronde secret list` and `fronde secret remove` need no key. `master-key.cred` opens only on
this machine: if the machine is lost, so is the key, and the secrets must be stored again on the
next one. To keep a way back, encrypt a key line you also keep in a password manager rather than
one made in the pipe above.

## Systems without systemd

If `systemctl` is missing or no systemd user instance is running (for example inside a container or under another init system), `fronde service` exits with an error. Run `fronde serve --config fronde.toml` under your own supervisor instead, or see [Run in Docker](run-in-docker.md).

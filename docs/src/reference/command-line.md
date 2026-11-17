# Command line

```text
fronde <command> --config <file>
```

Commands that read a configuration take it with `--config <file>` (optional with `fronde.toml` as
the default for `init` and `service install`). Logs go to standard error, filtered by `RUST_LOG`
(default `info`). A command exits with 0 on success and 1 on failure, printing `error: …` with
the cause, except `service status`, which passes `systemctl`'s exit code through.

## `fronde init`

```text
fronde init [--config <file>] [--force]
```

Writes a minimal native configuration, `fronde.toml` by default, and two secrets next to it:
`fronde_token` (32 random bytes in hex) and `fronde_password` (a generated passphrase, printed
once on the terminal). Every file is created with mode 0600. The configuration listens on
`127.0.0.1:8484`, leaves `[data] dir` out (see the [default](configuration.md#data)), takes the
time zone from the system (`UTC` when it cannot tell) and the user name from `$USER`, and refers
to the secrets by relative paths. If `agent/node_modules/.bin/claude-agent-acp` exists in the
current directory (after `mise run agent`), its absolute path becomes `[agent] command`;
otherwise `init` suggests `mise run agent`. It ends by printing the commands to run next.

It refuses to overwrite any of the three files unless `--force` is given, which makes a new token
and password.

## `fronde serve`

Starts the server: opens the memory, registers the tools, connects to upstream MCP servers in the
background, and serves HTTP on `[server] listen`. With `[web]`, it also serves the chat and runs
the agent. It stops on Ctrl+C.

On Linux, it first makes its process non-dumpable and sets `RLIMIT_CORE` to 0, so that no core
dump, ptrace or `/proc/<pid>/mem` read by another process of the same user can reach its secrets
(see [Security model](../explanation/security.md#core-dumps-and-other-processes)). Set
`FRONDE_ALLOW_CORE_DUMPS=1` to skip this when debugging a crash.

## `fronde check`

Validates the configuration and exits. It parses the file, then reads every secret: the server
token (at least 32 characters), the chat password (not empty), the upstream tokens and the
components' credentials. For each component it prints the SHA-256 of its `.wasm` file and of its
manifest, then checks it as `serve` would before loading it (approval, grants within its
manifest, settings, tool prefixes, credentials over https) and prints which approval applied:
`approved: shipped with this build (calendar)`, `approved: pinned by the hashes in its entry` or
`approved: by the owner, stored in the database`. For each credential that names a stored secret
(`{ stored = "…" }`) it then prints `source <name>: credential for <origin>: stored secret <secret>
opens, stored for this origin`, never the value; a secret that is not stored, one stored for
another origin, or one that does not open is an error saying how to fix it. It reads the
approvals and secrets stored in `fronde.db` when the data directory has one, and checks the master
key against them, as `serve` does at start (a key an earlier version left in the data directory
is read there and not moved: `serve` moves it), and never creates, updates or writes the
database: it
opens it read-only, and a database an older Fronde left without the approvals or secrets table
has no stored approval or secret, which is what `serve` would find after updating it. With
`[python]`, it checks that `python.wasm` is the pinned build and compiles it, then prints
`python: approved (CPython 3.15.2)`. It also checks that `[data] dir` can be created and written:
it creates the directory (mode 0700), writes a probe file and removes it, and otherwise names the
path and the fix (set `[data] dir` to a directory you own, such as `~/.local/share/fronde`). A
`[server] listen` that is not an address is reported by the parse. When `[web]` is set and the
agent command is not found on `PATH` (or at the path given), it prints a warning, not an error.
It prints `config OK` when all is well. It does not connect to anything.

## `fronde secret`

Stores, lists and removes the secrets kept encrypted in `fronde.db`, scope `sources` (see
[Store a secret](../how-to/store-a-secret.md) and
[Security model](../explanation/security.md#stored-secrets)). Each command makes itself
non-dumpable on Linux, moves a master key an earlier version left in the data directory to its
place, and opens the database, each as `serve` does, and works while Fronde runs.

```text
fronde secret set <name> --origin <origin> --config <file>  < value
fronde secret list --config <file>
fronde secret remove <name> --config <file>
```

- `set` reads the value from standard input, never from an argument, and refuses a terminal,
  which would echo it. One final line break (`\n` or `\r\n`) is dropped; an empty value, one
  that is not UTF-8 or one over 64 KiB is refused. The name is 1 to 64 letters, digits, `-`, `_`
  or `.`. The origin is `scheme://host[:port]` over http or https, stored as Fronde normalises it
  (`HTTPS://Cloud.Example:443` becomes `https://cloud.example`); the secret is bound to it.
  Setting a name again replaces its value and origin. The first secret creates the master key,
  `master.key` beside the configuration file unless `[secrets] master_key` names another file,
  and prints its path; while secrets are stored, a missing or different key file is an error and
  no new key is made. Any argument besides the name and the options is refused, exit status 2,
  without being shown. It prints `stored secret <name> for <origin>`.
- `list` prints one line per secret: its name, origin and last change (UTC), separated by tabs,
  or `no stored secret`. It never prints a value.
- `remove` removes one secret, and exits with 1 if there is none by that name. Removing the last
  one also removes the key check, so a lost `master.key` then no longer blocks a new one.

A component's credential names a stored secret with `{ stored = "<name>" }` (see
[Configuration](configuration.md#kind--component)).

## `fronde healthcheck`

Exits with 0 when the server configured in the file answers `GET /health` with a success within
5 seconds. A server listening on every interface (`0.0.0.0` or `::`) is reached over the loopback.
The container image uses it as its health check.

## `fronde service`

Manages Fronde as a systemd user service (`fronde.service`). See
[Run as a systemd service](../how-to/run-as-a-service.md).

```text
fronde service install [--config <file>] [--no-start] [--force]
fronde service uninstall
fronde service status
```

- `install` resolves the configuration file (default `fronde.toml`) and the running `fronde`
  binary to absolute paths, writes `$XDG_CONFIG_HOME/systemd/user/fronde.service` (falling back
  to `~/.config/systemd/user/fronde.service`) with mode 0644, runs `systemctl --user daemon-reload`,
  and runs `systemctl --user enable --now fronde.service`. With `--no-start`, it stops after
  `daemon-reload`. If the unit already exists with different content, it refuses unless `--force`
  is given; if the content is identical, it ensures the service is enabled.
- `uninstall` runs `systemctl --user disable --now fronde.service` (ignoring "not loaded" errors),
  removes the unit file, and runs `systemctl --user daemon-reload`. Configuration and data files
  are kept.
- `status` runs `systemctl --user status fronde.service --no-pager` and passes its exit code
  through.

If `systemctl` or a systemd user instance is not available, all three commands exit with 1 and
suggest running `fronde serve` under another supervisor.

## Other options

`fronde --help` lists the commands, `fronde --version` prints the version.

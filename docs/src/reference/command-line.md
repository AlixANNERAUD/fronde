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
`approved: by the owner, stored in the database`. It reads the approvals stored in `fronde.db`
when the data directory has one, as `serve` does at start, and never creates, updates or writes
the database: it opens it read-only, and a database an older Fronde left without the approvals
table has no stored approval, which is what `serve` would find after updating it. With
`[python]`, it checks that `python.wasm` is the pinned build and compiles it, then prints
`python: approved (CPython 3.15.2)`. It also checks that `[data] dir` can be created and written:
it creates the directory (mode 0700), writes a probe file and removes it, and otherwise names the
path and the fix (set `[data] dir` to a directory you own, such as `~/.local/share/fronde`). A `[server]
listen` that is not an address is reported by the parse. When `[web]` is set and the agent
command is not found on `PATH` (or at the path given), it prints a warning, not an error. It
prints `config OK` when all is well. It does not connect to anything.

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

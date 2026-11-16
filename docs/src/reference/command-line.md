# Command line

```text
fronde <command> --config <file>
```

Every command but `init` takes the path of the [configuration file](configuration.md). Logs go to standard
error, filtered by `RUST_LOG` (default `info`). A command exits with 0 on success and 1 on
failure, printing `error: …` with the cause.

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

## `fronde check`

Validates the configuration and exits. It parses the file, then reads every secret: the server
token (at least 32 characters), the chat password (not empty), the calendar password and the
upstream tokens. It also checks that `[data] dir` can be created and written: it creates the
directory (mode 0700), writes a probe file and removes it, and otherwise names the path and the
fix (set `[data] dir` to a directory you own, such as `~/.local/share/fronde`). A `[server]
listen` that is not an address is reported by the parse. When `[web]` is set and the agent
command is not found on `PATH` (or at the path given), it prints a warning, not an error. It
prints `config OK` when all is well. It does not connect to anything.

## `fronde healthcheck`

Exits with 0 when the server configured in the file answers `GET /health` with a success within
5 seconds. A server listening on every interface (`0.0.0.0` or `::`) is reached over the loopback.
The container image uses it as its health check.

## Other options

`fronde --help` lists the commands, `fronde --version` prints the version.

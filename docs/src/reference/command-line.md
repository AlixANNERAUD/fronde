# Command line

```text
fronde <command> --config <file>
```

Every command takes the path of the [configuration file](configuration.md). Logs go to standard
error, filtered by `RUST_LOG` (default `info`). A command exits with 0 on success and 1 on
failure, printing `error: …` with the cause.

## `fronde serve`

Starts the server: opens the memory, registers the tools, connects to upstream MCP servers in the
background, and serves HTTP on `[server] listen`. With `[web]`, it also serves the chat and runs
the agent. It stops on Ctrl+C.

## `fronde check`

Validates the configuration and exits. It parses the file, then reads every secret: the server
token (at least 32 characters), the chat password (not empty), the calendar password and the
upstream tokens. It prints `config OK` when all is well. It does not connect to anything.

## `fronde healthcheck`

Exits with 0 when the server configured in the file answers `GET /health` with a success within
5 seconds. A server listening on every interface (`0.0.0.0` or `::`) is reached over the loopback.
The container image uses it as its health check.

## Other options

`fronde --help` lists the commands, `fronde --version` prints the version.

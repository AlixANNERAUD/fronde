# Container image

The `Dockerfile` at the root of the repository builds the image. See
[Run in Docker](../how-to/run-in-docker.md) to use it.

## Contents

| Path | What |
| --- | --- |
| `/app/fronde` | The server. |
| `/app/site` | The built web interface (`FRONDE_SITE_ROOT`). |
| `/app/components` | Tool components with their manifests: `calendar.wasm` and `calendar.toml`. A `[[sources]]` entry enables one; `/app/fronde` approves them without hashes in the entry, being built from the same files. |
| `/opt/agent` | The Claude ACP adapter, installed from `agent/package-lock.json`. It brings its own Claude Code. |
| `/usr/local/bin/node` | Node.js 22, for the adapter. |

The image is based on Debian trixie-slim, with `ca-certificates`, `git` (to version the memory)
and `tini`.

## Settings

| Setting | Value |
| --- | --- |
| User | `65532:65532`. Mounted secrets and a mounted `~/.claude` must be readable by this uid. |
| Entry point | `tini -- /app/fronde`. tini passes `docker stop`'s signal on to Fronde and reaps the agent's processes. |
| Command | `serve --config /config/fronde.toml` |
| Volume | `/data`, owned by 65532. Use it as `[data] dir`. |
| Port | `8484` |
| `HOME` | `/data/home`, so the agent's sign-in and settings survive restarts. |
| `PATH` | Starts with `/opt/agent/node_modules/.bin`, where `claude-agent-acp` is. |
| Health check | `fronde healthcheck --config /config/fronde.toml` every 30 seconds, 6 second timeout, 20 second start period, 3 retries. |

## Mounts and variables to provide

| What | Where |
| --- | --- |
| Configuration | `/config/fronde.toml`, read-only. |
| Secrets | Any path, such as `/run/secrets/…`, named in the configuration. |
| Data | A volume on `/data`. |
| Claude sign-in | `CLAUDE_CODE_OAUTH_TOKEN`, or a Claude directory on `/data/home/.claude`. See [Sign Fronde in to Claude](../how-to/sign-in-to-claude.md). |

## Build

The builder stage installs mise at a pinned version, checked against its release checksums, then
the toolchain pinned in `mise.toml`, and runs `mise run build`, which builds the components
before the binary, so that the hashes of the shipped components it carries are those of the
files copied to `/app/components`. Cargo's registry, the Dioxus CLI's
downloads and the target directories, the components' included, are cache mounts, per
architecture. The image builds for
`amd64` and `arm64`.

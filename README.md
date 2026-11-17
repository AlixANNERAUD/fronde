<h1 align="center">Fronde</h1>

<p align="center">
  <strong>A personal assistant that runs on your own Claude subscription.</strong><br>
  A web chat with Claude, and your memory, calendar and tools served over MCP.
</p>

---

## Why “Fronde”

A *fronde* is a sling, the small one's weapon against giants, and *la Fronde* was the
17th-century French revolt against central power. Fronde runs at home as one binary and drives the
big models (Claude, Codex, Gemini) through your own subscriptions instead of locking you into their
platforms. It is pronounced the French way, /fʁɔ̃d/.

> One binary. Every agent. No masters.
>
> « La petite arme qui fait plier les géants. »

## What it is

Fronde is a small self-hosted server. It drives Claude Code through its
[ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp), signed in with your own
Claude subscription, and gives it one set of tools: a memory kept in Markdown files, your CalDAV
calendar and reminders, and the tools of other MCP servers. You talk to it from a web page on your
phone or computer. Other agents, such as Claude Code, Codex or T3 Code, reach the same tools at
`/mcp`.

## Features

- **Chat** from any browser, behind a password: answers stream in, the tools Fronde uses show as
  they run, any turn can be stopped, and the conversation survives restarts.
- **Your subscription**: Claude Code runs through its ACP adapter, with its built-in tools turned
  off except web search and fetch. Fronde otherwise only uses its own tools.
- **Memory** in plain Markdown files (`USER.md`, `MEMORY.md` and a dated journal), searchable and
  versioned with git.
- **Calendar and reminders** on any CalDAV server (Nextcloud, Radicale, Fastmail and others).
- **Python for calculations**: exact arithmetic, dates and aggregates in a sandboxed CPython for
  WASI, with the standard library only and no files or network.
- **More tools** from upstream MCP servers (MCPHub or any other), found with `search_tools`.
- **An MCP server** at `/mcp` behind a bearer token, for your other agents.
- **One binary or one container**, configured with one TOML file. Secrets live in files or
  environment variables, never in the configuration.

## Quick start

Native installs run as your own user, with no root. `fronde init` writes a minimal
`fronde.toml` and its two secrets (`fronde_token`, `fronde_password`, mode 0600) next to it. The
data goes to `$XDG_DATA_HOME/fronde` (`~/.local/share/fronde`). For Docker, copy
[`fronde.example.toml`](fronde.example.toml) (see [Docker](#docker)).

### Native, with mise

You need [mise](https://mise.jdx.dev) and Claude Code signed in to your
subscription (`claude` works in your terminal). Node.js comes from mise.

```sh
git clone https://github.com/AlixANNERAUD/fronde && cd fronde
mise install
mise run build
mise run agent
target/release/fronde init
target/release/fronde check --config fronde.toml
mise exec -- target/release/fronde serve --config fronde.toml
```

`fronde init` prints the chat password once; it is also in `fronde_password`. Open
http://127.0.0.1:8484 and sign in with it. `mise exec` puts the adapter installed by
`mise run agent` on the `PATH`; `init` also writes its path into the configuration when it runs
from the checkout. The server reads the chat's files
from `target/dx/fronde-web/release/web/public`, where `mise run build` puts them; run it from the
checkout, or set `FRONDE_SITE_ROOT`.

### Docker

The image has Fronde, Node.js and the Claude adapter. Copy
`fronde.example.toml` to `fronde.toml`, create the secrets (`openssl rand -hex 32 > fronde_token`,
and a password in `fronde_password`, both mode 0600), set
`listen = "0.0.0.0:8484"` and `dir = "/data"`, and point the secrets to `/run/secrets/…`. Fronde
runs as uid 65532, which must be able to read the secret files.

```sh
sudo chown 65532 fronde_token fronde_password
docker build -t fronde .
docker run -d --name fronde -p 8484:8484 \
  -v ./fronde.toml:/config/fronde.toml:ro \
  -v ./fronde_token:/run/secrets/fronde_token:ro \
  -v ./fronde_password:/run/secrets/fronde_password:ro \
  -v fronde-data:/data \
  -e CLAUDE_CODE_OAUTH_TOKEN \
  fronde
```

The image builds the web interface and the server as `mise run build` does. The
`fronde-data` volume holds the memory, the conversation and the agent's home, where Claude
keeps its settings. Then [sign Fronde in to Claude](#signing-fronde-in-to-claude). The image
checks itself with `fronde healthcheck`, which reads the address from `/config/fronde.toml`.

## Configuration

```toml
# Secrets are never written here: use { file = "…" } or { env = "…" }.

[server]
listen = "127.0.0.1:8484"
# Bearer token required on /mcp (at least 32 characters).
token = { file = "/run/secrets/fronde_token" }

[data]
dir = "/data"
timezone = "Europe/Paris"
user = "alix"

# The chat at /, behind a password. Without this section the server is only the MCP hub.
[web]
password = { file = "/run/secrets/fronde_password" }
# true when the page is served over https.
secure_cookie = false

# code_run_python: Python in a sandbox for calculations. The image ships the runtime here; from a
# checkout, `mise run python` fetches it into python/.
[python]
dir = "/app/python"

# The agent Fronde runs (ACP over stdio), started on the first message.
[agent]
command = ["claude-agent-acp"]
# Extra environment for the agent, written in plain text: never put a secret here.
env = {}
# Claude Code's own tools Fronde may use besides the hub's; only these two are allowed, [] for none.
builtin_tools = ["WebSearch", "WebFetch"]
```

[`fronde.example.toml`](fronde.example.toml) also shows the memory caps and the `[[sources]]` of
tools: the calendar component and upstream MCP servers. `fronde check --config fronde.toml`
validates the file and its secrets, prints the hashes of each component and says which approval
applied.

### Migrating from `[calendar]` and `[[mcp]]`

Both sections are gone, and Fronde refuses them with a message saying what to write. An `[[mcp]]`
section becomes a `[[sources]]` entry with `kind = "mcp"` and the same keys. The calendar is now a
WebAssembly component, `calendar.wasm`, shipped in the image under `/app/components` (from a
checkout, `mise run build` builds it into `components/dist` first). For `[calendar]`, the message
is the entry to paste in its place, filled with your values:

```toml
[[sources]]
kind = "component"
name = "calendar"
path = "/app/components/calendar.wasm"
allow = ["https://cloud.example"]   # the origin of the two URLs

[sources.settings]
events_url = "https://cloud.example/remote.php/dav/calendars/alix/personal/"
reminders_url = "https://cloud.example/remote.php/dav/calendars/alix/tasks/"

[[sources.credentials]]
origin = "https://cloud.example"
basic = { username = "alix", password = { env = "FRONDE_CALDAV_PASSWORD" } }
```

The calendar shipped with Fronde needs no hashes: the binary knows those of the components built
with it, and `fronde check` says "approved: shipped with this build". A component from elsewhere
needs the `sha256` and `manifest_sha256` that `fronde check` prints. The component never sees the
password: Fronde adds it to requests for that origin, over https only (or plain http to
`localhost`; in Docker, that is the container). See the "Configure the calendar" guide in
`docs/`.

## Signing Fronde in to Claude

The agent uses Claude Code's own sign-in and inherits Fronde's environment. It reads none of
Claude Code's settings: your plugins, MCP servers, hooks and permission rules do not apply to
Fronde, which only has the hub's tools plus web search and fetch.

- **Native**: nothing to do. The agent uses your Claude Code sign-in in `~/.claude`. To run your
  installed Claude Code rather than the one bundled with the adapter, set
  `env = { CLAUDE_CODE_EXECUTABLE = "/home/you/.local/bin/claude" }` under `[agent]`.
- **Docker, with a token** (recommended): on any machine with a browser, run
  `claude setup-token`. It signs in to your subscription and prints a long-lived token. Give it to
  the container as the `CLAUDE_CODE_OAUTH_TOKEN` environment variable, as in the `docker run`
  above (`-e CLAUDE_CODE_OAUTH_TOKEN` passes the variable from your shell without writing it on
  the command line), or through your orchestrator's secrets. Never put it in `fronde.toml`.
- **Docker, with an existing sign-in**: mount a Claude configuration directory at
  `/data/home/.claude`, owned by uid 65532, for example
  `-v ./claude:/data/home/.claude` after `cp -r ~/.claude ./claude && sudo chown -R 65532:65532 ./claude`.
  The copy and the original share one sign-in; if one of them stops working, use a token.

When the agent is not signed in, the chat answers « L'agent n'est pas connecté à Claude » and the
server's log has the details. When the adapter is missing, it asks to install it.

## Behind a reverse proxy

- Forward the `Host` header unchanged. The chat's WebSocket (`/ws`) only opens when the page's
  `Origin` matches the `Host` the server receives.
- Pass WebSocket upgrades through on `/ws`.
- Serve it over https and set `secure_cookie = true` under `[web]`.

Caddy does all of this with `reverse_proxy 127.0.0.1:8484`. With nginx:

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    ''      close;
}

server {
    # listen, server_name and certificates as usual
    location / {
        proxy_pass http://127.0.0.1:8484;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 1h;
    }
}
```

## Using the tools from other agents

Any MCP client reaches Fronde's tools at `/mcp` with the header
`Authorization: Bearer <token>`. For Claude Code:

```sh
claude mcp add --transport http fronde http://127.0.0.1:8484/mcp \
  --header "Authorization: Bearer $(cat fronde_token)"
```

## Development

You need [mise](https://mise.jdx.dev).

```sh
mise install
mise run check   # what CI runs: rustfmt, clippy, tests, cargo-deny
mise run build   # target/release/fronde and the web interface
```

The web interface is a [Dioxus](https://dioxuslabs.com) app in `crates/web`. The Dioxus CLI
(`dx`, pinned by mise) builds it into static files that the server serves. To work on it, run
the server and the interface side by side:

```sh
mise run dev       # the server on http://127.0.0.1:8484, password `fronde`
mise run web-dev   # the interface with hot reload on http://127.0.0.1:8080
```

Open http://127.0.0.1:8080. dx serves the pages and passes the chat's WebSocket (`/ws`) and the
sign-in forms (`/auth/…`) to the server. `mise run dev` keeps its configuration and data in
`target/dev`, and needs the Claude adapter on the `PATH` to chat. `mise run web-build` builds the
interface alone, into `target/dx/fronde-web/release/web/public`. On its first build dx downloads
the tools it needs (wasm-bindgen, wasm-opt, esbuild) into `~/.local/share/.dx`.

See [`AGENTS.md`](AGENTS.md) for the layout and the rules.

## Documentation

The user guide is an mdBook in [`docs/`](docs/src/SUMMARY.md): a tutorial, how-to guides, the
reference and explanations. `mise run docs` builds it into `docs/book`.

<h1 align="center">Jarvis</h1>

<p align="center">
  <strong>A personal assistant that runs on your own Claude subscription.</strong><br>
  A web chat with Claude, and your memory, calendar and tools served over MCP.
</p>

---

Jarvis (a code name) is a small self-hosted server. It drives Claude Code through its
[ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp), signed in with your own
Claude subscription, and gives it one set of tools: a memory kept in Markdown files, your CalDAV
calendar and reminders, and the tools of other MCP servers. You talk to it from a web page on your
phone or computer. Other agents, such as Claude Code, Codex or T3 Code, reach the same tools at
`/mcp`.

## Features

- **Chat** from any browser, behind a password: answers stream in, the tools Jarvis uses show as
  they run, any turn can be stopped, and the conversation survives restarts.
- **Your subscription**: Claude Code runs through its ACP adapter, with its built-in tools turned
  off. Jarvis only uses its own tools.
- **Memory** in plain Markdown files (`USER.md`, `MEMORY.md` and a dated journal), searchable and
  versioned with git.
- **Calendar and reminders** on any CalDAV server (Nextcloud, Radicale, Fastmail and others).
- **More tools** from upstream MCP servers (MCPHub or any other), found with `search_tools`.
- **An MCP server** at `/mcp` behind a bearer token, for your other agents.
- **One binary or one container**, configured with one TOML file. Secrets live in files or
  environment variables, never in the configuration.

## Quick start

Jarvis needs a configuration file, a token for `/mcp` and a password for the chat. Copy
[`jarvis.example.toml`](jarvis.example.toml) to `jarvis.toml` and edit it (see
[Configuration](#configuration)), then create the secrets it points to:

```sh
(umask 077 && openssl rand -hex 32 > jarvis_token)
(umask 077 && printf '%s' 'your chat password' > jarvis_password)
```

### Native, with mise

You need [mise](https://mise.jdx.dev), Node.js 22 or later, and Claude Code signed in to your
subscription (`claude` works in your terminal).

```sh
git clone https://github.com/AlixANNERAUD/jarvis && cd jarvis
mise install
mise run build
npm install --global @agentclientprotocol/claude-agent-acp@0.107.0
target/release/jarvis check --config jarvis.toml
target/release/jarvis serve --config jarvis.toml
```

Open http://127.0.0.1:8484 and sign in with the chat password. The server reads the chat's files
from `target/site`; run it from the checkout, or set `JARVIS_SITE_ROOT`.

### Docker

The image has Jarvis, Node.js and the Claude adapter. In the container, set
`listen = "0.0.0.0:8484"` and `dir = "/data"`, and point the secrets to `/run/secrets/…`. Jarvis
runs as uid 65532, which must be able to read the secret files.

```sh
sudo chown 65532 jarvis_token jarvis_password
docker build -t jarvis .
docker run -d --name jarvis -p 8484:8484 \
  -v ./jarvis.toml:/config/jarvis.toml:ro \
  -v ./jarvis_token:/run/secrets/jarvis_token:ro \
  -v ./jarvis_password:/run/secrets/jarvis_password:ro \
  -v jarvis-data:/data \
  -e CLAUDE_CODE_OAUTH_TOKEN \
  jarvis
```

The `jarvis-data` volume holds the memory, the conversation and the agent's home, where Claude
keeps its settings. Then [sign Jarvis in to Claude](#signing-jarvis-in-to-claude). The image
checks itself with `jarvis healthcheck`, which reads the address from `/config/jarvis.toml`.

## Configuration

```toml
# Secrets are never written here: use { file = "…" } or { env = "…" }.

[server]
listen = "127.0.0.1:8484"
# Bearer token required on /mcp (at least 32 characters).
token = { file = "/run/secrets/jarvis_token" }

[data]
dir = "/var/lib/jarvis"
timezone = "Europe/Paris"
user = "alix"

# The chat at /, behind a password. Without this section the server is only the MCP hub.
[web]
password = { file = "/run/secrets/jarvis_password" }
# true when the page is served over https.
secure_cookie = false

# The agent Jarvis runs (ACP over stdio), started on the first message.
[agent]
command = ["claude-agent-acp"]
# Extra environment for the agent, written in plain text: never put a secret here.
env = {}
```

[`jarvis.example.toml`](jarvis.example.toml) also shows the memory caps, the CalDAV calendar and
upstream MCP servers. `jarvis check --config jarvis.toml` validates the file and its secrets.

## Signing Jarvis in to Claude

The agent uses Claude Code's own sign-in and inherits Jarvis's environment. It reads none of
Claude Code's settings: your plugins, MCP servers, hooks and permission rules do not apply to
Jarvis, which only has the hub's tools.

- **Native**: nothing to do. The agent uses your Claude Code sign-in in `~/.claude`. To run your
  installed Claude Code rather than the one bundled with the adapter, set
  `env = { CLAUDE_CODE_EXECUTABLE = "/home/you/.local/bin/claude" }` under `[agent]`.
- **Docker, with a token** (recommended): on any machine with a browser, run
  `claude setup-token`. It signs in to your subscription and prints a long-lived token. Give it to
  the container as the `CLAUDE_CODE_OAUTH_TOKEN` environment variable, as in the `docker run`
  above (`-e CLAUDE_CODE_OAUTH_TOKEN` passes the variable from your shell without writing it on
  the command line), or through your orchestrator's secrets. Never put it in `jarvis.toml`.
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

Any MCP client reaches Jarvis's tools at `/mcp` with the header
`Authorization: Bearer <token>`. For Claude Code:

```sh
claude mcp add --transport http jarvis http://127.0.0.1:8484/mcp \
  --header "Authorization: Bearer $(cat jarvis_token)"
```

## Development

You need [mise](https://mise.jdx.dev).

```sh
mise install
mise run dev     # http://127.0.0.1:3000 with live reload, password `jarvis`
mise run check   # what CI runs: rustfmt, clippy, tests, cargo-deny
mise run build   # target/release/jarvis and the site in target/site
```

`mise run dev` needs the Claude adapter on the `PATH` to chat. See [`AGENTS.md`](AGENTS.md) for
the layout and the rules.

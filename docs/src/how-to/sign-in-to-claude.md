# Sign Fronde in to Claude

Fronde's agent is Claude Code, run through its ACP adapter. It uses Claude Code's own sign-in and
inherits Fronde's environment. It reads none of Claude Code's settings: your plugins, MCP servers,
hooks and permission rules do not apply to it.

## Native

Nothing to do once `claude` works in your terminal: the adapter uses the sign-in kept in
`~/.claude`.

The adapter brings its own copy of Claude Code. To run the one you installed instead, set its path
under `[agent]`:

```toml
[agent]
env = { CLAUDE_CODE_EXECUTABLE = "/home/alex/.local/bin/claude" }
```

## Docker, with a token (recommended)

1. On any machine with a browser, run `claude setup-token`. It signs in to your subscription and
   prints a long-lived token.
2. Give it to the container as the `CLAUDE_CODE_OAUTH_TOKEN` environment variable, with
   `-e CLAUDE_CODE_OAUTH_TOKEN` as in [Run in Docker](run-in-docker.md), or through your
   orchestrator's secrets.

Never put the token in `fronde.toml`: `[agent] env` is written in plain text and is meant for
settings, not secrets.

## Docker, with an existing sign-in

Mount a Claude configuration directory at `/data/home/.claude`, owned by uid 65532:

```sh
cp -r ~/.claude ./claude && sudo chown -R 65532:65532 ./claude
docker run … -v ./claude:/data/home/.claude …
```

The copy and the original share one sign-in. If one of them stops working, use a token instead.

## When it fails

- The chat answers *L'agent n'est pas connecté à Claude*: the agent is not signed in. The server's
  log has the details.
- The chat says it cannot start the agent and asks to install the adapter: `claude-agent-acp` is
  not on the `PATH`. Run `mise run agent` in the checkout, or set the full path in
  `[agent] command`.

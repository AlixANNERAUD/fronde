# Sign Fronde in to Claude

Fronde's agent is Claude Code, run through its ACP adapter. It has its own home, `home/` in the
data directory, and keeps its sign-in there: it never uses your `~/.claude`. It reads none of
Claude Code's settings: your plugins, MCP servers, hooks and permission rules do not apply to it.
Of Fronde's environment it keeps only a few variables, `CLAUDE_CODE_OAUTH_TOKEN` among them (see
[Configuration](../reference/configuration.md#agent)).

## Native

Sign the agent in once, in its own home. With the default data directory:

```sh
HOME=~/.local/share/fronde/home claude
```

Then type `/login` and follow the browser sign-in. Quit `claude` once it is done. Use your
`[data] dir` followed by `/home` if you set one. Start Fronde once before, so that the directory
exists.

Alternatively, run `claude setup-token`, which prints a long-lived token, and give it to Fronde as
the `CLAUDE_CODE_OAUTH_TOKEN` environment variable (for the service, through a systemd drop-in
with `Environment=` or, better, a file only you can read with `EnvironmentFile=`).

Versions of Fronde before the agent got its own home used your `~/.claude`. After the upgrade,
the agent is signed out until you do the above, and the chat's agent session starts afresh: its
history stays in the chat, but the agent no longer remembers the previous session.

The adapter brings its own copy of Claude Code. To run the one you installed instead, set its path
under `[agent]`:

```toml
[agent]
env = { CLAUDE_CODE_EXECUTABLE = "/home/alex/.local/bin/claude" }
read = ["/home/alex/.local/share/claude"]
```

The confined agent may run only programs from the system's directories, its `PATH` and the
adapter's install, so `read` grants the directory the installed Claude Code lives in
(`~/.local/bin/claude` is a link into `~/.local/share/claude`).

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

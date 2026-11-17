# Sign Fronde in to Claude

Fronde's agent is Claude Code, run through its ACP adapter. Each conversation (thread) has its
own agent with its own home: `home/` in the data directory for the main one, the chat,
`threads/<id>/home/` for the others. None uses your `~/.claude`. It reads none of Claude Code's settings: your
plugins, MCP servers, hooks and permission rules do not apply to it. It signs in with a
long-lived token, which you make once and give to Fronde as `[agent] oauth_token` (see
[Configuration](../reference/configuration.md#agent)).

## Native

1. In your own terminal and home, as usual, run `claude setup-token`. It signs in to your
   subscription in the browser and prints a long-lived token.
2. Keep the token where Fronde reads secrets, in one of three ways:
   - a file only you can read, beside `fronde.toml`:

     ```sh
     (umask 077; cat > claude-token)   # paste the token, press Enter, then Ctrl-D
     ```

     ```toml
     [agent]
     oauth_token = { file = "claude-token" }
     ```

   - stored encrypted in `fronde.db`, for Claude's API, from that file or a pipe:

     ```sh
     fronde secret set claude --origin https://api.anthropic.com --config fronde.toml < claude-token
     ```

     ```toml
     [agent]
     oauth_token = { stored = "claude" }
     ```

   - an environment variable of the service, with `oauth_token = { env = "CLAUDE_TOKEN" }` (for a
     systemd unit, through a file only you can read with `EnvironmentFile=`).
3. Keep the token on one line: Fronde refuses a token with a space or a line break inside, which is
   what a token pasted across the terminal's wrapped lines looks like (Claude's API would answer
   *401 Invalid bearer token*).
4. Restart Fronde. It reads the token at each start of the agent, so a new token in the file or
   the store applies from the next one.

Do not sign the agent in by running `claude` with `HOME` set to its home (`/login` there). The
agent can write its home: a hook, an `apiKeyHelper` or a `statusLine` it put in
`.claude/settings.json`, or an MCP server it added to `.claude.json`, would run as you, outside
its sandbox, the moment you start `claude` there. The token keeps you from ever running Claude
Code in a directory the agent controls.

Every thread other than the main one starts with a fresh, empty home: only the token signs its
agent in. A Claude directory placed in the main thread's home (see Docker below) signs in the
main thread only.

Versions of Fronde before the agent got its own home used your `~/.claude`. After the upgrade,
the agent is signed out until you give it a token, and the chat's agent session starts afresh:
its history stays in the chat, but the agent no longer remembers the previous session.

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
   orchestrator's secrets. The agent keeps that variable from Fronde's environment. To name it
   as a secret instead, so that Fronde reads it at each start, set
   `oauth_token = { env = "CLAUDE_CODE_OAUTH_TOKEN" }` under `[agent]`, or use a mounted file
   with `{ file = "…" }`.

Never put the token in `fronde.toml`: `[agent] env` is written in plain text and is meant for
settings, not secrets.

## Docker, with an existing sign-in

Mount a Claude configuration directory at `/data/home/.claude`, owned by uid 65532:

```sh
cp -r ~/.claude ./claude && sudo chown -R 65532:65532 ./claude
docker run … -v ./claude:/data/home/.claude …
```

The copy and the original share one sign-in. If one of them stops working, use a token instead.
This signs in the main thread's agent only: the agents of other threads have homes of their own,
and need the token.

## When it fails

- The chat answers *L'agent n'est pas connecté à Claude*: the agent is not signed in. The server's
  log has the details.
- The chat says it cannot start the agent and asks to install the adapter: `claude-agent-acp` is
  not on the `PATH`. Run `mise run agent` in the checkout, or set the full path in
  `[agent] command`.

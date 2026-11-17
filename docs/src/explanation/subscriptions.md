# Why subscriptions and ACP

Fronde uses the Claude subscription you already pay for, through the same Claude Code you use in a
terminal. It does not call the Anthropic API with a key.

## Why not an API key

- **Cost.** An assistant that reads your calendar and memory on every message is cheap on a
  subscription you already have, and adds up quickly when billed per token.
- **One account.** Your usage stays in one place, under the limits you know.
- **No key to guard.** Fronde never stores a model provider's secret. The sign-in belongs to
  Claude Code, which keeps it in `~/.claude` or receives it as `CLAUDE_CODE_OAUTH_TOKEN`.

## Why ACP

The [Agent Client Protocol](https://agentclientprotocol.com) is how editors drive coding agents:
a JSON-RPC session over standard input and output, with streamed text, tool calls and permission
requests. Claude Code speaks it through its
[ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp). Driving the agent this way
means:

- Fronde runs the official client on its official sign-in, not an imitation of it.
- Fronde gets the agent's own loop: tool use, context management, web search, resumable sessions.
- The same client code can drive other agents that speak ACP. Codex and Antigravity are
  **planned** (see [Other providers](#other-providers)).

The adapter is locked in `agent/package-lock.json` and installed with `mise run agent`, or into
the container image, so an update of the adapter is a reviewed change.

## Other providers

What is Claude's alone is gathered in a provider profile (see
[How Fronde works](architecture.md#providers-and-models)); Claude is the only provider, and
`[agent]` holds its settings. Another provider needs, besides its profile, a sign-in design of
its own: each of these CLIs keeps its sign-in in a home it writes during an interactive login,
and Fronde never runs a CLI in a home its agent can write (see
[Sign Fronde in to Claude](../how-to/sign-in-to-claude.md)). Each comes with its own
`[providers.<id>]` table.

- **Codex**, **planned**, through `@agentclientprotocol/codex-acp` (Apache-2.0, an npm package
  like Claude's adapter, so its sandbox grant is its `node_modules`). It needs: a sign-in other
  than an API key (the ChatGPT login writes `~/.codex`); a way to carry the hub's token, since
  custom headers on its MCP servers are not documented and it may not expand `${VAR}` (a
  `bearer_token_env_var` setting would keep the token off its command line); its permission
  option ids read and listed before any lasting option is offered (until then none is); its own
  `_meta`, if any, to turn off the owner's configuration; and its environment variables kept by
  the sandbox. It lists its models, reasoning effort, approval and sandbox as config options.
- **Gemini** means **Antigravity** (`agy acp`), **planned**: on a personal subscription, Gemini
  CLI was replaced by Antigravity in June 2026. It is a closed-source native binary (its sandbox
  grant is its directory). Its sign-in is a cached interactive login (`agy acp --login`); whether
  it takes MCP servers from the client, with which headers, and how it lists models, is not
  documented and must be checked first. Third-party bridges warn of a terms-of-service risk and
  are not used.

Until a provider's permission options are read, its profile offers no lasting option and none on
a mode switch: a new provider never inherits Claude's lists.

## Why MCP for the tools

Fronde's tools are not wired into one agent: the hub serves them over MCP, the protocol every
agent already speaks. Fronde's own agent reaches them like any other client, at `/mcp` with the
token. The memory and calendar you use in the chat are the same ones Claude Code sees in your
terminal.

## The trade-off

A subscription is meant for a person using their own agent, and comes with usage limits. Fronde
is built for exactly that: one owner, one conversation, the owner's own machine. It is not a way
to serve other people from one subscription.

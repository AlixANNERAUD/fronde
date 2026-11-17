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

## Usage and quotas

A subscription has two usage windows: one of 5 hours and one of a week. Fronde reads how much
of each is used from what the Claude adapter already sends during a turn. It makes no request of
its own for it: no call to an Anthropic endpoint, no `/usage` turn.

What the adapter sends:

- A **reading only when the limits change.** Claude Code emits a `rate_limit_event` when an API
  response's rate-limit headers change (about once per percentage point); the adapter passes it
  on in `_meta["_claude/rateLimit"]` of the next `usage_update`, maybe a later turn's. A turn
  whose limits did not change brings no reading.
- **One window, documented**: its kind (`five_hour`, `seven_day`, `seven_day_opus`,
  `seven_day_sonnet`, `overage`, …), its status (`allowed`, `allowed_warning`, `rejected`), its
  reset time, and its use as a fraction from 0 to 1, each of them optional.
- **Both windows, undocumented**: Claude Code adds `unifiedWindows`, the 5-hour and weekly
  windows at once. It is marked internal and may change with any release: Fronde reads it when
  it is there and falls back on the documented window.
- **Paid extra usage in use** (`isUsingOverage`), kept as an extra window marked as a warning.
- **A turn refused for a limit**: the prompt fails with `errorKind: "rate_limit"` and the latest
  reading, whose reset time says when turns run again.

What it does not send, and Fronde does not make up:

- **Nothing between turns.** Your use elsewhere (a terminal, claude.ai) shows only at Fronde's
  next turn; each window carries the time it was read.
- **No reading before Fronde's first turn**, and no plan, account or window length: the length
  comes from the kind.
- **Per-model weekly windows** only when one is the event's own window.

Every field is checked and anything odd is left out: a use outside 0 to 1 by more than a few
points (`42` would be a percentage) is dropped rather than rescaled, a value just past an edge is
clamped, a reset time more than a year from the reading (milliseconds, say) is dropped, and a
reading that does not parse never fails a turn. The log names the fields it left out, never the
whole `_meta`. Once a window's reset time has passed, it reads as reset since the reading. Codex
and Antigravity send no reading Fronde reads yet.

Capturing the readings during turns and keeping them across restarts, showing them in the chat
and in `fronde check`, and keeping a share of each window for your own turns by pausing
background work, are **planned**.

## Why MCP for the tools

Fronde's tools are not wired into one agent: the hub serves them over MCP, the protocol every
agent already speaks. Fronde's own agent reaches them like any other client, at `/mcp` with the
token. The memory and calendar you use in the chat are the same ones Claude Code sees in your
terminal.

## The trade-off

A subscription is meant for a person using their own agent, and comes with usage limits. Fronde
is built for exactly that: one owner, one conversation, the owner's own machine. It is not a way
to serve other people from one subscription.

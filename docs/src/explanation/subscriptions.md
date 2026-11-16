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
- The same client code can drive other agents that speak ACP. Codex and Gemini are **planned**.

The adapter is locked in `agent/package-lock.json` and installed with `mise run agent`, or into
the container image, so an update of the adapter is a reviewed change.

## Why MCP for the tools

Fronde's tools are not wired into one agent: the hub serves them over MCP, the protocol every
agent already speaks. Fronde's own agent reaches them like any other client, at `/mcp` with the
token. The memory and calendar you use in the chat are the same ones Claude Code sees in your
terminal.

## The trade-off

A subscription is meant for a person using their own agent, and comes with usage limits. Fronde
is built for exactly that: one owner, one conversation, the owner's own machine. It is not a way
to serve other people from one subscription.

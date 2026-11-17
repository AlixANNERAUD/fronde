# How Fronde works

Fronde is one Rust binary, `fronde serve`, with two layers: a **hub** that owns your tools and
memory and serves them over MCP, and an **assistant** that drives one coding agent over ACP and
shows the conversation in a web chat.

```text
browser ──── /, /ws, /auth ───┐
                              ▼
                  ┌────── fronde serve ──────┐
                  │  web chat and its log    │
                  │  assistant ── ACP ───────┼──▶ claude-agent-acp ──▶ Claude
                  │                          │          │           (your subscription)
other agents ─────┼─▶ /mcp: the hub ◀── MCP ─┼──────────┘
                  │    memory, components,   │
                  │    tool registry ── MCP ─┼──▶ upstream MCP servers
                  └──────────────────────────┘
```

## The hub

The hub (`crates/hub`) is a registry of tools. Its own tools are written in Rust: memory and the
two tool-search tools. The others come from the `[[sources]]` of the configuration: tool
components, such as the calendar and reminders, and upstream MCP servers, whose tools are
prefixed with the server's name. The hub serves the registry at `/mcp` to any client with the
token.

One supervisor starts every source. A component that fails to load is skipped with a warning; an
upstream server that is down is retried, 2 seconds after the first failure and up to a minute
apart. While a source is down, its tools stay listed and answer that it is unreachable.

The hub is useful alone: without `[web]`, Fronde is only the hub, and agents you already use,
such as Claude Code in a terminal, get one shared memory and one calendar.

Tools from upstream servers are **deferred**: they stay out of the tool list, and an agent finds
them with `search_tools` and calls them with `use_tool`. A dozen servers would otherwise put
hundreds of tool schemas in the model's context on every turn.

## The assistant

With `[web]`, Fronde also runs an agent for its own chat (`crates/agent`). On the first message it
starts `claude-agent-acp`, the Claude Code ACP adapter, and opens a session with it over standard
input and output. The session gets the hub as an MCP server named `fronde`, reached over the
loopback with the same token, and a short system prompt. The session id is saved, so a restart
resumes the same conversation.

Each turn streams back as events: text, tool calls, permission requests, the end of the turn.
Fronde numbers them and saves each one in the database, committed to disk, before it broadcasts
it to every open page: a page never shows an entry a crash could lose. A page that reconnects
says which entry it saw last and gets only what it missed. The conversation is one stream of the
database, the topic `conversation`; the session id is saved there too.

## The web interface

The interface (`crates/web`) is a Dioxus single-page app compiled to WebAssembly. The server has
no Dioxus dependency: it serves the built files from `FRONDE_SITE_ROOT`. The chat's logic, what
to show for each event and when to reconnect, lives in `crates/client`, which has no framework
and is tested natively.

## Wasm components

`crates/wasm` runs WebAssembly components that bring tools, sandboxed and with a deny-by-default
HTTP gate. The gate's rules live in one place, `fronde_hub::origin`, shared by the configuration
and the sandbox: the `Origin` type (scheme, normalised host, port), parsed once when the
configuration loads and once per request, which addresses are local or never allowed, and which
origins may carry credentials. Integrations with outside services are components, the CalDAV calendar first: each is
a `.wasm` file with a manifest saying what it asks for, and a `[[sources]]` entry granting part
of it, approved by the files' hashes: those in the entry, or those of the components the binary
was built with (`crates/wasm/build.rs` hashes `components/dist`). See [Component interface](../reference/wit-interface.md)
and [Security model](security.md#wasm-components).

## Planned

These are designed but **not built yet**:

- **A user service.** `fronde service install|uninstall|status`, installing Fronde as a
  `systemd --user` unit without root, with each agent in its own scope under a memory-capped
  slice, so that the agents, not the server, give way when memory runs low.
- **Federation.** Several Fronde instances linked together: a node, such as a laptop, opens an
  outgoing WebSocket over TLS (`wss://`) to the main instance, which reaches the node's tools over
  MCP inside that socket. TLS is mandatory between instances, with a self-signed certificate
  generated at first start and trusted by pinning its SHA-256 fingerprint, exchanged when pairing.
- **More agents.** Codex and Gemini through their own ACP adapters, on their own subscriptions.

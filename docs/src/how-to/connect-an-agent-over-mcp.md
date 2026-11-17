# Connect an agent over MCP

Any MCP client reaches Fronde's tools and memory at `/mcp`, over Streamable HTTP, with the header
`Authorization: Bearer <token>`. The token is the one `[server] token` points to.

## Claude Code

```sh
claude mcp add --transport http fronde http://127.0.0.1:8484/mcp \
  --header "Authorization: Bearer $(cat fronde_token)"
```

Claude Code then lists the hub's tools as `mcp__fronde__…`, and receives the hub's instructions
with your core memory when it connects. Add `--scope user` to have it in every project.

## Other clients

T3 Code, Codex, an aggregator such as MCPHub, or any other MCP client needs the same three
things:

| Setting | Value |
| --- | --- |
| Transport | Streamable HTTP |
| URL | `http://<host>:8484/mcp`, or your reverse proxy's https address |
| Header | `Authorization: Bearer <token>` |

Keep the token out of shared files: use the client's own secret store or an environment variable.

When Fronde runs its chat (`[web]`), an agent can ask you a question with `user_ask`: the card
shows in the chat, headed with the client's name, and the call waits for your answer, up to 10
minutes by default. Raise the client's tool timeout above that, for example Codex's
`tool_timeout_sec`, which is 60 s by default; a call cut short leaves the card answerable, but the
answer no longer reaches the agent.

## Check the connection

A request without the token gets `401 Unauthorized`:

```sh
curl -i -X POST http://127.0.0.1:8484/mcp
```

With the token, an MCP client lists `search_tools`, `use_tool` and the `memory_*` tools, plus the
calendar tools when the calendar component is configured. See [Tools](../reference/tools.md).

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

## Check the connection

A request without the token gets `401 Unauthorized`:

```sh
curl -i -X POST http://127.0.0.1:8484/mcp
```

With the token, an MCP client lists `search_tools`, `use_tool` and the `memory_*` tools, plus the
calendar tools when `[calendar]` is set. See [Tools](../reference/tools.md).

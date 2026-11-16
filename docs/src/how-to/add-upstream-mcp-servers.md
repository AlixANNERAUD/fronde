# Add upstream MCP servers

Fronde can connect to other MCP servers and offer their tools to its agent and to every client of
`/mcp`. Add one `[[sources]]` entry with `kind = "mcp"` per server, with a unique `name` and
exactly one of `url` or `command`.

## A server over HTTP

```toml
[[sources]]
kind = "mcp"
name = "mcphub"
url = "https://mcphub.example/mcp"
token = { env = "MCPHUB_TOKEN" }
```

`token` is optional. Fronde sends it as `Authorization: Bearer <token>`. Like every secret, it
comes from a file or an environment variable.

## A server run as a command

```toml
[[sources]]
kind = "mcp"
name = "files"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/srv/shared"]
```

Fronde starts the command and talks MCP over its standard input and output. The command runs with
Fronde's rights and environment, so only run programs you trust.

## Names

`name` must match `^[a-z][a-z0-9_]{0,63}$`, must be unique among all sources, and may not be
`hub`, which is reserved for Fronde's own tools. Each upstream tool becomes `<name>_<tool>`, lowercased, with any
character outside `a-z`, `0-9` and `_` turned into `_`, cut at 64 characters.

## How the agent finds them

Upstream tools are **deferred**: they do not appear in the tool list. The hub's instructions list
their names, and the agent calls `search_tools` with its need, then `use_tool` with the name it
found. This keeps the agent's context small however many servers you add.

## When a server is down

Fronde connects in the background and keeps trying, waiting 2 seconds at first and up to one
minute between attempts. A server that does not answer within 30 seconds counts as down. While it
is down, its tools answer that it is unreachable. When it comes back, its tools are listed again.
Check `fronde serve`'s log for `connected`, `disconnected` and `cannot start` lines.

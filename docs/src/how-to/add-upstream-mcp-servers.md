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
`hub` or overlap the hub's reserved tool prefixes (`code_`, `hub_`, `memory_`, `search_`, `use_`). Each
upstream tool becomes `<name>_<tool>`, lowercased, with any character outside `a-z`, `0-9` and `_`
turned into `_`, cut at 64 characters.

## How the agent finds them

Upstream tools are **deferred**: they do not appear in the tool list. The hub's instructions list
their names, and the agent calls `search_tools` with its need, then `use_tool` with the name it
found. This keeps the agent's context small however many servers you add.

## When a server asks you something

A server may ask you to fill a form while one of its tools runs (MCP's `elicitation/create`, or an
`input_required` round of MCP 2026-07-28). When the chat runs (`[web]`), Fronde tells servers it
takes forms, and shows each one as a card in the chat, headed with the server's name, such as
*Question de « github » (serveur MCP)*. Only an admin answers it, whoever's call made the server
ask. The fields are those of the agent's forms (see
[Forms from the agent](../explanation/requests.md#forms-from-the-agent)): questions, texts,
numbers and toggles. *Refuser* sends the server a `decline`; a card left for **10 minutes**
expires, and the server gets a `cancel`. When the server gives up its own request, the card
closes; so does it when the call that waits on an `input_required` round is given up.

Fronde never takes a secret in a form. A form with a text or number field named like one
(*password*, *token*, *key*, *secret*…) is declined without a card, the reason in the answer's
`_meta` (`fronde/refused`), and `fronde serve` logs a warning. Give such a server its secret
through its configuration instead, such as its `token` above. A server's forms by URL (a sign-in
page) are cancelled: Fronde does not show them yet.

Without the chat, Fronde does not tell servers it takes forms, and cancels any form one sends
anyway.

## When a server is down

Fronde connects in the background and keeps trying, waiting 2 seconds at first and up to one
minute between attempts. A server that does not answer within 30 seconds counts as down. While it
is down, its tools answer that it is unreachable. When it comes back, its tools are listed again.
Check `fronde serve`'s log for `connected`, `disconnected` and `cannot start` lines.

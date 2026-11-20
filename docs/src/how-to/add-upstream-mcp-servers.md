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
comes from a file, an environment variable or the encrypted store.

### Keep the token in the encrypted store

Store the token for the server's origin (scheme, host and port, no path), then name it:

```sh
fronde secret set mcphub --origin https://mcphub.example --config fronde.toml < mcphub-token
```

```toml
token = { stored = "mcphub" }
```

Fronde opens it each time it connects, and only for that origin: a secret stored for another
origin is refused, and the message gives the `fronde secret set` command to run. If the secret is
not stored yet, the server waits, with the same backoff as a server that is down, and connects
within a minute of `fronde secret set`, with no restart. `fronde check` says whether it opens,
never showing it. A server run as a `command` has no origin: use `{ file = "…" }` there. See
[Store a secret](store-a-secret.md).

## A server run as a command

```toml
[[sources]]
kind = "mcp"
name = "files"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/srv/shared"]
```

Fronde starts the command and talks MCP over its standard input and output. The command runs with
Fronde's rights, so only run programs you trust. Its environment is cleared: it gets only `PATH`,
`HOME`, `LANG`, `TMPDIR`, the proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`,
`ALL_PROXY`, in either case) and the certificate variables (`SSL_CERT_FILE`, `SSL_CERT_DIR`,
`REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`) from Fronde's, when set, never a secret's or a
sign-in variable. A command with an argument that sets an agent's sign-in variable, wherever it
sits (`env CLAUDE_CODE_OAUTH_TOKEN=… server`, `--env=ANTHROPIC_API_KEY=…`, `sh -c 'export …=…'`),
or a `token` read from one (`{ env = "ANTHROPIC_API_KEY" }`), is refused: an account's token
goes to its agents only. This guards against a mistake in the configuration; it is not a
boundary, since a command can always build a variable's name Fronde does not see.

## Names

`name` must match `^[a-z][a-z0-9_]{0,63}$`, must be unique among all sources, and may not be
`hub`, `fronde` or `assistant` (a server's questions are headed with its name), or overlap the hub's reserved tool prefixes (`code_`, `hub_`, `integrations_`, `memory_`, `search_`, `threads_`, `use_`, `user_`). Each
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
*Question de « github » (serveur MCP)*, saying whose call the server was serving (*Pendant un
appel de Fronde*) and, above *Envoyer*, that what you type goes to that server. Only an admin
answers it, whoever's call made the server ask. The fields are those of the agent's forms (see
[Forms from the agent](../explanation/requests.md#forms-from-the-agent)): questions, texts,
numbers and toggles.

- *Refuser* sends the server a `decline`.
- A card left for **10 minutes** expires, and the server gets a `cancel`.
- The card closes, and the server gets a `cancel`, when the server gives up its own request, when
  the last call to the server ends (its caller gave it up, say), and, when Fronde's assistant
  alone was calling, when you press *Stop* or its turn ends.
- A form is shown only while a call to the server is in flight, one at a time per server: a form
  that comes outside a call, or while another of the server's cards is open, is cancelled
  unseen. `fronde serve` logs a warning for refused forms, at most once a minute per server.
- A pending card expires when Fronde restarts.

Fronde never takes a secret in a form. A form whose message or fields read like they ask for one
(*password*, *token*, *secret*…, or a field named *code*, *PIN* or *key*) is declined without a card, with a plain reason in
the answer's `_meta` (`fronde/refused`), and `fronde serve` logs which text tripped the rule. Give
such a server its secret through its configuration instead, such as its `token` above. A server's
forms by URL (a sign-in page) are cancelled: Fronde does not show them yet.

Without the chat, Fronde does not tell servers it takes forms, and cancels any form one sends
anyway.

## When a server is down

Fronde connects in the background and keeps trying, waiting 2 seconds at first and up to one
minute between attempts. A server that does not answer within 30 seconds counts as down. While it
is down, its tools answer that it is unreachable. When it comes back, its tools are listed again.
A stored token that is missing or stored for another origin is reported in the same way.
Check `fronde serve`'s log for `connected`, `disconnected` and `cannot start` lines.

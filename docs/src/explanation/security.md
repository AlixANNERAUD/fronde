# Security model

Fronde holds your memory, your calendar and a signed-in Claude subscription. It is built for one
owner, at home or on a server they control.

## Secrets

Secrets never appear in the configuration file, in logs or in tool output. The file only names
where each one is: `{ file = "…" }` or `{ env = "…" }`. The configuration can then be committed,
shared or mounted read-only. `[agent] env` is written in plain text, so it is for settings only;
the Claude token reaches the agent through Fronde's own environment.

A secret file must be mode 0600 and owned by the user running Fronde; otherwise Fronde refuses to
read it. Setting `FRONDE_ALLOW_SHARED_SECRET_FILES=1` lifts the check for setups like Docker
secrets, where the runtime sets the file's mode and owner.

## Personal data on disk

Fronde creates its data directory and every directory inside it with mode 0700, and every file it
writes (memory, conversation log, session id, search index, temporary files) with mode 0600,
whatever the umask. At start, a data directory that group or others could enter is tightened to
0700 and a warning is logged. Other users on the same machine cannot read your memory or
conversation. This does not encrypt anything: see below.

## Two doors

- **`/mcp`** needs the bearer token, compared in constant time. It is at least 32 characters, so
  it cannot be guessed. `/mcp` skips the `Host` check against DNS rebinding: a page that rebinds
  a name to Fronde still cannot send the token.
- **The chat** needs the password. A wrong one is answered after a second, and five in a row lock
  sign-in for a minute for everyone: there is a single password to guess, so the throttle is
  global. Passwords are compared as SHA-256 digests in constant time, so neither their content
  nor their length leaks through timing. Signing in issues a new session id, so an id planted
  before sign-in is worthless after it.

## The browser

The session cookie is `HttpOnly` and `SameSite=Lax`, and `Secure` behind https. Pages cannot be
framed by another site (`frame-ancestors 'none'`). Browsers do not apply the same-origin policy
to WebSockets, so `/ws` checks that the page's `Origin` matches the `Host` it was sent to: another
site cannot open your chat with your cookie. This is why a reverse proxy must forward `Host`.

## The agent

Fronde's agent has no shell and no files. Its session turns off Claude Code's built-in tools
except `WebSearch` and `WebFetch`, which only read the web, and allows only those and the hub's
tools. It loads none of Claude Code's settings, so your plugins, MCP servers, hooks and permission
rules on the same machine do not reach it. Its permission mode asks before acting, and any request
it makes shows in the chat with *Autoriser* and *Refuser*. Adding a built-in tool beyond the two
web ones is refused by the configuration check: widening it is a deliberate code change.

## Untrusted content

Text from the web, an email or a document can carry instructions written by someone else. Nothing
read from an external source is saved to memory automatically: the hub's instructions and
`memory_add`'s own description say to save only what you said or confirmed, and the system prompt
says web results are untrusted. A poisoned page can still mislead one answer; it should not
become a lasting memory.

## Wasm components

Components get no filesystem, sockets, environment or arguments. Their HTTP goes through a gate
that denies every origin not allowed for that component, does not follow redirects and does not
let the component set `Authorization` or `Cookie`. The host adds the credentials for an allowed
origin itself, so a component never holds a secret it could leak elsewhere. Each call has a
memory cap, a CPU budget and a timeout. Loading components from the configuration is **planned**;
see [Component interface](../reference/wit-interface.md).

## What is out of scope

- Several users: there is one password and one conversation.
- Encryption at rest: file modes keep other users out, but not someone with the disk. Protect the data directory with the machine's own means.
- TLS: put Fronde behind a reverse proxy for https. Built-in TLS is **planned** with federation.

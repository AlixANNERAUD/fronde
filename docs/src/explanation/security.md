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
let the component set `Authorization` or `Cookie`. An allowed name that resolves to a local
address (loopback, private network, link-local, cloud metadata) is refused too, unless that
origin is allowed as local: a public name cannot be pointed at the host, its network or its
cloud's metadata service. The host adds the credentials for an allowed origin itself, so a
component never holds a secret it could leak elsewhere. Each call has a memory cap, a CPU budget
and a timeout. What a component writes to its standard error goes to Fronde's log, as warnings
under its name, at most 100 lines a call.

A credential travels only over https, or over plain http to this machine (`localhost`,
`127.0.0.1`, `::1`). A CalDAV server on the local network served over plain http must move to
https, or Fronde must run on the same machine, for the component to send it a password.

A component asks and the configuration grants. Its manifest lists the origins and credentials it
wants; its `[[sources]]` entry may grant only those, and never more (an origin or a credential
the manifest does not ask for is refused). The grant is bound to the SHA-256 of the `.wasm` file
and of the manifest, written in the entry: after an update, the component is skipped until you
read the new manifest and copy the new hashes from `fronde check`. No tool can grant or approve
anything, so the agent cannot widen what a component reaches. See
[Configuration](../reference/configuration.md#kind--component).

## What is out of scope

- Several users: there is one password and one conversation.
- Encryption at rest: file modes keep other users out, but not someone with the disk. Protect the data directory with the machine's own means.
- TLS: put Fronde behind a reverse proxy for https. Built-in TLS is **planned** with federation.

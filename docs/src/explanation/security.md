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

## Secrets in memory

Fronde holds the secrets it resolves (the `/mcp` bearer token, the web chat password, upstream MCP
tokens and component credentials) as `secrecy::SecretString` values. They print as redacted with
`Debug` and their memory is zeroed when they are dropped. The plain value is read only at a few
places in the code, each listed in a test (`crates/server/tests/secrets.rs`): the checks of the
token's length and of the password, the constant-time compare of `/mcp` tokens and sign-in
passwords (on SHA-256 digests, so the length does not leak either), the component gate, the ACP
header and the upstream MCP client.

A component's `Authorization` header is built by the gate for each request, marked sensitive (its
`Debug` prints `Sensitive`) and dropped with the request; between requests only the
`SecretString` is kept.

Some plain copies remain:

- An upstream MCP token is handed to the MCP client library as a `String`, which keeps it for the
  life of the connection.
- The `Authorization` header Fronde gives its agent for `/mcp` is a plain string in the ACP
  request, and the agent process holds the token.
- A `{ env = "…" }` value stays in Fronde's environment, and the agent inherits that environment
  for now (an explicit agent environment is planned).
- A sign-in form's body, and each `/mcp` request's `Authorization` header, are read into buffers
  that are freed but not zeroed. The parsed password is zeroed after the compare.

## Core dumps and other processes

Zeroing on drop cannot help if a crash writes a core dump, which holds the whole address space, or
if another process reads Fronde's memory. On Linux, `fronde serve` makes itself non-dumpable
(`PR_SET_DUMPABLE` 0) and sets `RLIMIT_CORE` to 0 before it reads any secret. Non-dumpable also
stops processes of the same user, the agent and the tools it runs among them, from reading
`/proc/<pid>/environ` and `/proc/<pid>/mem` or attaching to Fronde with ptrace. Root, or a process
with `CAP_SYS_PTRACE`, still can. To debug a crash, set `FRONDE_ALLOW_CORE_DUMPS=1`: Fronde then
leaves both settings as they were. On other systems it does nothing.

As belt and braces, disable core dumps outside Fronde too: `fronde service install` writes
`LimitCORE=0` in its systemd unit, and in Docker, run the container with `--ulimit core=0`
(`ulimits: { core: 0 }` in Compose). Elsewhere, `ulimit -c 0` before starting Fronde.

## Personal data on disk

Fronde creates its data directory and every directory inside it with mode 0700, and every file it
writes (memory, the database `fronde.db`, which holds the conversation, the agent's session id
and the web sessions, its `-wal` and `-shm` files, temporary files) with mode 0600, whatever the
umask. At start, a data directory that group or others could enter is tightened to
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

The session cookie is `HttpOnly` and `SameSite=Lax`, and `Secure` behind https. Sessions live in
`fronde.db`, so a restart signs no one out. Their ids are stored as they are: anyone who can read
that file can take over a signed-in browser, one more reason it is 0600. Fronde never logs a
session id. Signing out deletes the session at once. A session is refused as soon as it expires,
after 30 days without use, and its row is deleted within the hour. Only a successful sign-in
stores a session: a cookie a client made up is never saved.

Pages cannot be framed by another site (`frame-ancestors 'none'`). Browsers do not apply the
same-origin policy to WebSockets, so `/ws` checks that the page's `Origin` matches the `Host` it
was sent to: another site cannot open your chat with your cookie. This is why a reverse proxy must
forward `Host`.

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
address (loopback, private network, shared 100.64.0.0/10, link-local, cloud metadata, or any of
those carried in IPv6) is refused too, unless that
origin is allowed as local: a public name cannot be pointed at the host, its network or its
cloud's metadata service. Origins are compared after the URL standard normalises them, the same
way for the configuration and for each request: `http://0x7f.1`, `http://127.0.0.1:80` and
`http://127.0.0.1` are one origin, as are `[0:0:0:0:0:0:0:1]` and `[::1]`. The connection goes
to the host the gate matched, and the `Host` header names it, not the request's spelling of it.
A request with a port that is not a number from 0 to 65535 has no origin and is denied. The host
adds the credentials for an allowed origin itself, so a component never holds a secret it could leak elsewhere. Each call has a memory cap, a CPU budget
and a timeout. What a component writes to its standard error goes to Fronde's log, as warnings
under its name, at most 100 lines a call.

A credential travels only over https, or over plain http to this machine (`localhost` exactly, a
loopback address such as `127.0.0.1` or `::1`, or a loopback written as IPv4-mapped IPv6,
`::ffff:127.0.0.1`). `localhost.`, with the final dot, is not this machine: the system's resolver
may send that name to DNS, which can answer with any address. The unspecified address (`0.0.0.0`,
`::`) is not loopback either. After resolving the name, Fronde checks again: a credential over
plain http goes only when every address the name resolves to is a loopback one, and the request
is refused otherwise. A CalDAV server on the local network served over plain http must move to
https, or Fronde must run on the same machine, for the component to send it a password. In
Docker, the same machine means the container: a CalDAV server on the Docker host reached over
plain http (`host.docker.internal`) does not get credentials, so use https there.

A component asks and the configuration grants. Its manifest lists the origins and credentials it
wants; its `[[sources]]` entry may grant only those, and never more (an origin or a credential
the manifest does not ask for is refused). The grant is bound to the SHA-256 of the `.wasm` file
and of the manifest. The `fronde` binary carries the hashes of the components built with it
from this repository: an entry without hashes is approved only when its files are exactly those.
Any other component needs both hashes written in its entry, which also pin a shipped one: after
an update, the component is skipped until you read the new manifest and copy the new hashes from
`fronde check`. An entry without hashes may also be approved by an approval stored in
`fronde.db`, which names the source and both hashes: it approves only those files for that
entry, and never overrides hashes written in an entry. Only the owner will store one, from the
chat (planned); no tool writes them. A path never approves anything, and nothing in Fronde loads a component
without going through that check: the loader takes only what the check returns, and the file is
hashed again as it is compiled. No tool can grant or approve anything, so the
agent cannot widen what a component reaches.

A component's tools start with the prefixes its manifest declares, which no other source may
use: a component cannot shadow the hub's tools or another source's. See
[Configuration](../reference/configuration.md#kind--component).

## What is out of scope

- Several users: there is one password and one conversation.
- Encryption at rest: file modes keep other users out, but not someone with the disk. Protect the data directory with the machine's own means.
- TLS: put Fronde behind a reverse proxy for https. Built-in TLS is **planned** with federation.

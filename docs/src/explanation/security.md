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

## Stored secrets

`fronde secret set` keeps a secret in `fronde.db`, encrypted, so that it needs no file of its own
(see [Store a secret](../how-to/store-a-secret.md)). Each value is sealed with
XChaCha20-Poly1305 under a master key of 256 bits and a random 24-byte nonce drawn for every
write. The seal also covers, without encrypting them, the row's format version, its scope, its
name and the canonical origin it is for: a row edited, or moved to another name or origin,
fails to open instead of handing its secret to another place. With the first secret, Fronde
stores a key check, a fixed text sealed with the same key under a label of its own, so that it
never passes for a secret: a wrong key is then reported as a wrong key, and Fronde never makes a
new key while secrets are stored, since it could not open them. The value is read from standard
input, never from the command line, where other users could see it in the process list, and
never from a terminal, which would echo it; a value given as an argument is refused without
being shown. Decrypted values are held in buffers zeroed when dropped, and no command, log or
tool output shows one.

A stored secret is used only as a component's credential, `{ stored = "name" }` in
`[[sources.credentials]]`; the configuration refuses it anywhere else. Rows are read still sealed,
and a value is opened only when a component that names it is prepared, just before it loads, and
only when the origin the secret was stored for is the credential's origin, both normalised. A
secret stored for one origin is never sent to another, even when the configuration names it for
one: the component is refused with the command that stores it for the right origin. The opened
value goes to the component's gate, which adds it to requests for that origin; the component
never sees it. The rows are read again each time a component starts, so a secret stored while
Fronde runs reaches a component waiting for it, which Fronde tries again until it is there.
Fronde checks the master key when it starts: a key that did not seal the stored secrets, or no
key file while some are stored, stops it.

The master key is one line, `fronde-key-v1:` followed by the key in base64url, in a file of mode
0600 created with the first secret: `master.key` beside the configuration file, or the file
`[secrets] master_key` names. It is kept out of the data directory on purpose. Fronde reads it
only from a plain file it owns, never through a symbolic link, and never from an environment
variable: the agent inherits Fronde's environment. Earlier versions kept it in the data
directory; Fronde moves it from there at the next start or `fronde secret` command, and stops
if both places hold different keys. Keeping it in the system's keychain or in a systemd
credential is **planned**.

What this protects against, today: a backup of the data directory, or a snapshot of the volume
that holds it, contains `fronde.db` but not the key, so it reveals no stored secret; and nothing
in the database's files, its write-ahead log included, holds a value in clear. What it does not
protect against yet: whoever can read both the key file and the data directory has the secrets,
and that includes any process running as Fronde's user, the agent among them. Until the key
moves to a keychain, treat a copy that holds both, such as a backup of the whole home directory,
as holding the secrets themselves.

## Secrets in memory

Fronde holds the secrets it resolves (the `/mcp` bearer token, the web chat password, upstream MCP
tokens and component credentials, stored ones included once opened) as `secrecy::SecretString`
values. They print as redacted with
`Debug` and their memory is zeroed when they are dropped. The plain value is read only at a few
places in the code, each listed in a test (`crates/server/tests/secrets.rs`): the checks of the
token's length and of the password, the constant-time compare of `/mcp` tokens and sign-in
passwords (on SHA-256 digests, so the length does not leak either), the component gate, the ACP
header and the upstream MCP client.

A component's `Authorization` header is built by the gate for each request, marked sensitive (its
`Debug` prints `Sensitive`) and dropped with the request; between requests only the
`SecretString` is kept.

The master key is read when a component that names a stored secret starts, used to open that
secret and dropped (zeroed) once the component is prepared; nothing kept for the whole run holds
it. `fronde serve` also reads it once at start, to check it, and drops it.

Some plain copies remain:

- An upstream MCP token is handed to the MCP client library as a `String`, which keeps it for the
  life of the connection.
- The `Authorization` header Fronde gives its agent for `/mcp` is a plain string in the ACP
  request, and the agent process holds the token. That token is the assistant's own, not
  `server.token`: it is made at each start, never written anywhere, and dies with the process.
- A `{ env = "…" }` value stays in Fronde's environment, and the agent inherits that environment
  for now (an explicit agent environment is planned).
- A sign-in form's body, and each `/mcp` request's `Authorization` header, are read into buffers
  that are freed but not zeroed. The parsed password is zeroed after the compare.

## Core dumps and other processes

Zeroing on drop cannot help if a crash writes a core dump, which holds the whole address space, or
if another process reads Fronde's memory. On Linux, `fronde serve` makes itself non-dumpable
(`PR_SET_DUMPABLE` 0) and sets `RLIMIT_CORE` to 0 before it reads any secret, and so do the
`fronde secret` commands. Non-dumpable also
stops processes of the same user, the agent and the tools it runs among them, from reading
`/proc/<pid>/environ` and `/proc/<pid>/mem` or attaching to Fronde with ptrace. Root, or a process
with `CAP_SYS_PTRACE`, still can. To debug a crash, set `FRONDE_ALLOW_CORE_DUMPS=1`: Fronde then
leaves both settings as they were. On other systems it does nothing.

As belt and braces, disable core dumps outside Fronde too: `fronde service install` writes
`LimitCORE=0` in its systemd unit, and in Docker, run the container with `--ulimit core=0`
(`ulimits: { core: 0 }` in Compose). Elsewhere, `ulimit -c 0` before starting Fronde.

## Personal data on disk

Fronde creates its data directory and every directory inside it with mode 0700, and every file it
writes (memory, the database `fronde.db`, which holds the conversation, the agent's session id,
the web sessions and the stored secrets, its `-wal` and `-shm` files, temporary files, and the
master key beside the configuration) with mode 0600, whatever the umask. At start, a data
directory that group or others could enter is tightened to 0700 and a warning is logged. Other
users on the same machine cannot read your memory or conversation. Only stored secrets are
encrypted: see above, and below.

## Two doors

- **`/mcp`** needs a bearer token, compared in constant time: `server.token`, at least 32
  characters, for outside clients, or the assistant's own random token. Which one a call carries
  tells the hub who asks, so a card an outside client opens is labelled with that client's name
  and is never mistaken for Fronde's own question. `/mcp` skips the `Host` check against DNS
  rebinding: a page that rebinds a name to Fronde still cannot send a token.
- **The chat** needs the password. A wrong one is answered after a second, and five in a row lock
  sign-in for a minute for everyone: there is a single password to guess, so the throttle is
  global. Passwords are compared as SHA-256 digests in constant time, so neither their content
  nor their length leaks through timing. Signing in issues a new session id, so an id planted
  before sign-in is worthless after it.

## The browser

The session cookie is `HttpOnly` and `SameSite=Lax`, and `Secure` behind https. Sessions live in
`fronde.db`, so a restart signs no one out. Their ids are stored hashed (SHA-256): a copy of that
file, a backup for instance, does not let anyone take over a signed-in browser. Fronde never logs
a session id. Signing out deletes the session at once. A session is refused as soon as it expires,
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
those carried in IPv6) is refused too, unless that origin is allowed as local: a public name
cannot be pointed at the host, its network or its cloud's metadata service. Origins are compared
after the URL standard normalises them, the same way for the configuration and for each request:
`http://0x7f.1`, `http://127.0.0.1:80` and `http://127.0.0.1` are one origin, as are
`[0:0:0:0:0:0:0:1]` and `[::1]`. The connection goes to the host the gate matched, and the `Host`
header names it, not the request's spelling of it. A request with a port that is not a number
from 0 to 65535 has no origin and is denied. The host adds the credentials for an allowed origin
itself, so a component never holds a secret it could leak elsewhere. Each call has a memory cap,
a CPU budget and a timeout. What a component writes to its standard error goes to Fronde's log,
as warnings under its name, at most 100 lines a call.

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
wants; its `[[sources]]` entry may grant only those, and never more (an origin or a credential the
manifest does not ask for is refused). The grant is bound to the SHA-256 of the `.wasm` file and
of the manifest. The `fronde` binary carries the hashes of the components built with it from this
repository: an entry without hashes is approved only when its files are exactly those. Any other
component needs both hashes written in its entry, which also pin a shipped one: after an update,
the component is skipped until you read the new manifest and copy the new hashes from
`fronde check`. An entry without hashes may also be approved by an approval stored in
`fronde.db`, which names the source and both hashes: it approves only those files for that
entry, and never overrides hashes written in an entry. Only the owner will store one, from the
chat (planned); no tool writes them. A path never approves anything, and nothing in Fronde loads
a component without going through that check: the loader takes only what the check returns, and
the file is hashed again as it is compiled. No tool can grant or approve anything.

This makes `fronde.db` an approval authority, and a weaker one than the configuration. File
modes keep other users out, but any process running as Fronde's user can write the database: the
agent and the tools it runs included. Such a process can store an approval for files of its
choice under an entry without hashes. Hashes pinned in the entry close that door, since they
always win over stored approvals: pin every component in a `fronde.toml` that Fronde's user
cannot write (owned by another user, or mounted read-only), and approval stays out of the agent's
reach. Confining the agent away from the data directory, so that it cannot write `fronde.db` at
all, is **planned**.

A component's tools start with the prefixes its manifest declares, which no other source may
use: a component cannot shadow the hub's tools or another source's. See
[Configuration](../reference/configuration.md#kind--component).

## Python sandbox

Agents run Python for calculations in CPython 3.15 built for WASI preview 1, inside the same
wasmtime engine as components but with a linker of its own that holds WASI preview 1 and nothing
else. A module that imports anything more, sockets or HTTP among them, cannot be instantiated.
Each run gets a fresh instance, so nothing carries over from one run to the next: no globals, no
imported modules, no files.

What a run is granted:

- its arguments, which are fixed: the interpreter in isolated mode and Fronde's own runner;
- standard input, which carries the code, the data and the user's time zone as JSON;
- standard output, to a buffer in memory of at most 64 KiB, which carries the reply;
- standard error, to Fronde's log as warnings under `python`, at most 100 lines a run. The socket
  functions of WASI preview 1, which would each log a warning, are replaced by ones that fail
  silently: no socket exists here;
- the clocks and random numbers;
- one directory, the standard library, read-only at `/lib`.

It gets no environment variables, no other directory, no network and no way to start a process
or a thread. There is no `/tmp`: nothing a calculation needs is worth a writable directory, which
would be one more thing to clean up and to bound. Prints are captured in memory by the runner.

Each run has its limits, which end that run with an error that says what to change and leave the
hub and the next run untouched:

| Limit | Value | Why |
| --- | --- | --- |
| Memory | 256 MiB | CPython reserves 40 MiB at start; loading and grouping 100 000 rows of JSON needs about 56 MiB in all. Only the pages touched are used. |
| CPU | about 10 s of running time | Counted as for components; time spent waiting is free. Python under WASI is about four times slower than native. |
| Wall clock | 30 s, waiting for a turn included | A run cannot hold a tool call open longer, even while sleeping: every wait (`time.sleep`, `select`, `poll`) is a timer on Fronde's async runtime, never a blocked thread, so the timeout cancels it. Only lookups and reads of the standard library run on the calling thread, which keeps imports fast; the stock WASI would also sleep there when a lone relative timeout is asked for, so that setting covers the preopened directory alone. A test runs `select` and `poll` with a 60 s timeout on a runtime with a single thread and checks that both stop at the timeout while another run finishes. |
| Runs at once | 2 | Each may use 256 MiB and a core; a third waits for a turn within its 30 s. |
| Code and data | 16 MiB | The JSON sent in is refused above it, before anything runs. |
| Open files and streams | 128 at once | Imports open one file at a time; a run over the cap is stopped. |
| Output | 64 KiB | Results and prints go back to the agent's context: aggregates, not rows. Prints keep their first 16 KiB. |

Fronde runs only the CPython build it pins: the SHA-256 of `python.wasm` is written in Fronde's
source, and the file is hashed and compiled from the same bytes, at start. Any other file is
refused with its hash. The standard library is not hashed. It only ever runs inside the sandbox,
so changing it gains nothing the code sent to the tool could not already do, and changing it
needs write access to the installation, which would also allow replacing the `fronde` binary.

## What is out of scope

- Several users: there is one password and one conversation.
- Encryption at rest, beyond stored secrets: file modes keep other users out, but not someone
  with the disk, who finds the memory, the conversation and, unless it is kept on another disk,
  the master key that opens the stored secrets. Protect the data directory with the machine's
  own means.
- TLS: put Fronde behind a reverse proxy for https. Built-in TLS is **planned** with federation.

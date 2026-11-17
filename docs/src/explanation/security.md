# Security model

Fronde holds your memory, your calendar and a signed-in Claude subscription. It is built for one
owner, at home or on a server they control.

## Secrets

Secrets never appear in the configuration file, in logs or in tool output. The file only names
where each one is: `{ file = "…" }` or `{ env = "…" }`. The configuration can then be committed,
shared or mounted read-only. `[agent] env` is written in plain text, so it is for settings only;
the Claude token is `[agent] oauth_token`, a secret like the others, which Fronde gives the agent
as `CLAUDE_CODE_OAUTH_TOKEN` (see [The agent's environment](#the-agents-environment)).

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
Fronde checks the master key when it starts: a key that did not seal the stored secrets, no key
while some are stored, or a backend holding it that cannot be read, stops it.

The master key is one line, `fronde-key-v1:` followed by the key in base64url, the same in each
place it can be kept (`[secrets] backend`, see
[Configuration](../reference/configuration.md#secrets-1)):

- A **systemd credential**, `fronde-master-key`, encrypted by `systemd-creds` with a key of the
  machine (and its TPM when it has one) and decrypted by systemd, as the service starts, into its
  `$CREDENTIALS_DIRECTORY`, which lives in memory that is never swapped out and exists while the
  service runs. Fronde reads that file; it runs no `systemd-creds` and writes nothing there. On
  disk, only the encrypted credential exists.
- The **keychain** of the desktop session: Secret Service (spoken over D-Bus in Rust, with no
  libdbus and no OpenSSL), the macOS Keychain or the Windows Credential Manager. The keychain
  encrypts it at rest with the login password. Each call runs on a thread of its own, one at a
  time, under a 2-second limit: a keychain waiting on an unlock prompt fails the call with an
  error after 2 seconds, the next call waits for that one instead of opening another prompt, and
  Fronde exits without waiting for it, within 3 seconds.
- A **file** of mode 0600: `master.key` beside the configuration file, or the file
  `[secrets] master_key` names. It is kept out of the data directory on purpose. Fronde reads it
  only from a plain file it owns, never through a symbolic link. Earlier versions kept it in the
  data directory; Fronde moves it from there at the next start or `fronde secret` command, and
  stops if both places hold different keys.

The key never comes from an environment variable, which the processes Fronde starts may inherit
(an upstream MCP server's command does).

With `backend = "auto"`, the default, the first key goes to a systemd credential when the service
has one, since only an operator puts one there; else to the keychain; else, when there is no
keychain or it cannot be reached, to a file. A keychain that is there but locked, or that waits
on an unlock prompt, stops the first secret with an error that says to unlock it and try again:
unlocked later, it would hold no key while a file held one. The database records where the key
went, and for the keychain the entry's name, beside the key check, in the transaction that seals
the first secret, and Fronde uses that place alone from then on: a data directory moved elsewhere
still finds its keychain entry. This choice is sticky on purpose: a keychain that is locked at
boot must stop Fronde with what to do, not make it mint a key file whose key opens nothing, which
would strand every stored secret. Three rules back it: Fronde makes a key only when nothing is
sealed; two first secrets stored at once, by two commands, take turns on a lock file in the data
directory (`master-key.lock`), so that the keychain, which replaces an entry it writes, never ends
up holding another key than the one that sealed; and every write checks, inside its transaction,
that its key opens the key check.

What this protects against: a backup of the data directory, or a snapshot of the volume that
holds it, contains `fronde.db` but not the key, so it reveals no stored secret; and nothing in the
database's files, its write-ahead log included, holds a value in clear.

What it does not protect against: code running as Fronde's user. Against it, the three backends
give the same protection, which is none:

- it can read a key file;
- it can ask the unlocked keychain for the entry: on Linux, Secret Service has no access control
  per application, and any process of the session reads any unlocked item
  (`secret-tool lookup service fronde username master-key@…`);
- for a user service, it can read
  `$XDG_RUNTIME_DIR/credentials/fronde.service/fronde-master-key` while the service runs, and it
  can decrypt the `.cred` file itself with `systemd-creds --user decrypt`, since a credential
  encrypted with `--user` is bound to that user, not to the service.

The agent and the tools it runs are such code. Its sandbox (see
[Where the agent runs](#where-the-agent-runs)) keeps it from reading the key file, the data
directory and systemd's credentials directly, and it cannot open a Unix socket, so it cannot ask
the keychain over D-Bus, run `systemd-creds --user decrypt`, or have the user manager run
something for it (`systemd-run --user`) either. Any other process of your user can, and root can
read all three as well.

What each backend adds beyond the file:

- the systemd credential: its `.cred` file opens only on this machine, so a copy of it taken
  elsewhere is useless; the decrypted key lives in memory that is never swapped out; and it is
  there at boot, before anyone signs in;
- the keychain: the key is encrypted at rest under the login password, so a copy of the home
  directory holds only that encrypted form; it needs a desktop session that unlocked it.

With the file backend, treat a copy that holds both the key file and the data directory, such as a
backup of the whole home directory, as holding the secrets themselves.

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
- A `{ env = "…" }` value stays in Fronde's environment. The agent never gets it (see
  [The agent's environment](#the-agents-environment)), but an upstream MCP server started from a
  `command` inherits Fronde's whole environment.
- A sign-in form's body, and each `/mcp` request's `Authorization` header, are read into buffers
  that are freed but not zeroed. The parsed password is zeroed after the compare.
- A master key read from the keychain passes through libraries Fronde does not control, which
  free their copies without zeroing them: on Linux, the Secret Service client's buffers for the
  decrypted secret and zbus's D-Bus message buffers. An entry that is not text comes back as
  bytes inside keyring-core's `BadEncoding` error: Fronde zeroes the bytes it gets, not the copies
  made before.
- A secret typed in a card crosses the WebSocket library's read buffer, which is not zeroed.
  Fronde wipes the message it is handed once read, and the value it parses becomes a
  `SecretString` at once. In the browser, the page empties the box once the secret is sent, but
  the browser's own copies (the input's history, the outgoing message) are its own.

## Secrets given in a card

`user_secret` asks for a secret on a card; see [Requests and cards](requests.md#secret-cards).
The value goes from the page to the store and nowhere else: not to the agent, which gets a
reference or a name; not to the conversation stream or any message the pages get; not to the
request's row, which keeps where it went; not to the log. Tests check each of these with known
values: the stream, the rows, every file of the data directory (`fronde.db`, its `-wal` and
`-shm`), the tool's result and the log captured at TRACE from every library. The WebSocket
library writes every message it receives at TRACE, so Fronde's log filter turns its targets
(`tungstenite`, `tokio_tungstenite`) off, whatever `RUST_LOG` says.

A secret kept **for this conversation** lives in Fronde's memory only: never on disk, gone at the
next restart. Its reference, `sr_` and 128 random bits, works for the asker that got it, in that
conversation, once, for one hour (at most a day); every refusal gives the same message, so a
probe learns nothing. Expired ones are wiped at the next use of the store and every hour, all of
them when Fronde stops. No tool takes a reference yet.

A secret **kept for an origin** is sealed like one stored with `fronde secret set`, and only an
admin can choose that: the box is shown to admins only, and the server refuses it from anyone
else. The agent proposes the name and origin, and the card shows the origin. A name already
stored for another origin is refused: a card cannot move a stored secret to another origin. The
first secret kept this way makes the master key, as `fronde secret set` does, under the same
lock, and the closed card and the log say where, to be backed up.

The card's link button takes an `https://` address only, with no user name or password before
the host, and names the host it opens: the agent's text never becomes a link otherwise.

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
  tells the hub who asks, so a card an outside client opens is labelled with that client's name,
  quoted and marked, as in *Question de « Codex » (client MCP)*, and is never mistaken for
  Fronde's own question. A client chooses its name, so the hub asks nothing for one named
  *Fronde* or *assistant*, one with a name over 40 characters, or one whose name holds a control
  character; a name is never what makes a caller the assistant, only its token is. `/mcp` skips the `Host` check against DNS
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
it makes is a permission card in the chat (see
[Requests and cards](requests.md#permission-cards)): only the user it is for answers it, and only
an admin is offered an option that lasts, and nobody one that switches the agent's mode (out of
plan mode into a mode that asks less); a card left unanswered refuses after 30 minutes, and
*Stop* or the end of the turn cancels it. The card's texts come from the agent, so they are capped
and stripped of control and bidirectional characters, which could otherwise make a command read
as another. *Toujours autoriser* lasts only as long as the agent's process: Fronde keeps no grant,
and the agent loads no settings where one could persist. Adding a built-in tool beyond the two
web ones is refused by the configuration check: widening it is a deliberate code change.

### The agent's environment

Fronde does not start the agent directly: it starts its own binary as `fronde __confine`, which
prepares the process and then replaces itself with the agent, keeping the same process, pipes and
process group. The agent does not inherit Fronde's environment. It keeps only an allow list:
`PATH`, the locale and `TZ`, `CLAUDE_*` and `ANTHROPIC_*` (where the Claude token is), the proxy
and CA certificate variables, and the names `[agent] env` sets. Fronde sets `HOME` to the agent's
own home, `<data>/home`, and `TMPDIR` to `<data>/home/tmp`. Every other variable is dropped,
among them `XDG_*` and `DBUS_*`, which point at your session's directories and buses,
`CREDENTIALS_DIRECTORY`, where systemd puts the service's credentials, `SSH_AUTH_SOCK`, and every
`{ env = "…" }` secret's variable, even one whose name the allow list matches. The helper reads
the values from its environment; its command line, which any user can read, carries names and
paths only.

Two secrets reach the agent, both through its environment, which only your user can read:

- `[agent] oauth_token`, its Claude sign-in, as `CLAUDE_CODE_OAUTH_TOKEN`, read again at each
  start;
- the token it calls the hub's `/mcp` with, made at each start, as `FRONDE_HUB_TOKEN`. The ACP
  adapter hands the session's MCP servers to Claude Code on its command line, which every user
  of the machine can read in `/proc`, so the header Fronde sends holds only
  `Bearer ${FRONDE_HUB_TOKEN}`, which Claude Code expands from its environment.

The agent has its own home rather than yours, so that it never reads or writes your
`~/.claude`: a hook it wrote there would run in your own terminal. For the same reason, never run
`claude` yourself with `HOME` set to the agent's home, not even to sign it in: the agent can
write that home, and a hook, an `apiKeyHelper` or a `statusLine` it put in
`.claude/settings.json`, or an MCP server it added to `.claude.json`, would run with your rights,
outside its sandbox. The agent signs in with a token instead, which you make with
`claude setup-token` in your own home (see
[Sign Fronde in to Claude](../how-to/sign-in-to-claude.md)). The helper also
raises the agent's OOM score to the maximum and lowers its priority, so that the kernel kills the
agent first when memory runs out, and Fronde comes first for the CPU.

### Where the agent runs

Before it starts the agent, the helper confines itself with Landlock, a Linux security module
that any user can apply to their own processes, and then with a seccomp filter, which refuses
some system calls. The agent and every process it starts inherit both and cannot lift them. No
external program, no root and no user namespace are needed, so it works the same natively and
in Docker.

The agent may:

- read and run files under the system's directories (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib32`,
  `/lib64`, `/etc`, `/proc`, `/sys`), each directory on its `PATH`, the adapter's install
  (the topmost `node_modules` above the program, or else its directory), and `[agent] read`;
- read `/dev/zero`, `/dev/random` and `/dev/urandom`, and write `/dev/null` and `/dev/full`: no
  other device, so no terminal (`/dev/tty`, `/dev/pts`) and no shared memory (`/dev/shm`);
- read, write, create and remove files in `<data>/agent`, its working directory, and
  `<data>/home`, its home;
- connect over TCP to ports 443 (https, Claude's API and `WebFetch`), 53 (DNS over TCP), the
  hub's own port, for `/mcp`, and the port of an `HTTPS_PROXY` or `HTTP_PROXY` it is given. It may
  not listen on any TCP port;
- open IPv4, IPv6 and netlink route sockets (read-only for your user: glibc reads the network
  interfaces through them when it resolves a name), and connected pairs of Unix stream sockets,
  which reach nothing but each other and carry the standard streams of the processes Node
  starts.

Everything else on disk is out of its reach. It cannot read or write `fronde.db` (with its `-wal`
and `-shm`), the memory files, anything else in the data directory, the configuration's directory
(`fronde.toml`, `master.key`, a systemd `.cred` file), any `{ file = "…" }` secret, systemd's
credentials (`$CREDENTIALS_DIRECTORY`, `/run/credentials`, `/run/user/<uid>/credentials`), or
your own files (`~/.ssh`, `~/.claude` and the rest of your home) outside what the grants above
name. Every path is resolved before it is granted, so a symbolic link cannot smuggle one in; a
bind mount or a hard link can, since neither is told apart from the place it shows: do not give
the agent's directories either. Before it starts the agent, Fronde refuses any grant that equals
or contains one of these: `[agent] read` naming the data directory is an error, a configuration
under `/etc` is an error, and a `PATH` entry that holds the data directory is left out with a
warning.

Your home is guarded the same way for the `PATH` and the adapter's install: a `PATH` entry that
equals or holds your home is left out with a warning, and an adapter installed in a directory
that holds it is an error. A directory inside your home can still be granted: an entry of the
`PATH` such as `~/.local/bin`, the adapter's `node_modules` in a checkout, or `[agent] read`.
Fronde then logs a warning at every start that names it, since the agent can read everything in
it: keep secrets out of those directories.

The agent cannot ptrace Fronde or read its `/proc/<pid>/environ` and `mem`, and, from ABI 6, it
cannot send Fronde a signal. It cannot gain privileges either: the helper sets `no_new_privs`, so
a set-user-ID program runs without its privileges, and the agent does not start if the kernel
refused it. Landlock only adds limits: the agent still runs as your user, under the usual file
permissions.

Fronde sets every variable of its environment the agent does not get to an empty value before it
starts the helper, so a `{ env = "…" }` secret's value never reaches it, and the helper makes
itself non-dumpable before it reads its environment, so that other processes of your user cannot
read it either. The agent's standard streams are pipes; when Fronde runs in a terminal, the agent
still has it as its controlling terminal, but cannot open it (`/dev/tty` is not granted) and holds
no descriptor to it, and the seccomp filter refuses `TIOCSTI` and `TIOCLINUX`, the `ioctl`s that
would type into it.

### No Unix sockets

Landlock checks a Unix socket reached by path only from ABI 9 (Linux 7.1). Before that, a
confined agent could still talk to whatever listens on a socket it can name, and some of those
would act for it outside the sandbox. So the seccomp filter makes `socket()` fail with
`EACCES` (permission denied) for every family but IPv4, IPv6 and netlink route sockets: no Unix
socket, by path or abstract, no raw packet, vsock (which reaches the host from a virtual
machine) or other netlink socket. It refuses a pair of Unix datagram sockets too, which could
still send to any datagram socket by path, and io_uring, which can create sockets without the
`socket()` system call. A socket can only come from these calls, and the agent inherits none: its
standard streams are pipes, and Fronde opens everything else close-on-exec. This closes:

- the systemd user manager: `systemd-run --user` would run any command unconfined, able to read
  `fronde.db` or the configuration;
- `systemd-creds --user decrypt`, which would decrypt a credential encrypted for your user, the
  master key among them;
- your session's D-Bus and, through it, the Secret Service keychain, which hands any unlocked
  item, the master key among them, to any process of your session;
- ssh-agent, gnupg's agent, and the Docker socket if your user may use it (which is root).

It costs Claude Code's cross-session messaging, which talks over a Unix socket in its temporary
directory and which Fronde does not use: Claude Code carries on without it. The filter covers
x86_64 and aarch64; a 32-bit x86 program, whose socket calls go through `socketcall`, is killed
at its first system call instead.

### What the kernel enforces

What the kernel can enforce depends on its Landlock version (its ABI). Fronde needs ABI 3
(Linux 6.2) for files: before it, Landlock cannot stop `truncate`, and the agent could empty
`fronde.db` or `fronde.toml` without opening them. It also needs seccomp filters, which every
kernel since Linux 3.17 has. The rest is applied when the kernel has it, and missing otherwise:

- TCP ports need ABI 4 (Linux 6.7): without it, the agent may connect to any TCP port;
- device `ioctl`s need ABI 5 (Linux 6.10): without it, the agent may send any `ioctl` to the
  devices it may open;
- signals and abstract Unix sockets need ABI 6 (Linux 6.12): without it, the agent may send
  Fronde a signal, and kill it (seccomp still refuses abstract sockets).

`fronde check` and the log at start say which apply and which are missing, for example
`agent sandbox: Landlock ABI 6 (files, TCP ports 443 53 8484, device ioctls, signals and abstract
sockets); seccomp: no Unix sockets but connected pairs, only IPv4, IPv6 and netlink route sockets,
no io_uring`.

With `[agent] sandbox = true`, the default, the agent does not start on a kernel without Landlock,
with ABI 1 or 2, or where the seccomp filter cannot be installed: the chat answers *L'agent a
rencontré une erreur*, and the log says why and how to fix it. Debian 12's kernel (6.1) has ABI 2,
so there the agent needs a newer kernel, such as the one from `bookworm-backports`, or
`[agent] sandbox = false`.
`[agent] sandbox = false` runs the agent unconfined, with the explicit environment only, and
Fronde logs a warning at every start. There is no silent fallback.

What the sandbox does not cover:

- **UDP.** Landlock does not restrict it before ABI 10: the agent may send DNS queries, or any
  other UDP, to any host.
- **Hosts.** TCP is limited by port, not by host: `WebFetch` needs the open web on 443, so the
  agent can reach any https server.
- **Memory and CPU.** Limits for the agent's processes are **planned (C4)**; for now only its OOM
  score and priority favour Fronde.

## Requests

A card is answered only by the user it is for, from a signed-in page, and the answer is checked
against the card on the server: options the card did not offer, a second pick on a question with
one answer, or an answer to a closed request are refused, and the request does not change. The
refusal goes to that page only and is never logged; a message that does not parse is logged with
its position only, never its content. A question card is not for secrets: `user_ask` tells agents
never to ask for a password or key, and its answers are kept in `fronde.db` as an audit trail.
Secrets have their own card, `user_secret` (see above).
Cards show agents' text as text, never as HTML. See [Requests and cards](requests.md).

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
reach. The agent's sandbox keeps it from opening `fronde.db` itself, and from having the systemd
user manager write it; any other process of your user can still write it, so pin the hashes
anyway.

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
  with the disk, who finds the memory, the conversation and, with the file backend and unless it
  is kept on another disk, the master key that opens the stored secrets. Protect the data
  directory with the machine's own means.
- TLS: put Fronde behind a reverse proxy for https. Built-in TLS is **planned** with federation.

# Configuration

Fronde reads one TOML file, given with `--config`. Unknown sections and keys are errors, so a typo
never goes unnoticed. The file is read at start: restart Fronde after a change.
`fronde check --config <file>` validates it and its secrets. The repository's
[`fronde.example.toml`](https://github.com/AlixANNERAUD/fronde/blob/main/fronde.example.toml)
shows every section.

## Relative paths

A relative path in the file (`[data] dir`, a secret `file`, a component's `path`, `[python] dir`,
`[secrets] master_key`) is resolved from the directory
containing the configuration file, whatever directory Fronde is started from, and becomes
absolute. Earlier versions read relative secret paths from the working directory.

## Secrets

A secret is never written in the file. It is one of:

| Form | Meaning |
| --- | --- |
| `{ file = "/run/secrets/name" }` | The file's content, without its trailing line breaks. A relative path is read from the directory of the configuration file, not from Fronde's working directory. |
| `{ env = "NAME" }` | The value of the environment variable `NAME`. |
| `{ stored = "name" }` | The secret stored under `name` by `fronde secret set`, encrypted in `fronde.db` (see [Store a secret](../how-to/store-a-secret.md)). Only in a component's `[[sources.credentials]]`, the `token` of a `kind = "mcp"` entry with a `url`, an account's `oauth_token` (and the legacy `[agent] oauth_token`), and only for the origin it was stored for. |

The settings below marked *secret* take one of these forms, except `{ stored = "…" }`, which only
a component's credential, the `token` of an upstream server reached by `url` and an account's
`oauth_token` take: `server.token`, `web.password` and the users' `password` refuse it, saying to use `{ file = "…" }` or
`{ env = "…" }`, and so does the `token` of a server run as a `command`, which has no origin to
bind it to.

When secrets are stored, Fronde checks the master key as it starts: a key that is not the one
that sealed them, no key at all, or a keychain that is locked or out of reach, stops it with an
error that says what to do. `fronde check` does the same. Where the key is kept is set in
[`[secrets]`](#secrets-1).

A secret file must be private. On Unix, Fronde refuses a file that group or others can read or
write, or that another user owns, and says so: `secret file <path> is readable by other users; run
chmod 600 <path>`. `fronde check` reports it too. For Docker secrets, which are usually mode 0444
and owned by root, set the environment variable `FRONDE_ALLOW_SHARED_SECRET_FILES=1` to skip
this check. It is off by default and applies to every secret file.

`fronde serve` keeps secrets out of core dumps and away from other processes of the same user on
Linux. Set `FRONDE_ALLOW_CORE_DUMPS=1` to turn that off while debugging a crash; see
[Security model](../explanation/security.md#core-dumps-and-other-processes).

## `[server]`

Required.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | address | `"127.0.0.1:8484"` | Address and port to listen on. Use `"0.0.0.0:8484"` in a container. |
| `token` | secret | required | Bearer token for `/mcp`. At least 32 characters. Fronde's own agent uses it too. |

## `[data]`

Required.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `dir` | path | `$XDG_DATA_HOME/fronde`, else `~/.local/share/fronde` (macOS and Windows: the platform's per-user data directory) | Where Fronde keeps its memory, index and conversation. Created (mode 0700) if missing; it must be writable by the user running Fronde. A relative path is read from the configuration file's directory. See [Data directory](data-directory.md). |
| `timezone` | IANA name | required | Time zone of the journal, the calendar and reminders, such as `"Europe/Paris"`. |
| `user` | string | `"me"` | The owner: what the assistant calls you, in its instructions and its system prompt, and whose the main conversation is. With [`[users]`](#usersname), the name of one of them, an admin. |

## `[secrets]`

Optional.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `backend` | `"auto"`, `"systemd"`, `"keychain"` or `"file"` | `"auto"` | Where the master key of the [stored secrets](../how-to/store-a-secret.md) is kept. See below. |
| `master_key` | `{ file = "…" }` | `master.key` beside the configuration file | The key file of the `file` backend, created (mode 0600) with the first secret. A relative path is read from the configuration file's directory. Only `{ file = "…" }`: the key never comes from an environment variable, which the processes Fronde starts, such as an upstream MCP server's command, inherit. |

The backends:

- `systemd`: the credential `fronde-master-key`, which systemd decrypts into
  `$CREDENTIALS_DIRECTORY` for a unit that has `LoadCredentialEncrypted=fronde-master-key:…`.
  Fronde only reads it: create it with `systemd-creds encrypt` (see
  [Run as a systemd service](../how-to/run-as-a-service.md#keep-the-master-key-in-a-systemd-credential)).
- `keychain`: the platform's keychain (Secret Service on Linux, such as GNOME Keyring or KWallet;
  the Keychain on macOS; the Credential Manager on Windows), entry `fronde` /
  `master-key@<hash of the data directory>`, one per data directory. Fronde makes the key there
  with the first secret, and the database records the entry's name, so that a data directory
  moved elsewhere keeps using it. Each call has 2 seconds to answer, and one runs at a time.
- `file`: the file `master_key` names.

With `auto`, the first secret stored makes or finds the key in the first of these that can hold
it: the systemd credential if the service has one, else the keychain, else, when there is no
keychain or it cannot be reached, the file. A keychain that is there but locked, or that does not
answer within 2 seconds (an unlock prompt), stops the first secret with an error that says to
unlock it and try again, rather than putting the key in a file. The database then records that
backend, and `auto` uses it alone from then on, even after the last secret is removed. A keychain
that is locked when Fronde starts, as after a reboot before the desktop session unlocks it, stops
Fronde with an error that says to unlock it: Fronde never makes a key in another place while
secrets are stored, since that key could not open them. A database from a version without
backends keeps its key file.

A backend named here is used alone, even when the database records another: a key moved there by
hand opens the stored secrets, and the record then follows it. If the key is not there, or does
not open them, the error names the recorded backend and says to set `backend = "auto"` or that
backend.

The key file is kept out of the data directory, so that a backup of the data directory, or a
snapshot of its volume, does not hold it with the secrets it opens (see
[Data directory](data-directory.md)). Pick a file outside `[data] dir`. Fronde must be able to
create it in its directory when the first secret is stored: if the configuration's directory is
read-only, as in a container where it is mounted read-only, set `master_key` to a file in a
writable directory of its own (see [Run in Docker](../how-to/run-in-docker.md)). A key left in
the data directory by an earlier version is moved to this file at the next start or
`fronde secret` command; if both hold a key and they differ, Fronde refuses to start.
To move the key from one backend to another, use `fronde secret move-key` (see
[Store a secret](../how-to/store-a-secret.md#move-the-master-key)): set `backend` to `"auto"`
or to the new backend first.

With the environment variable `FRONDE_KEYCHAIN=off`, Fronde behaves as if the machine had no
keychain: `auto` keeps a first key in the file, and `backend = "keychain"` fails with an error
that names the variable. Fronde warns at start while it is set. The repository's `mise.toml` and
`.cargo/config.toml` set it, so that tests and `mise run dev` never reach the keychain of the
machine they run on. A Fronde started by systemd or Docker does not get it.

## `[memory]`

Optional.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `memory_limit` | integer | `4000` | Maximum size of `MEMORY.md`, in characters. |
| `user_limit` | integer | `2000` | Maximum size of `USER.md`, in characters. |
| `git` | boolean | `true` | Commit the memory directory with git after each change. Needs `git` on the `PATH`. |

## `[[sources]]`

Zero or more sources of tools besides Fronde's own (memory and tool search), each an upstream
MCP server or a tool component. `kind` says which. Every source has a `name`, unique among all
sources, matching `^[a-z][a-z0-9_]{0,63}$`, and not `hub`, which is reserved for Fronde's own
tools, nor `python`, `[python]`'s, with `[python]` or without.

Earlier versions had `[calendar]` and `[[mcp]]`: Fronde now refuses them and says what to write
instead. The calendar is a component (see
[Configure the calendar](../how-to/configure-calendar.md)); an `[[mcp]]` section becomes a
`[[sources]]` entry with `kind = "mcp"` and the same keys.

### `kind = "mcp"`

An upstream MCP server. See [Add upstream MCP servers](../how-to/add-upstream-mcp-servers.md).

| Key | Type | Meaning |
| --- | --- | --- |
| `name` | string | Required. Prefixes the server's tools, so it cannot overlap the hub's reserved prefixes (`code_`, `hub_`, `integrations_`, `memory_`, `search_`, `threads_`, `use_`, `user_`). |
| `url` | URL | Streamable HTTP address of the server. |
| `command` | array of strings | Program and arguments to run, talking MCP over standard input and output. |
| `token` | secret | Optional, with `url` only: sent as `Authorization: Bearer <token>`. `{ stored = "name" }` is opened for the URL's origin each time Fronde connects: a name not stored yet, or stored for another origin, is waited for and tried again with backoff. |

Each server sets exactly one of `url` or `command`.

```toml
[[sources]]
kind = "mcp"
name = "mcphub"
url = "https://mcphub.example/mcp"
token = { env = "MCPHUB_TOKEN" }
```

### `kind = "component"`

A tool component: a WebAssembly file run in a sandbox, with no files and no network except the
origins granted here. Its manifest says what it asks for (see
[Component manifest](component-manifest.md)); this entry grants at most that.

| Key | Type | Meaning |
| --- | --- | --- |
| `name` | string | Required. The source of its tools, which keep the names the component gives them, each starting with one of its manifest's `tool_prefixes`. No other source's tools may use those prefixes: a component whose prefix is `<another source's name>_`, a prefix of the hub's own tools (`code_`, `hub_`, `integrations_`, `memory_`, `search_`, `threads_`, `use_`, `user_`) or another component's is not loaded. |
| `path` | path | Required. The `.wasm` file. |
| `manifest` | path | The manifest. Default: the `.toml` file beside `path` with the same name (`calendar.wasm`, `calendar.toml`). |
| `sha256` | hex string | SHA-256 of the `.wasm` file. Set it with `manifest_sha256`, or neither for a component shipped with Fronde. |
| `manifest_sha256` | hex string | SHA-256 of the manifest. |
| `allow` | array | Origins the component may reach: `"https://cloud.example"`, or `{ origin = "http://192.168.1.10:5232", local = true }` for a server on this machine or the local network. Each must be one the manifest asks for, and listed once: origins are compared after normalising them, so `https://Cloud.example:443` is `https://cloud.example` and `http://0x7f.1` is `http://127.0.0.1`. An IP address on a local network needs `local = true` (`fronde check` says so); link-local (169.254.0.0/16, fe80::/10) and cloud metadata addresses are refused even then. A request the sandbox refuses is logged as a warning naming the component, the origin and the fix. |
| `settings` | table of strings | What the component reads, checked against the manifest's settings: unknown keys and missing required ones are errors. Never a secret: a URL setting with a user name or password in it is refused (put them in `credentials`). Fronde adds `timezone` from `[data]` when the manifest declares it and the table does not set it. |
| `credentials` | array of tables | Secrets Fronde adds to the component's requests, one per origin; see below. |

Paths are read from the configuration file's directory, like every relative path.

**Approval.** A component loads only when its files are approved, by their SHA-256:

- An entry with `sha256` and `manifest_sha256` is pinned to those files, whatever they are.
  Hashes in the entry always win.
- An entry with neither is approved when its files are exactly those of a component shipped
  with this build of Fronde: the binary carries the hashes of the components built with it
  (`/app/components` in the image, `components/dist` in a checkout). The log says
  `approved: shipped with this build`.
- An entry with neither is also approved when its files are ones you approved for this source
  name, stored in `fronde.db`. Each such approval holds the source's name and both hashes, so it
  approves neither another entry nor changed files. The log says
  `approved: by the owner, stored in the database`. Fronde reads these approvals when it starts:
  one stored later applies at the next start, and one removed while Fronde runs keeps approving
  until the next start, even when the supervisor restarts the component. Approving a component
  from the chat is planned; nothing stores an approval yet.
- Anything else is refused. A path never approves anything.

`fronde check` prints both hashes of each component and which approval applied. For a component
from elsewhere, copy the hashes into the entry once you have read the manifest. When a pinned
file changes, such as after an update, the component is skipped with a warning until you check
the new manifest and copy the new hashes.

**Credentials.** Each `[[sources.credentials]]` table names an `origin` from `allow` and exactly
one of:

| Key | Type | Meaning |
| --- | --- | --- |
| `basic` | `{ username = "…", password = secret }` | Sent as `Authorization: Basic …`. |
| `bearer` | secret | Sent as `Authorization: Bearer …`. |

The secret may be `{ stored = "name" }`, a secret stored with `fronde secret set`. It is opened
when the component starts, and only for a credential whose `origin` is the one the secret was
stored for, both normalised: a secret stored for `https://other.example` is refused for
`https://cloud.example`, with the `fronde secret set` command that stores it for the right one.
A missing one is refused with the same command.

The manifest must ask for a credential of that kind for that origin. A credential travels only
over https, or plain http to this machine (`localhost`, `127.0.0.1`, `::1`, `::ffff:127.0.0.1`;
not `localhost.` nor `0.0.0.0`), and only when the name resolves to loopback addresses alone: a
server on the local network needs https. In Docker, this machine is the container: a server on
the Docker host (`host.docker.internal`) needs https too.

```toml
[[sources]]
kind = "component"
name = "calendar"
path = "/app/components/calendar.wasm"
allow = ["https://cloud.example"]

[sources.settings]
events_url = "https://cloud.example/remote.php/dav/calendars/alex/personal/"
reminders_url = "https://cloud.example/remote.php/dav/calendars/alex/tasks/"

[[sources.credentials]]
origin = "https://cloud.example"
basic = { username = "alex", password = { env = "FRONDE_CALDAV_PASSWORD" } }
```

A component that fails to load (bad file, files not approved, a grant its manifest does not ask
for, a credential over plain http to another machine, a tool prefix another source uses, a
missing secret) is skipped with a warning, and the rest of Fronde starts. `fronde check` reports
the same problems as errors, except those only compiling the component finds (a file that is not
a component, imports it may not have, tools outside its prefixes).

A component whose stored secret is missing, or stored for another origin, is not skipped: Fronde
tries it again, after 2 seconds, then waiting twice as long each time up to a minute, and reads
the stored secrets again at each try. Store the secret with `fronde secret set` while Fronde runs
and the component starts within a minute, with no restart. If a try then fails for another
reason, such as a master key that does not open the stored secrets, the component is skipped as
above and not tried again. A component that started keeps the value it read: after changing a
stored secret, restart Fronde.

## `[python]`

Optional. With it, Fronde serves `code_run_python`, which runs Python in a sandbox for
calculations (see [Tools](tools.md#python)). Without it, there is no such tool.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `dir` | path | required | The Python runtime: `python.wasm` and the standard library under `lib/`, as `mise run python` lays them out in `python/`. The container image has it at `/app/python`. A relative path is read from the configuration file's directory. |

```toml
[python]
dir = "/app/python"
```

At start, Fronde checks that `python.wasm` is the CPython build it pins and compiles it, which
takes about a second. If that fails (another build, a missing directory), the tool is left out
with a warning and the rest of Fronde starts. `fronde check` loads it the same way and prints
`python: approved (CPython 3.15.2)`, or the error.

## `[web]`

Optional. With it, Fronde serves the chat at `/` and runs its agent. Without it, the server is
only the hub: `/health` and `/mcp`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `password` | secret | required without `[users]` | The legacy form of the owner's password (see [`[users.<name>]`](#usersname)): `[data] user` signs in with it. Must not be empty. Refused with `[users]`, saying to move it to `[users.<name>] password`. |
| `secure_cookie` | boolean | `false` | Mark the session cookie `Secure`. Set it to `true` when the page is served over https. |

With `[users]`, `[web]` may be left empty (`[web]` alone) to serve the chat.

## `[users.<name>]`

Optional. The people who sign in to Fronde, one section per person, the section's name being
theirs. Signing in by name, each person to their own conversations, is **planned**: until then,
only the owner, `[data] user`, signs in, with their own password, and the others' sections are
only checked. Until each person has spaces of their own, the memory, the calendar, the
integrations and every other service are the owner's, not every admin's (see
[Security model](../explanation/security.md#threads)). Once someone is no longer listed, Fronde
gives their threads to `removed:<name>` at start, read-only for good: a person later listed
under the same name starts with none of them.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `admin` | boolean | `false` | Whether they see and manage everything. The owner must be one. |
| `password` | secret | required | Their password, `{ file = "…" }` or `{ env = "…" }`, never stored. Must not be empty. Signing in with OIDC instead is **planned**. |

A name is 1 to 64 characters, with no hidden character (a zero-width space, a direction mark, a
control character), no space at either end, and none of `/`, `\`, `:` or `..`; quote it when it
is more than letters, digits, `_` and `-` (`[users."Anne-Sophie Dupré"]`). Names are compared
exactly, and two names that look alike are refused: the same once both are put in NFKC form and
in lower case (`Marie` and `marie`, an accent composed or not), or once letters of another script
that look the same are read as the Latin ones (UTS #39 confusable skeletons: a Cyrillic `а` for an
`a`). Fronde refuses, naming the keys, a user without a password, `[data] user` not in `[users]`
or not an admin, and `[web] password` with `[users]`. The legacy form's `[data] user` follows the
same rules for its characters.

No two people's credentials are shared, nor a person's with anything else: `server.token`,
`web.password`, each user's password, each account's token and the master key file each read from
a stored secret, a variable and a file of their own, none named by a source's credential or token
either, and `[agent] env` sets none of their variables. A file is the same file however its path
is written: through `..`, a symbolic link or a hard link (Fronde compares the file it leads to).
`fronde check` and `fronde serve` also read every one of them that reads, stored tokens included,
and refuse two that hold the same value, naming both keys and never the value.

```toml
[data]
user = "alix"

[web]

[users.alix]
admin = true
password = { file = "alix.password" }

[users.marie]
password = { file = "marie.password" }
```

Without `[users]`, the legacy form keeps working, and is never rewritten: Fronde reads it as one
user, `[data] user`, an admin, signing in with `[web] password`.

## `[accounts.<name>]`

Optional. The provider subscriptions the agents sign in with, one section per account. An
account belongs to one person, its `owner`, and is where that is written, once: the threads of
its owner run on it: every agent Fronde starts for a thread of theirs (their threads, their
sub-agents, the turns a sub-agent's result wakes) signs in with this account's token, read again
at each start, and with no one else's. A person without an account on a provider gets no agent
there: the thread says « Aucun abonnement Claude n'est relié à ton compte : demande à
l'administrateur. », and nothing falls back to the owner's account. Which account is a person's
is read from the file at start, never stored with the threads: a change applies at each agent's
next start after a restart.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `provider` | string | required | The provider the subscription is for: `"claude"`, the only one configured. Codex and Gemini/Antigravity are refused as planned. |
| `owner` | string | required | The person it belongs to: one of [`[users]`](#usersname), or `[data] user` without them. |
| `oauth_token` | secret | required | The token `claude setup-token` prints, run in the owner's own home and signed in with the owner's own Claude account: `{ stored = "…" }` (stored with `fronde secret set <name> --origin https://api.anthropic.com`), `{ file = "…" }` or `{ env = "…" }`. Fronde reads it at each start of the owner's agents and gives it as `CLAUDE_CODE_OAUTH_TOKEN`, through their environment only. |

The name matches `^[a-z][a-z0-9_]{0,63}$`. Fronde refuses, naming the keys:

- an `owner` who is not a user, or two accounts of one owner on one provider: a person has one
  account per provider;
- two accounts reading one token, or an account reading another credential's secret (see
  [`[users.<name>]`](#usersname)), since one token would then sign both in;
- `[accounts.claude]`, the legacy form's name, owned by anyone but `[data] user`;
- an account without `oauth_token`;
- `[agent] oauth_token` with `[accounts]`: move the token to the owner's account.

```toml
[data]
user = "alix"

[accounts.alix]
provider = "claude"
owner = "alix"
oauth_token = { stored = "claude_alix" }
```

### The legacy form

A file without `[accounts]` keeps working, and is never rewritten: Fronde reads it as one account,
named `claude`, on Claude, owned by `[data] user`, whose token is `[agent] oauth_token`; without
it, `CLAUDE_CODE_OAUTH_TOKEN` when Fronde's own environment holds it (the
[Docker](../how-to/run-in-docker.md) set-up), which Fronde reads and gives the agent as an account's
token, with a warning at start saying to name it; without either, the agent starts unsigned and
uses the sign-in kept in its home.

With `[accounts]`, `fronde check` and `fronde serve` refuse to start while Fronde's environment
holds a variable an agent signs in with (`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`,
`ANTHROPIC_AUTH_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`) that no account names as `{ env = "…" }`,
saying which, never its value. Whatever the form, no agent inherits these variables, nor any
`ANTHROPIC_*` or `CLAUDE_CODE_USE_*`: an agent's token is only its account's, given by Fronde. Only this account may
lack a token. `fronde check` says which it read (see
[Command line](command-line.md#fronde-check)).

## `[agent]`

Optional. The agent Fronde runs for the chat, over ACP on standard input and output. It is
started on the first message, and only when `[web]` is set. These are the settings of Claude, the
only provider so far; another provider will come with a table of its own, `[providers.<id>]`
(**planned**, see [Why subscriptions and ACP](../explanation/subscriptions.md#other-providers)).
Until then a `[providers]` table is refused, and so is a sub-agent asked to run on `codex` or
`gemini` (the `provider` of [`threads_start`](tools.md#threads)): "only claude is configured;
Codex and Gemini/Antigravity are planned".

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | array of strings | `["claude-agent-acp"]` | Program and arguments. A bare name is looked up on the `PATH`, which does not include `agent/node_modules/.bin` outside `mise exec`: use the adapter's full path (`fronde init` writes it when it finds it). `fronde check` warns when the program is not found. |
| `env` | table of strings | `{}` | Extra environment variables, in plain text: never a secret. The agent does not inherit Fronde's environment: it gets `PATH`, the locale (`LANG`, `LANGUAGE`, `LC_*`), `TZ`, the proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, in either case) and CA variables (`NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `SSL_CERT_DIR`) when set, these, and `HOME` and `TMPDIR`, which Fronde sets to its own home (see [Data directory](data-directory.md)). Never a `{ env = "…" }` secret's variable, even one whose name matches. No `CLAUDE_*` variable is inherited: set the ones the agent needs here (`CLAUDE_CODE_EXECUTABLE`). `HOME`, `TMPDIR`, `CREDENTIALS_DIRECTORY`, `CLAUDE_CONFIG_DIR` (one Claude directory would sign every thread in alike), `XDG_*` and `DBUS_*` cannot be set here, nor the variables that decide how an agent signs in (`CLAUDE_CODE_OAUTH_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`, any `ANTHROPIC_*`, `CLAUDE_CODE_USE_*`, `CLAUDE_CODE_OAUTH_*`, `CLAUDE_CODE_API_KEY_*` or `CLAUDE_CODE_CLIENT_*`): only an account's `oauth_token` signs an agent in. |
| `sandbox` | boolean | `true` | Confine the agent with Landlock and seccomp (see [Where the agent runs](../explanation/security.md#where-the-agent-runs)). On a kernel that cannot (no Landlock, or ABI 1 or 2, before Linux 6.2), the agent does not start. `false` runs it unconfined, with access to everything your user can read and write, and Fronde logs a warning at every start; it is refused once `[users]` lists someone who is not an admin. |
| `read` | array of paths | `[]` | More directories the confined agent may read and run programs from, besides the system's, its `PATH` and the adapter's install. A relative path is read from the configuration file's directory. One that equals or contains the data directory, the configuration's directory, a secret file or systemd's credentials is an error. Use it for a Claude Code installed elsewhere (`CLAUDE_CODE_EXECUTABLE`) or a Node.js outside the `PATH`. Only the owner's agents (`[data] user`'s threads) get it. |
| `oauth_token` | secret | none | The legacy form of the owner's account (see [The legacy form](#the-legacy-form)): the token `claude setup-token` prints, as `{ file = "…" }`, `{ env = "…" }` or `{ stored = "…" }` (stored with `fronde secret set <name> --origin https://api.anthropic.com`). Fronde reads it at each start of the agent and gives it as `CLAUDE_CODE_OAUTH_TOKEN`, through its environment only. Refused with `[accounts]`. See [Sign Fronde in to Claude](../how-to/sign-in-to-claude.md). |
| `builtin_tools` | array of strings | `["WebSearch", "WebFetch"]` | Claude Code's own tools the agent may use besides the hub's. Only `WebSearch` and `WebFetch` are allowed; `[]` for none. `AskUserQuestion` is always on, never allowed up front: it asks you through a card. |

There is no setting for the model: each thread runs on the agent's default unless it was given
one, which Fronde chooses through the session's config options (see
[How Fronde works](../explanation/architecture.md#providers-and-models)).

```toml
[agent]
command = ["claude-agent-acp"]
env = { CLAUDE_CODE_EXECUTABLE = "/home/alex/.local/bin/claude" }
builtin_tools = ["WebSearch", "WebFetch"]
sandbox = true
# The installed Claude Code: ~/.local/bin/claude links into ~/.local/share/claude.
read = ["/home/alex/.local/share/claude"]
```

### `[agent.limits]`

Optional. What the agent, with everything it starts, may use of the machine. When a systemd
user manager answers, Fronde starts the agent in a systemd scope under `fronde-agents.slice`
with these limits (see [Run as a service](../how-to/run-as-a-service.md#the-agents-scope)).
Without one, as in a container, they do not apply: give the container its own limits (see
[Run in Docker](../how-to/run-in-docker.md#limits)). Either way, the kernel kills the agent
before Fronde when memory runs out (`oom_score_adj` 1000), and the agent writes no core dump.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `memory` | size or `"none"` | `"2G"` | The most memory it may use (`MemoryMax`), as a number of bytes or with `K`, `M`, `G` or `T` (powers of 1024). Past three quarters of it (`MemoryHigh`), the kernel reclaims its memory first; at the limit, it is OOM-killed. |
| `cpus` | number or `"none"` | `2` | How many CPUs' worth of time it may use (`CPUQuota`, 100 % per CPU): `0.5` is half of one. At least `0.01`. |
| `tasks` | integer or `"none"` | `512` | How many processes and threads it may run at once (`TasksMax`). Node.js and Claude Code use a few dozen. |
| `total_memory` | size or `"none"` | `"4G"` | The most memory all the agents together may use: Fronde sets it at start as the `MemoryMax` of `fronde-agents.slice`, with `MemoryHigh` at three quarters (`systemctl --user set-property --runtime`). `"none"` leaves the slice uncapped. Only with a scope; without one, `[threads] max_running` and the container's limits are the budget. |

`"none"` leaves out that limit: the slice's and systemd's own defaults then apply (systemd caps
a user's tasks at 15 % of the system's by default). With all three `"none"`, Fronde starts no
scope. If `systemd-run` is there but the user manager makes no scope, the agent starts without
one, and Fronde logs a warning saying why; the limits are a safeguard for the machine, not part
of the agent's confinement, which never depends on systemd. `fronde check` and the start log
say which applies (`agent limits: …`).

```toml
[agent.limits]
memory = "4G"
cpus = 1.5
tasks = "none"
```

## `[threads]`

Optional. Each conversation (thread) has its own agent process, started on its first message.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `max_running` | integer, 1 to 1000 | `3` | How many agent processes run at once, the main thread's included; at least 1. When none is free, Fronde stops the agent idle for the longest time; when every agent is in a turn, the message waits, its thread shows « En attente : d'autres conversations utilisent l'agent. », and it starts once another turn ends (Stop gives up the wait). Threads you did not start (the sub-agents an agent starts with [`threads_start`](tools.md#threads); an outside client's are planned) share all of them but one, kept for yours. |
| `idle_stop` | duration | `"10m"` | How long an agent is kept after its turn ends without a new message (`"90s"`, `"1h 30m"`). It then stops, freeing its place and revoking its hub token; the next message starts it again and resumes its session. |
| `max_depth` | integer, 0 to 3 | `1` | How deep a thread may be and still start sub-agents. Your threads are at depth 0, their sub-agents at 1: with `1`, your threads may delegate and their sub-agents may not; with `2`, a sub-agent may start sub-agents of its own, which may not; `0` turns sub-agents off. Past it, `threads_start` and `threads_send` are hidden and refused. |
| `children_running` | integer, 1 to 1000 | `3` | How many sub-agents one thread may have in a turn at once, those waiting for an agent included. It bounds how much work one agent can set going in parallel, and the queue behind the pool's slots (`max_running`): the sub-agents of every thread share them. |
| `reserve` | percentage, `"0%"` to `"100%"` | `"30%"` | The share of each subscription window (5 hours, and weekly for all models) kept for you. Once a window's use reaches `100%` minus it (70 % by default), as the provider's latest reading says, `threads_start` and `threads_send` are refused, even from a turn you started, naming the window, its use, this setting and the reset time, and sub-agents' results wait for your next message instead of waking their thread. Your own messages always run, and turns already running go on. While a reading decides, `children_per_day` and `child_turns_per_day` do not apply; `"0%"` turns the reserve off and leaves them. See [Threads and sub-agents](../explanation/threads.md#what-background-work-may-spend). |
| `children_per_day` | integer, 1 to 1000 | `30` | How many sub-agents all threads together may start in the last 24 hours, counted from the database; archiving one frees no place. It bounds how many tasks an agent can start in the background over a day. It applies only when no usage reading decides (see `reserve`). |
| `children_kept` | integer, 1 to 1000 | `200` | How many sub-agents one thread may keep not archived. Only a guard against clutter in the thread list; archiving a sub-agent frees its place. |
| `child_turns_per_day` | integer, 1 to 1000 | `100` | How many turns of sub-agents all threads together may start in the last 24 hours, counted from the threads' streams: each sub-agent's task and each `threads_send`. It bounds how much an agent can keep sub-agents working over a day, whether it starts new ones or sends messages to the same. It applies only when no usage reading decides (see `reserve`). |
| `child_turn_timeout` | duration, at most `"24h"` | `"30m"` | How long one turn of a sub-agent may run once its agent has it (the wait for an agent slot does not count). Past it the turn is stopped as Stop does, with the notice « Délai dépassé » in the sub-agent's thread, and its parent is told it ended `cancelled`. A turn of yours that a sub-agent's result woke is capped the same way; the turns your messages start are not. |
| `wakes_without_owner` | integer, 1 to 1000 | `10` | How many turns a thread may be woken by its sub-agents' results since your last message in it. The results that come next are held: the thread says how many wait, and your next message carries them to the agent and starts the count again. It ends a chain of wakes that a sub-agent's answer could keep going without you. |

The agent with its Claude Code uses about 0.3 to 0.45 GiB before its first turn (measured, see
[Architecture](../explanation/architecture.md#threads)), more during a long conversation:
`max_running` times about 0.6 GiB fits within the default `total_memory` of 4 GiB.

The limits on sub-agents are there for your subscription's quota. A sub-agent spends it in
the background, without you in the loop, and an agent may be steered by what it reads (a web page,
a mail: a prompt injection) into starting many. Fronde enforces them whatever the agent is told;
each refusal names the setting to change. Together they bound that spending; they do not make it
nothing. `reserve` reads the quota itself, from the provider's usage windows; the daily counts
stand in for it when no reading decides (see
[Threads and sub-agents](../explanation/threads.md#limits)).

```toml
[threads]
max_running = 3
idle_stop = "10m"
max_depth = 1
children_running = 3
reserve = "30%"
children_per_day = 30
children_kept = 200
child_turns_per_day = 100
child_turn_timeout = "30m"
wakes_without_owner = 10
```

## Environment variables

| Variable | Meaning |
| --- | --- |
| `FRONDE_SITE_ROOT` | Directory of the built web interface. Default `target/dx/fronde-web/release/web/public`, relative to the working directory. |
| `RUST_LOG` | Log filter, such as `debug` or `fronde_hub=debug`. Default `info`. The WebSocket library's targets (`tungstenite`, `tokio_tungstenite`) stay off whatever it says: they would log the chat's messages, secrets typed in a card included. |
| `FRONDE_KEYCHAIN` | `off` makes the platform's keychain absent, for tests and development (see [`[secrets]`](#secrets-1)); Fronde warns at start while it is set. |
| `CLAUDE_CODE_OAUTH_TOKEN` | The token from `claude setup-token`, read by Fronde as the legacy form's account when the file has neither `[accounts]` nor `[agent] oauth_token` (see [The legacy form](#the-legacy-form)); never inherited by an agent as it is. Prefer naming it, as `oauth_token = { env = "CLAUDE_CODE_OAUTH_TOKEN" }` in the owner's account. |

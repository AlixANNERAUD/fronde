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
| `{ stored = "name" }` | The secret stored under `name` by `fronde secret set`, encrypted in `fronde.db` (see [Store a secret](../how-to/store-a-secret.md)). Only in a component's `[[sources.credentials]]`, and only for the origin it was stored for. |

The settings below marked *secret* take one of these forms, except `{ stored = "…" }`, which only
a component's credential takes: `server.token`, `web.password` and an upstream server's `token`
refuse it, saying to use `{ file = "…" }` or `{ env = "…" }`.

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
| `user` | string | `"me"` | What the assistant calls you, in its instructions and its system prompt. |

## `[secrets]`

Optional.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `backend` | `"auto"`, `"systemd"`, `"keychain"` or `"file"` | `"auto"` | Where the master key of the [stored secrets](../how-to/store-a-secret.md) is kept. See below. |
| `master_key` | `{ file = "…" }` | `master.key` beside the configuration file | The key file of the `file` backend, created (mode 0600) with the first secret. A relative path is read from the configuration file's directory. Only `{ file = "…" }`: the key never comes from an environment variable, which the agent inherits. |

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
Moving the key from one backend to another (`fronde secret move-key`) is **planned**.

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
tools. With `[python]`, `python` is taken too.

Earlier versions had `[calendar]` and `[[mcp]]`: Fronde now refuses them and says what to write
instead. The calendar is a component (see
[Configure the calendar](../how-to/configure-calendar.md)); an `[[mcp]]` section becomes a
`[[sources]]` entry with `kind = "mcp"` and the same keys.

### `kind = "mcp"`

An upstream MCP server. See [Add upstream MCP servers](../how-to/add-upstream-mcp-servers.md).

| Key | Type | Meaning |
| --- | --- | --- |
| `name` | string | Required. Prefixes the server's tools, so it cannot overlap the hub's reserved prefixes (`code_`, `hub_`, `memory_`, `search_`, `use_`). |
| `url` | URL | Streamable HTTP address of the server. |
| `command` | array of strings | Program and arguments to run, talking MCP over standard input and output. |
| `token` | secret | Optional, with `url` only: sent as `Authorization: Bearer <token>`. |

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
| `name` | string | Required. The source of its tools, which keep the names the component gives them, each starting with one of its manifest's `tool_prefixes`. No other source's tools may use those prefixes: a component whose prefix is `<another source's name>_`, a prefix of the hub's own tools (`code_`, `hub_`, `memory_`, `search_`, `use_`) or another component's is not loaded. |
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
| `password` | secret | required | The chat's password. Must not be empty. |
| `secure_cookie` | boolean | `false` | Mark the session cookie `Secure`. Set it to `true` when the page is served over https. |

## `[agent]`

Optional. The agent Fronde runs for the chat, over ACP on standard input and output. It is
started on the first message, and only when `[web]` is set.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | array of strings | `["claude-agent-acp"]` | Program and arguments. A bare name is looked up on the `PATH`, which does not include `agent/node_modules/.bin` outside `mise exec`: use the adapter's full path (`fronde init` writes it when it finds it). `fronde check` warns when the program is not found. |
| `env` | table of strings | `{}` | Extra environment variables, in plain text: never a secret. The agent also inherits Fronde's environment. |
| `builtin_tools` | array of strings | `["WebSearch", "WebFetch"]` | Claude Code's own tools the agent may use besides the hub's. Only `WebSearch` and `WebFetch` are allowed; `[]` for none. |

```toml
[agent]
command = ["claude-agent-acp"]
env = { CLAUDE_CODE_EXECUTABLE = "/home/alex/.local/bin/claude" }
builtin_tools = ["WebSearch", "WebFetch"]
```

## Environment variables

| Variable | Meaning |
| --- | --- |
| `FRONDE_SITE_ROOT` | Directory of the built web interface. Default `target/dx/fronde-web/release/web/public`, relative to the working directory. |
| `RUST_LOG` | Log filter, such as `debug` or `fronde_hub=debug`. Default `info`. |
| `CLAUDE_CODE_OAUTH_TOKEN` | Read by the agent, not by Fronde: the token from `claude setup-token`. |

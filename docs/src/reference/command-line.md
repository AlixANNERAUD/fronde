# Command line

```text
fronde <command> --config <file>
```

Commands that read a configuration take it with `--config <file>` (optional with `fronde.toml` as
the default for `init` and `service install`). Logs go to standard error, filtered by `RUST_LOG`
(default `info`). A command exits with 0 on success and 1 on failure, printing `error: …` with
the cause, except `service status`, which passes `systemctl`'s exit code through.

## `fronde init`

```text
fronde init [--config <file>] [--force]
```

Writes a minimal native configuration, `fronde.toml` by default, and two secrets next to it:
`fronde_token` (32 random bytes in hex) and `fronde_password` (a generated passphrase, printed
once on the terminal). Every file is created with mode 0600. The configuration listens on
`127.0.0.1:8484`, leaves `[data] dir` out (see the [default](configuration.md#data)), takes the
time zone from the system (`UTC` when it cannot tell) and the user name from `$USER`, and refers
to the secrets by relative paths. If `agent/node_modules/.bin/claude-agent-acp` exists in the
current directory (after `mise run agent`), its absolute path becomes `[agent] command`;
otherwise `init` suggests `mise run agent`. It ends by printing the commands to run next.

It refuses to overwrite any of the three files unless `--force` is given, which makes a new token
and password.

## `fronde serve`

Starts the server: opens the memory, registers the tools, connects to upstream MCP servers in the
background, and serves HTTP on `[server] listen`. With `[web]`, it also serves the chat and runs
the agent. It stops on Ctrl+C.

On Linux, it first makes its process non-dumpable and sets `RLIMIT_CORE` to 0, so that no core
dump, ptrace or `/proc/<pid>/mem` read by another process of the same user can reach its secrets
(see [Security model](../explanation/security.md#core-dumps-and-other-processes)). Set
`FRONDE_ALLOW_CORE_DUMPS=1` to skip this when debugging a crash.

`fronde __confine` is internal: it is how `serve` starts its agent (see
[Security model](../explanation/security.md#the-agents-environment)). It is not meant to be run by
hand.

## `fronde check`

Validates the configuration and exits. It parses the file, then reads every secret: the server
token (at least 32 characters), the chat password (not empty), the upstream tokens and the
components' credentials. For each component it prints the SHA-256 of its `.wasm` file and of its
manifest, then checks it as `serve` would before loading it (approval, grants within its
manifest, settings, tool prefixes, credentials over https) and prints which approval applied:
`approved: shipped with this build (calendar)`, `approved: pinned by the hashes in its entry` or
`approved: by the owner, stored in the database`. For each credential that names a stored secret
(`{ stored = "…" }`) it then prints `source <name>: credential for <origin>: stored secret <secret>
opens, stored for this origin`, never the value; a secret that is not stored, one stored for
another origin, or one that does not open is an error saying how to fix it. It reads the
approvals and secrets stored in `fronde.db` when the data directory has one, and checks the master
key against them, from the backend `serve` would use, as `serve` does at start; when secrets are
stored it prints `secrets: master key from <place> (<backend>), opens the stored secrets` (a key an earlier version left in the data directory
is read there and not moved: `serve` moves it), and never creates, updates or writes the
database: it
opens it read-only, and a database an older Fronde left without the approvals or secrets table
has no stored approval or secret, which is what `serve` would find after updating it. With
`[python]`, it checks that `python.wasm` is the pinned build and compiles it, then prints
`python: approved (CPython 3.15.2)`. With `[web]`, it prints what the agent's sandbox covers on
this kernel, such as `agent sandbox: Landlock ABI 6 (files, TCP ports 443 53 8484, device ioctls,
signals and abstract sockets); seccomp: no Unix sockets but connected pairs, only IPv4, IPv6 and netlink route
sockets, no io_uring`, followed by `the agent cannot start` on a kernel without Landlock or
seccomp, then what caps its resources: `agent limits: 2 GiB memory, 2 CPUs, 512 tasks, in a
systemd scope under fronde-agents.slice` once the systemd user manager made a scope like the
agent's (it starts one that runs `true`), or `agent limits: none (…)` with the reason, then how
many agents run at once and their memory together: `agents: at most 3 at once (one kept for your
own threads), stopped after 10m idle, 4 GiB for all of them (2 GiB each)`, then the limits on
sub-agents: `sub-agents: your threads may start sub-agents, theirs may not; 3 running at once per
thread, 200 kept per thread, 30m per turn, 10 wakes in a row without you; 30% of each subscription
window kept for you; 30 a day in all (100 turns), daily counts apply only without a usage
reading`, then the latest usage reading of your subscription, read from the database opened
read-only: `usage: claude: 5 h 42 % (resets 16:40), week 18 % (resets Mon 24 Nov 09:00), read 12
min ago; background work allowed below 70%` (times in `[data] timezone`, the day added when it is
not today, rounded up to the minute; `use unknown (reset since the reading)` once a reset time has
passed; `background work paused
until 16:40 (5-hour window past the reserve's line, 70%)` past `[threads] reserve`), or `usage:
claude: no reading yet; background work bounded by the daily counts` before Fronde's first turn,
or `usage: cannot read (…)` when the stored reading does not read, and the check goes on (see [Usage and quotas](../explanation/subscriptions.md#usage-and-quotas)); the start log prints
the same line; then one line per [account](configuration.md#accountsname), never with its
token: `account alix (claude, owner alix): stored secret claude_alix opens, for
https://api.anthropic.com` (opened as the agent's start opens it, from the database read-only),
`account alix (claude, owner alix): token from the file /run/secrets/claude, read` or `… from the
environment variable CLAUDE_TOKEN, read`; for the [legacy form](configuration.md#the-legacy-form),
`account claude (claude, owner alix): token from the file …, read ([agent] oauth_token, legacy
form)`, `… token from Fronde's environment, CLAUDE_CODE_OAUTH_TOKEN, read (legacy form)` or `…: no
token: the agent starts unsigned (legacy form)`. A token that does not read, or holds spaces or
line breaks, fails the check, and so does the owner's stored token not stored yet; another
person's not stored yet is a warning, `warning: stored secret claude_marie not stored yet: store
it with fronde secret set …`. It also refuses a grant over Fronde's data or configuration
(see [Where the agent runs](../explanation/security.md#where-the-agent-runs)); it creates the
agent's `agent/` and `home/` in the data directory for that. It also checks that `[data] dir` can
be created and written:
it creates the directory (mode 0700), writes a probe file and removes it, and otherwise names the
path and the fix (set `[data] dir` to a directory you own, such as `~/.local/share/fronde`). A
`[server] listen` that is not an address is reported by the parse. When `[web]` is set and the
agent command is not found on `PATH` (or at the path given), it prints a warning, not an error.
It prints `config OK` when all is well. It does not connect to anything.

## `fronde secret`

Stores, lists and removes the secrets kept encrypted in `fronde.db`, scope `sources`, and
rotates or moves the master key that seals them (see
[Store a secret](../how-to/store-a-secret.md) and
[Security model](../explanation/security.md#stored-secrets)). Each command makes itself
non-dumpable on Linux, moves a master key an earlier version left in the data directory to its
place, and opens the database, each as `serve` does. `set`, `list` and `remove` work while
Fronde runs; `rotate-key` and `move-key` do not.

```text
fronde secret set <name> --origin <origin> --config <file>  < value
fronde secret list --config <file>
fronde secret remove <name> --config <file>
fronde secret rotate-key --config <file>
fronde secret move-key --to <file|keychain|systemd> --config <file>
```

- `set` reads the value from standard input, never from an argument, and refuses a terminal,
  which would echo it. One final line break (`\n` or `\r\n`) is dropped; an empty value, one
  that is not UTF-8 or one over 64 KiB is refused. The name is 1 to 64 letters, digits, `-`, `_`
  or `.`. The origin is `scheme://host[:port]` over http or https, stored as Fronde normalises it
  (`HTTPS://Cloud.Example:443` becomes `https://cloud.example`); the secret is bound to it.
  Setting a name again replaces its value and origin. The first secret creates the master key in
  the backend `[secrets] backend` picks (see
  [Configuration](configuration.md#secrets-1)): with `auto`, a systemd credential when the
  command runs under one (read, never made), else the keychain (an error saying to unlock it if it
  is locked), else, with no keychain to reach, `master.key` beside the configuration file (or the
  file `[secrets] master_key` names), and prints where; while secrets are stored, a missing or different key, or a keychain that is
  locked or out of reach, is an error and no new key is made anywhere. Any argument besides the name and the options is refused, exit status 2,
  without being shown. It prints `stored secret <name> for <origin>`.
- `list` prints one line per secret: its name, origin and last change (UTC), separated by tabs,
  or `no stored secret`. It never prints a value.
- `remove` removes one secret, and exits with 1 if there is none by that name. Removing the last
  one also removes the key check, so a lost key then no longer blocks a new one; the backend
  recorded for it stays.
- `rotate-key` makes a new master key, seals every stored secret (of every scope) and the key
  check again with it in one transaction, and puts it in place of the old one, in the same
  backend. It prints `rotated the master key <place>: <n> stored secret(s) sealed again with the
  new key`. A key in a systemd credential is refused: Fronde never writes one.
- `move-key --to <backend>` keeps the same key in another backend, `file` or `keychain`: it
  copies the key there, reads it back, checks that it opens the stored secrets, records the new
  backend, then removes the key from the old one, and prints both places. It is refused, with
  nothing changed, when nothing is stored, when the key is there already, when `[secrets]
  backend` names another backend than the target, when the target cannot be written (a locked or
  unreachable keychain, with the keychain's own error) or already holds a different key. `--to
  systemd` only explains how to make the credential yourself.

Both refuse to run while Fronde runs on the same data directory, and name the process that
holds it (`process <pid> (fronde serve)`); `serve` likewise refuses to start while either runs.
Both print where the key is, never the key. Each step is durable before the next, so that a
crash leaves a key that opens every stored secret; `serve` finishes an interrupted rotation when
it starts, with a warning in the log, and so does the next `rotate-key` or `move-key`. Until
then, `fronde check` and `fronde secret set` say that a rotation was interrupted.

A component's credential names a stored secret with `{ stored = "<name>" }` (see
[Configuration](configuration.md#kind--component)).

## `fronde healthcheck`

Exits with 0 when the server configured in the file answers `GET /health` with a success within
5 seconds. A server listening on every interface (`0.0.0.0` or `::`) is reached over the loopback.
The container image uses it as its health check.

## `fronde service`

Manages Fronde as a systemd user service (`fronde.service`). See
[Run as a systemd service](../how-to/run-as-a-service.md).

```text
fronde service install [--config <file>] [--no-start] [--force]
fronde service uninstall
fronde service status
```

- `install` resolves the configuration file (default `fronde.toml`) and the running `fronde`
  binary to absolute paths, writes `$XDG_CONFIG_HOME/systemd/user/fronde.service` (falling back
  to `~/.config/systemd/user/fronde.service`) with mode 0644, runs `systemctl --user daemon-reload`,
  and runs `systemctl --user enable --now fronde.service`. With `--no-start`, it stops after
  `daemon-reload`. If the unit already exists with different content, it refuses unless `--force`
  is given; if the content is identical, it ensures the service is enabled.
- `uninstall` runs `systemctl --user disable --now fronde.service` (ignoring "not loaded" errors),
  removes the unit file, and runs `systemctl --user daemon-reload`. Configuration and data files
  are kept.
- `status` runs `systemctl --user status fronde.service --no-pager` and passes its exit code
  through.

If `systemctl` or a systemd user instance is not available, all three commands exit with 1 and
suggest running `fronde serve` under another supervisor.

## Other options

`fronde --help` lists the commands, `fronde --version` prints the version.

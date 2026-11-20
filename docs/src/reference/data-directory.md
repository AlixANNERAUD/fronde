# Data directory

Everything Fronde keeps lives under `[data] dir`, except the master key of the stored secrets,
which is kept apart on purpose: in a systemd credential, the keychain, or `master.key` beside the
configuration file (see [Configuration](configuration.md#secrets-1)). Back up the data directory
and, separately, a key file.

| Path | Holds |
| --- | --- |
| `memory/USER.md` | Who you are: one fact per line, capped by `user_limit`. |
| `memory/MEMORY.md` | Facts, preferences and decisions, capped by `memory_limit`. |
| `memory/journal/YYYY-MM-DD.md` | One file a day, one timed line per entry, append-only. |
| `memory/.git` | The memory's history, when `[memory] git = true`. |
| `fronde.db` | The database: one SQLite file, created at the first start and updated to the running version's schema at each start, all of an update or none of it. It holds the conversations, one thread each (see below), the web sign-ins (sessions), kept across restarts, the component approvals you gave (see [Configuration](configuration.md#kind--component)), the secrets you stored, encrypted (see [Store a secret](../how-to/store-a-secret.md)), the requests agents made to you, with who answered what and when (see [Requests and cards](../explanation/requests.md)), the memory's search index, rebuilt from the memory files at each start, and small values in a key-value table: each provider's model list (`provider/<id>`, `models`) and each account's latest usage reading (`account/<name>`, `usage`: the subscription's windows, see [Usage and quotas](../explanation/subscriptions.md#usage-and-quotas); a reading an earlier version kept under `provider/<name>` is moved there at start, the later of the two kept when both exist, and removed when no account has that name), kept across restarts. Expired sessions and other expired entries are deleted at start and every hour. |
| `fronde.db-wal`, `fronde.db-shm` | SQLite's write-ahead log and its index, beside the database while Fronde runs. The log holds the latest changes: never delete `fronde.db` without it, and see below for backups. |
| `master.key` | Only from an earlier version, which kept the master key here. Fronde moves it to its place beside the configuration (or to `[secrets] master_key`) at the next start or `fronde secret` command, and logs it. If both places hold a key and they differ, Fronde refuses to start: keep the one that opens the stored secrets (`fronde check` says) and move the other away. |
| `fronde.lock` | Locked by `fronde serve` while it runs, and by `fronde secret rotate-key` and `move-key`, so that none of them runs beside another on this data directory; it holds the process id and command of the last one. The system releases the lock when the process ends, crash included; deleting the file while nothing runs is harmless. |
| `master-key.lock` | An empty file that two `fronde secret set` commands storing a first secret at once lock in turn, so that they make one master key. It holds nothing; deleting it while no command runs is harmless. |
| `*.imported` | Files an earlier version kept the conversation in, once imported into the database (see below). Fronde no longer reads them: keep them as a backup or delete them. |
| `agent/` | The main thread's agent's working directory. It starts empty. |
| `home/` | The main thread's agent's home (`HOME`): Claude's sign-in and settings (`.claude.json`, `.claude/`), its sessions' transcripts, and `tmp/`, its `TMPDIR`. Your own `~/.claude` is never used. See [Sign Fronde in to Claude](../how-to/sign-in-to-claude.md). |
| `threads/<id>/work/` | Thread `<id>`'s agent's working directory, for every thread but the main one. Created private (0700) when that thread's agent first starts. |
| `threads/<id>/home/` | Thread `<id>`'s agent's home, with its `tmp/`: its own Claude settings and transcripts, never another thread's. It starts empty, so the agent signs in with `[agent] oauth_token` only. |
| `threads/<id>.old-<time>/` | A thread directory a new thread found already there (from a database restored from a backup, or made anew), moved aside so that the new thread starts clean. Fronde never reads it: keep it or delete it. |

Earlier versions kept the memory's search index in `index.sqlite`: Fronde deletes it at start.

Each conversation is a thread: a row of the `threads` table (its title, who started it, its
parent if another thread started it, the id of its agent session, resumed after a restart when
the agent allows it, and when it was archived) and a stream of numbered entries, with the log id
pages use to notice a new conversation. The chat is thread 1, over the stream `conversation`.
Earlier versions kept the agent's session id apart, in a key-value row: the first start of this
version moves it to thread 1's row. Each thread's agent works in a directory of its own and has
a home of its own: `agent/` and `home/` for the main thread, as before threads, and
`threads/<id>/work/` and `threads/<id>/home/` for the others. Its sandbox lets it read and write
those two and no other thread's (see [Security](../explanation/security.md#threads)).

Earlier versions also kept the conversation in `conversation.jsonl` and the agent's session id in
`agent.session` (`jarvis.jsonl` and `jarvis.session` before that). When the database has no
conversation yet, Fronde imports them at start, in one transaction, then renames each one
`*.imported`. If the import fails, nothing is imported, the files stay as they are and the next
start tries again. A line of the log that is not a conversation entry stops the import: the error
names the file and the line; fix or delete that line, then start Fronde again. A last line cut
short by a crash was never shown and is skipped. If a crash came between the import and the
renames, the next start finishes the renames of the files the database already holds; any other
such file left beside an existing conversation is not imported, and a warning names it.

To start a new conversation, stop Fronde, delete `fronde.db`, `fronde.db-wal` and `fronde.db-shm`,
and start it again. This also drops the memory's search index, rebuilt at the start, signs every
browser out, forgets the component approvals stored there (hashes in the configuration stay) and
drops the stored secrets: set them again afterwards (the master key can stay). Fronde never
imports a `*.imported` file again, but it imports a `conversation.jsonl` or `agent.session` (or
`jarvis.*`) still there: delete those too. A command that starts a new conversation without
touching the rest is planned.

To back up, stop Fronde, then copy the whole directory; the master key is not in it (see
below). A copy taken while Fronde runs can be inconsistent: SQLite may move changes from the
`-wal` file into the database between two file copies. For a copy without stopping Fronde, let
SQLite make it: `sqlite3 fronde.db ".backup /path/to/fronde.db"`, then copy the other files. A
`fronde backup` command is planned. The database refuses to open after a newer Fronde used it: run that version
again or restore a backup. Sign-ins are stored by the hash of their id; the first start of the
version that began hashing them signs every browser out once, and so does restoring a backup
made before it.

The master key and `fronde.db` together give the stored secrets to whoever holds them. A copy of
the data directory, as above, or a snapshot of the volume that holds it, contains `fronde.db`
but not the key, which lives beside the configuration file: it reveals no stored secret, and the
key alone opens nothing. Back the key up once, apart from those copies, in a password manager
for instance, and keep it out of copies you send elsewhere. To restore, put both back. Fronde
checks the key when it starts: with a `fronde.db` restored beside a different key file, it
refuses to start and says the key is wrong; with no key file while secrets are stored, it
refuses to start too, and makes no new key until you put the right one back or remove every
stored secret (`fronde secret remove`).

The memory files are plain Markdown: you can read and edit them by hand. An edit by hand is
indexed at the next start, and committed with the next change Fronde makes.

The agent's home is `home/` everywhere: natively, and in the container image, where `HOME` is
`/data/home` already. Its `~/.claude` (its sign-in and session files) lives with the rest of the
data.

# Data directory

Everything Fronde keeps lives under `[data] dir`. Back up that directory, and you have backed up
Fronde.

| Path | Holds |
| --- | --- |
| `memory/USER.md` | Who you are: one fact per line, capped by `user_limit`. |
| `memory/MEMORY.md` | Facts, preferences and decisions, capped by `memory_limit`. |
| `memory/journal/YYYY-MM-DD.md` | One file a day, one timed line per entry, append-only. |
| `memory/.git` | The memory's history, when `[memory] git = true`. |
| `fronde.db` | The database: one SQLite file, created at the first start and updated to the running version's schema at each start, all of an update or none of it. It holds the chat's conversation (its numbered entries and the log id pages use to notice a new conversation), the id of the agent's session, resumed after a restart when the agent allows it, the web sign-ins (sessions), kept across restarts, the component approvals you gave (see [Configuration](configuration.md#kind--component)), the secrets you stored, encrypted (see [Store a secret](../how-to/store-a-secret.md)), and the memory's search index, rebuilt from the memory files at each start. Expired sessions and other expired entries are deleted at start and every hour. |
| `fronde.db-wal`, `fronde.db-shm` | SQLite's write-ahead log and its index, beside the database while Fronde runs. The log holds the latest changes: never delete `fronde.db` without it, and see below for backups. |
| `master.key` | The master key that encrypts the stored secrets: one line, `fronde-key-v1:` and the key. Created with the first stored secret, mode 0600, and refused if other users can read it or another user owns it. Without it the stored secrets cannot be read, and Fronde does not make a new one while any is stored: back it up (see below). |
| `*.imported` | Files an earlier version kept the conversation in, once imported into the database (see below). Fronde no longer reads them: keep them as a backup or delete them. |
| `agent/` | The agent's working directory. It starts empty. |

Earlier versions kept the memory's search index in `index.sqlite`: Fronde deletes it at start.

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
and start it again. This also drops the memory's search index, rebuilt at the start, signs
every browser out, forgets the component approvals stored there (hashes in the configuration
stay) and drops the stored secrets: set them again afterwards (`master.key` can stay). Fronde never imports a `*.imported` file again, but it imports a `conversation.jsonl`
or `agent.session` (or `jarvis.*`) still there: delete those too. A command that starts
a new conversation without touching the rest is planned.

To back up, stop Fronde, then copy the whole directory. A copy taken while Fronde runs can be
inconsistent: SQLite may move changes from the `-wal` file into the database between two file
copies. For a copy without stopping Fronde, let SQLite make it:
`sqlite3 fronde.db ".backup /path/to/fronde.db"`, then copy the other files. A `fronde backup`
command is planned. The database refuses to open after a newer Fronde used it: run that version
again or restore a backup. Sign-ins are stored by the hash of their id; the first start of the
version that began hashing them signs every browser out once, and so does restoring a backup
made before it.

`master.key` and `fronde.db` together give the stored secrets to whoever holds them. Keep a
copy of `master.key` apart from the backups of the rest, in a password manager for instance, and
leave it out of copies you send elsewhere: a backup without it reveals no stored secret, and the
key alone opens nothing. To restore, put both back. A `fronde.db` restored with a different key
file is refused with a message saying the key is wrong; one restored without any key file is
refused too, and Fronde makes no new key until you put the right one back or remove every stored
secret (`fronde secret remove`).

The memory files are plain Markdown: you can read and edit them by hand. An edit by hand is
indexed at the next start, and committed with the next change Fronde makes.

In the container image, `HOME` is `/data/home`, so the agent's `~/.claude` (its sign-in and
session files) lives on the same volume.

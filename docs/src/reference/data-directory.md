# Data directory

Everything Fronde keeps lives under `[data] dir`. Back up that directory, and you have backed up
Fronde.

| Path | Holds |
| --- | --- |
| `memory/USER.md` | Who you are: one fact per line, capped by `user_limit`. |
| `memory/MEMORY.md` | Facts, preferences and decisions, capped by `memory_limit`. |
| `memory/journal/YYYY-MM-DD.md` | One file a day, one timed line per entry, append-only. |
| `memory/.git` | The memory's history, when `[memory] git = true`. |
| `fronde.db` | The database: one SQLite file, created at the first start and updated to the running version's schema at each start. It holds a key-value table today; the memory's search index, the conversation, the agent's session and the web sign-ins move into it later (planned). |
| `fronde.db-wal`, `fronde.db-shm` | SQLite's write-ahead log and its index, beside the database while Fronde runs. The log holds the latest changes: never copy or delete `fronde.db` without it while Fronde runs. |
| `index.sqlite` | Full-text index of the memory. Rebuilt from the files at each start: safe to delete. |
| `conversation.jsonl` | The chat's conversation log, one JSON event per line. To start a new conversation, stop Fronde and delete it with `agent.session`. |
| `agent.session` | The id of the agent's session, resumed after a restart when the agent allows it. |
| `agent/` | The agent's working directory. It starts empty. |

To back up, copy the whole directory. Copy the database on its own only while Fronde is stopped,
or together with its `-wal` file. The database refuses to open after a newer Fronde used it: run
that version again or restore a backup.

The memory files are plain Markdown: you can read and edit them by hand. An edit by hand is
indexed at the next start, and committed with the next change Fronde makes.

In the container image, `HOME` is `/data/home`, so the agent's `~/.claude` (its sign-in and
session files) lives on the same volume.

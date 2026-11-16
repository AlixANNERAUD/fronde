# Data directory

Everything Fronde keeps lives under `[data] dir`. Back up that directory, and you have backed up
Fronde.

| Path | Holds |
| --- | --- |
| `memory/USER.md` | Who you are: one fact per line, capped by `user_limit`. |
| `memory/MEMORY.md` | Facts, preferences and decisions, capped by `memory_limit`. |
| `memory/journal/YYYY-MM-DD.md` | One file a day, one timed line per entry, append-only. |
| `memory/.git` | The memory's history, when `[memory] git = true`. |
| `index.sqlite` | Full-text index of the memory. Rebuilt from the files at each start: safe to delete. |
| `conversation.jsonl` | The chat's conversation log, one JSON event per line. To start a new conversation, stop Fronde and delete it with `agent.session`. |
| `agent.session` | The id of the agent's session, resumed after a restart when the agent allows it. |
| `agent/` | The agent's working directory. It starts empty. |

The memory files are plain Markdown: you can read and edit them by hand. An edit by hand is
indexed at the next start, and committed with the next change Fronde makes.

In the container image, `HOME` is `/data/home`, so the agent's `~/.claude` (its sign-in and
session files) lives on the same volume.

# Memory

Fronde's memory is a folder of Markdown files, versioned with git and searched through a
full-text index. It is meant to be read and fixed by a person as easily as by the agent.

## Three files, two kinds

- **The core**: `USER.md`, who you are, and `MEMORY.md`, facts, preferences and decisions. Each
  line is one fact. The core is small and capped (2,000 and 4,000 characters by default), and the
  hub hands it whole to every agent that connects, in its instructions. The agent never has to
  search for who you are.
- **The journal**: one file a day under `journal/`, one timed line per entry, never capped and
  append-only. It is not loaded up front: the agent finds old entries with `memory_search`.

## Why a cap

A core that grows forever would fill the model's context and dilute what matters. When a change
would go over the cap, `memory_add` refuses it and asks the agent to merge or prune with
`memory_replace`. From 80% full, every write carries a hint to tidy up soon. Keeping the core
short becomes the agent's job, done while it still has the context to do it well.

## Why plain files

Markdown files can be read, edited, backed up and moved without Fronde. If the agent saves
something wrong, you open `MEMORY.md` and fix it. A file edited by hand to beyond its cap is
accepted, and may always shrink.

Writes go to a temporary file renamed over the old one, so a crash never leaves half a file, and
readers never wait for writers.

## Why git

With `[memory] git = true`, every change is a commit, such as `memory: add MEMORY.md`. You can
see what the agent learned and when, and undo it. Fronde commits with its own identity and
ignores your git hooks and signing settings, so a commit never blocks on them. Git is best effort:
if it fails, the memory is still written, and the log says why.

## Why full-text search

`memory_search` uses SQLite's FTS5, one row per non-empty line, with accents ignored and prefix
matching on each word. It finds "train" in "trains" and "été" from "ete", needs no model and no
network, and is rebuilt from the files at each start. The index is a cache: the files are the
truth.

## What is never saved

Nothing read from an external source, a web page, an email or a document, is saved to memory
automatically. The agent saves only what you said or confirmed. See
[Security model](security.md#untrusted-content).

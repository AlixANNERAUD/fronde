# Tools

These are the tools the hub serves at `/mcp`, to its own agent and to any other client. Tool
names match `^[a-z][a-z0-9_]{0,63}$`.

## Results

Every tool answers with one text block holding JSON:

- `{"result": …}` on success, with an optional `"next"`: a hint about what to do next, such as
  "Memory is nearly full".
- `{"error": "…"}` on failure, flagged as a tool error. The message always says how to fix the
  call, for example "call calendar_list to get current uids".

Wrong arguments are a tool error naming the faulty field, never a protocol error.

## Instructions

When a client connects, the hub sends it instructions: when to use tools, never to save content
read from the web, an email or a document, the names of the deferred tools grouped by source, and
the current `USER.md` and `MEMORY.md`. The client does not need to search memory for those.

## Tool search

| Tool | Arguments | Does |
| --- | --- | --- |
| `search_tools` | `need` (text), `limit` (1 to 10, default 5) | Finds deferred tools by need, best first, and returns their names, descriptions and input schemas. |
| `use_tool` | `name`, `arguments` (object) | Calls a tool found with `search_tools`. |

Deferred tools are hidden from the tool list to keep the agent's context small. Today the
deferred tools are those of upstream MCP servers, named `<server>_<tool>`. The search is a
full-text search over names and descriptions, accents ignored.

## Memory

| Tool | Arguments | Does |
| --- | --- | --- |
| `memory_add` | `file` (`user`, `memory` or `journal`), `text` | Adds one line. `user`: who you are, in `USER.md`. `memory`: facts, preferences, decisions, in `MEMORY.md`. `journal`: what happened today, with the time, in `journal/YYYY-MM-DD.md`. |
| `memory_replace` | `file` (`user` or `memory`), `old`, `new` | Replaces the exact text `old`, which must appear once. An empty `new` removes the whole line. The journal is append-only. |
| `memory_search` | `query`, `limit` (1 to 50, default 10) | Searches every memory file, journal included. Returns the file, the line number and the line. |

`memory_add` and `memory_replace` return the file, its size and its limit. A change that would
take `USER.md` or `MEMORY.md` over its limit is refused, with a hint to merge or prune lines
first. See [Memory](../explanation/memory.md).

## Calendar

Brought by the calendar component, when it is configured (see
[Configure the calendar](../how-to/configure-calendar.md)). Dates and times are local, in
`[data] timezone`: `YYYY-MM-DD` for a day, `YYYY-MM-DDTHH:MM` for a time.

| Tool | Arguments | Does |
| --- | --- | --- |
| `calendar_list` | `from` (default today), `to` (inclusive, default `from` + 7 days) | Lists events, recurring ones expanded, sorted by start. |
| `calendar_create` | `title`, `start`, `end`, `location`, `description` | Adds an event. A date makes a whole-day event, and an end date is the last day included. Default end: one hour after the start, or the same day. |
| `calendar_update` | `uid`, then any of `title`, `start`, `end`, `location`, `description` | Changes an event. Moving `start` alone keeps the duration. |
| `calendar_delete` | `uid` | Deletes an event. |

Events come back as `uid`, `title`, `start`, `end`, `location` and `description`.

## Reminders

From the calendar component too. Reminders are tasks (`VTODO`) with a due time, in the reminders
collection.

| Tool | Arguments | Does |
| --- | --- | --- |
| `reminders_create` | `text`, `at` | Adds a reminder. A date alone means 09:00 that day. |
| `reminders_list` | `include_done` (default false) | Lists reminders, soonest first. |
| `reminders_done` | `uid` | Marks a reminder as done. |
| `reminders_delete` | `uid` | Deletes a reminder. |

Reminders come back as `uid`, `text`, `at` and `done`. Tasks without a due time are not listed.
Recurring events and tasks are never changed or deleted: the tools say to do it in a calendar
app.

## Upstream tools

Each tool of an upstream MCP server becomes `<server>_<tool>`, deferred, with the server's own
description and schema. Its result passes through: structured content as it is, otherwise the
text blocks joined, with images, audio and resources replaced by a placeholder.

## Agent built-in tools

Fronde's own agent also gets Claude Code's `WebSearch` and `WebFetch`, unless
`[agent] builtin_tools` says otherwise. Other clients of `/mcp` do not get them from Fronde.

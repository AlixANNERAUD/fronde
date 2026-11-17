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
deferred tools are `code_run_python` and those of upstream MCP servers, named
`<server>_<tool>`. The search is a
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

## Python

`code_run_python`, deferred, from the source `python`. Served only when `[python]` is configured
(see [Enable Python](../how-to/enable-python.md)). It runs short Python 3.15 for exact
arithmetic, dates and aggregates over data, in a sandbox: it is not for writing programs.

| Argument | Type | Meaning |
| --- | --- | --- |
| `code` | string | Required. Python source. Its last expression is the result; without one, the variable `result`, if set. |
| `data` | any JSON | Optional. The code reads it as the global `data`. |

The code also has `TZ`, a `zoneinfo.ZoneInfo` for `[data] timezone` (`datetime.now()` is UTC).

On success the result is an object with `value`, the result as plain JSON, and `stdout`, what the
code printed; each is left out when empty. The value is converted for JSON: `Decimal` and
`Fraction` become strings (`"0.3"`, `"1/3"`), dates and times ISO 8601 strings, sets and tuples
lists, NaN and infinities the strings `"nan"`, `"inf"` and `"-inf"`, keys strings, and any other
object its `repr`, cut at 1000 characters.

A Python exception is a tool error naming the exception and the line of `code` it came from,
such as `ZeroDivisionError: division by zero (line 3)`, followed by what the code printed before
it. `sys.exit()` with no code or 0 is not an error.

| Limit | Value | Past it |
| --- | --- | --- |
| CPU | about 10 s of running time | The run is stopped: "used up its compute budget". |
| Wall clock | 30 s | The run is stopped: "timed out". |
| Memory | 256 MiB | `MemoryError`. |
| Result | 60 KiB of JSON | An error asking for an aggregate rather than the rows. |
| Printed output | the first 16 KiB | Cut, with `"stdout_truncated": true`. |

What is there: the standard library, including `json`, `math`, `statistics`, `decimal`,
`fractions`, `datetime`, `zoneinfo` (with the full time zone database), `collections`,
`itertools`, `functools`, `re`, `csv`, `random`, `heapq`, `bisect`, `calendar`, `difflib`,
`unicodedata`, `hashlib` and `base64`. What is not: third-party packages (numpy, pandas), C
extensions, `sqlite3`, `zlib` and the compression modules, files other than the standard library
(read-only), the network, processes and threads. Nothing is kept from one call to the next.

Sum by month:

```json
{
  "code": "from collections import defaultdict\nfrom decimal import Decimal\ntotals = defaultdict(Decimal)\nfor row in data:\n    totals[row['date'][:7]] += Decimal(row['amount'])\ndict(sorted(totals.items()))",
  "data": [
    {"date": "2026-10-03", "amount": "12.50"},
    {"date": "2026-10-19", "amount": "7.25"},
    {"date": "2026-11-02", "amount": "40.00"}
  ]
}
```

```json
{"result": {"value": {"2026-10": "19.75", "2026-11": "40.00"}}}
```

Days between two dates:

```json
{"code": "from datetime import date\n(date(2026, 12, 25) - date(2026, 11, 17)).days"}
```

```json
{"result": {"value": 38}}
```

## Upstream tools

Each tool of an upstream MCP server becomes `<server>_<tool>`, deferred, with the server's own
description and schema. Its result passes through: structured content as it is, otherwise the
text blocks joined, with images, audio and resources replaced by a placeholder.

## Agent built-in tools

Fronde's own agent also gets Claude Code's `WebSearch` and `WebFetch`, unless
`[agent] builtin_tools` says otherwise. Other clients of `/mcp` do not get them from Fronde.

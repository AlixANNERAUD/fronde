# Tools

These are the tools the hub serves at `/mcp`, to its own agents and to any other client. Tool
names match `^[a-z][a-z0-9_]{0,63}$`. The hub's own tools use the prefixes `code_`, `hub_`,
`memory_`, `search_`, `threads_`, `use_` and `user_`, which no upstream server or component may
take.

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

## Asking the user

Served only with `[web]`: the question shows as a card in Fronde's chat, the one place where the
owner answers.

| Tool | Arguments | Does |
| --- | --- | --- |
| `user_ask` | `questions` (1 to 4), `timeout_ms` (1000 to 3600000, default 600000), `client_request_id` (optional, 1 to 128 characters) | Asks the user multiple-choice questions on one card and waits for the answers. |
| `user_secret` | `label`, `reason`, `placeholder` (optional), `url` (optional), `remember` (optional), `timeout_ms`, `client_request_id` (as for `user_ask`) | Asks the user for one secret on a private card and waits; returns where it was kept, never the value. |

Each question has `question` (the full text), `header` (a chip of at most 12 characters, such as
"Appli"), `multi_select` (default `false`) and `options`: 2 to 4 of `{ label, description,
preview?, recommended? }`. Labels are unique within a question. At most one option is
`recommended`, and it comes first: the card marks it *Recommandé* and never picks it for the user.
A `preview` is text shown in monospace while its option is picked: beside the options on a wide
screen, under the option on a phone. Every question also offers *Autre…* with free text, and a
note (*Ajouter une précision*).

The call blocks until the request ends:

```json
{"result": {"status": "answered", "answers": [
  {"question": "Quelle appli installer ?", "selected": ["Seerr"], "other": null, "note": null}
]}}
```

`selected` holds labels of the options offered, never anything else; `other` is the free text,
`note` the precision, both `null` when empty. A request that ends without an answer returns
`{"status": "declined"}`, `"expired"` (after `timeout_ms`) or `"cancelled"`, each with a `next`
saying what to do: a declined question is not asked again in the same turn.

Give the same `client_request_id` when retrying a call that failed on the way: the retry joins the
first request rather than showing a second card, and gets its answer, even one given already. A
retry joins only a request with the very same questions, for the same user, and not cancelled:
from Fronde's assistant, one of the same turn; from an outside client, one still pending or within
its `timeout_ms`. Otherwise the call opens a new request, or, while the request holding that id is
still pending, fails with an error asking for a new `client_request_id`.

A question from one of Fronde's own agents is headed *Question de Fronde* and shown in that
agent's thread; one from an outside client, shown in the main thread,
is headed with the name the client gave (`clientInfo.name`), quoted and marked, such as
*Question de « Codex » (client MCP)*. An outside client named *Fronde* or *assistant* (in any
case), with a name longer than 40 characters or holding a control character, gets an error asking
it to connect under another name. Texts are capped: `question` at 300 characters, `label` at 60,
`description` at 200, `preview` at 4000. None may hold a hidden character (a control, format,
zero-width or bidirectional character, a separator, a tag character, a variation selector, a
private-use or unassigned code point); only `preview` may hold line breaks and tabs. The error
names the character as `U+XXXX`. Never use it for a password or a key. An MCP client that cuts tool calls short needs a longer timeout
than the question's: Codex, for instance, gives up after `tool_timeout_sec` (60 s by default).
A call the client cancels closes its card as cancelled, and so does the end of the agent's
turn, or *Stop*, for the questions that turn asked.
See [Requests and cards](../explanation/requests.md).

### `user_secret`

One secret per call: a password, a token, a key. The card shows `label` (at most 60 characters)
above a masked box, `reason` (at most 300) above it, and `placeholder` (at most 60) in the empty
box. `url`, when given, is the sign-in or setup page where the user finds or makes the secret:
the card shows it as a button naming its host. It must be an `https://` address with a host and
no user name or password before it (`https://site@evil.example` is refused): `http:`,
`javascript:`, `data:` and anything else are refused. The texts follow `user_ask`'s rules: no
hidden character, no line break. The same outside client names are
refused.

The user picks how long the secret is kept:

- **For this conversation** (always offered, the default). The secret stays in Fronde's memory
  only, for the asker that asked, in this conversation (any later turn of it too), for one hour,
  and is used up by its first use. The call returns a reference:

  ```json
  {"result": {"status": "saved", "secret_ref": "sr_k3Jq…", "expires_at": "2026-11-17T11:00:00Z"},
   "next": "Kept for this conversation only, … No tool accepts a secret_ref yet: …"}
  ```

  **No tool accepts a `secret_ref` yet.** The first ones to take one will be integration setup
  from the chat and commands that read a secret from a file (both **planned**); until then the
  reference expires unused, and the `next` says so. To give a component a credential now, use
  `remember`.

- **Kept for an origin**, offered only when the call gives `remember: { name, origin }`, and only
  to an admin (today the owner). `name` is lower-case letters, digits and `_`, starting with a
  letter, at most 64 characters; `origin` is `scheme://host[:port]`, over https, or http to this
  machine, and is stored normalised (`HTTPS://Cloud.Example:443` becomes
  `https://cloud.example`). The secret is sealed in `fronde.db` as `fronde secret set` would
  store it, sent only to that origin, and the call returns its name:

  ```json
  {"result": {"status": "saved", "stored": "nextcloud", "origin": "https://cloud.example"}}
  ```

  A component then uses it as `{ stored = "nextcloud" }` in its `[[sources.credentials]]` for
  that origin; the owner writes that in the configuration. A name already stored for another
  origin is refused, before the card is shown and again when the answer comes: a stored secret is
  never moved to another origin behind the owner's back. The error says only that the name is
  already used, never for which origin. The same name for the same origin replaces the value, and
  the card says so to the owner.

The value never reaches the agent, the conversation, the request's row, a log or any file but the
sealed row. Declined, expired and cancelled calls return as `user_ask`'s do. `user_secret` is
served only while the chat runs, beside `user_ask`. See
[Requests and cards](../explanation/requests.md#secret-cards).

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
object its `repr`, cut at 1000 characters. An integer beyond ±2<sup>53</sup> becomes a string of
its digits (`math.factorial(25)` is `"15511210043330985984000000"`), since a JSON number that
large loses digits when read as a double; smaller integers stay numbers.

A Python exception is a tool error naming the exception and the line of `code` it came from,
such as `ZeroDivisionError: division by zero (line 3)`, followed by what the code printed before
it. `sys.exit()` with no code or 0 is not an error; any other exit is, and its output is not a
result.

| Limit | Value | Past it |
| --- | --- | --- |
| CPU | about 10 s of running time | The run is stopped: "used up its compute budget". |
| Wall clock | 30 s, waiting for a turn included | The run is stopped: "timed out", or "busy with other runs" if it never got a turn. |
| Runs at once | 2 | Others wait for a turn. |
| Code and data | 16 MiB of JSON together | Refused before running, asking for fewer rows or an aggregate. |
| Open files | 128 at once | The run is stopped, asking to close each file once read. |
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

## Threads

Served only with `[web]`, and only to a thread's agent: an outside MCP client is not offered
them and is refused them (letting outside clients start threads is **planned**). They let an agent
start sub-agents and follow them; see [Threads and sub-agents](../explanation/threads.md).

| Tool | Arguments | Does |
| --- | --- | --- |
| `threads_start` | `task`, `title` (optional, at most 80 characters), `provider` (optional), `model` (optional), `wait` (default `false`), `timeout_secs` (1 to 3600, default 600), `client_request_id` (optional, 1 to 128 characters) | Starts a sub-agent whose only context is `task`, and returns `{ thread, title, status }`; with `wait`, its answer. |
| `threads_send` | `thread`, `text`, `wait`, `timeout_secs` | Sends a sub-agent a new message, as a new turn; refused while it is in a turn. |
| `threads_read` | `thread`, `after` (default 0), `limit` (1 to 50, default 20), `max_chars` (100 to 8000, default 4000) | Reads a sub-agent's thread, oldest first: its messages (`user`), its answers (`assistant`, a turn's text together), Fronde's notices (`notice`) and each turn's end (`turn_ended`, with its `outcome`). Each item has its `seq`; a text longer than `max_chars` is cut and marked `truncated`. When there is more, `next_after` is the `after` of the next page. |
| `threads_list` | none | The sub-agents you started, newest first (at most 50, `more` counting the rest), each with its `thread`, `title`, `status`, `provider` and `model`, `waiting_for_user` when cards wait in it, `archived` when it is; and `providers`: each provider a sub-agent may run on, with the models its agent offered when it last started. |
| `threads_cancel` | `thread`, `reason` (optional, one line, at most 200 characters) | Stops a sub-agent's running turn, its pending cards and its own sub-agents' turns; `reason` is shown in its thread. A finished turn keeps its result. Returns whether it was in a turn. |

- **Who.** A thread's agent is known by its token only; nothing in a call names the caller. It
  reaches only the threads it started (and theirs): any other thread, its parent and siblings
  among them, gets the same error as a thread that does not exist,
  "no such thread among the sub-agents you started".
- **Depth.** A sub-agent is not offered `threads_start` and `threads_send`, and is refused them:
  the threads you start may delegate, theirs may not.
- **Provider and model.** `provider` is one of `threads_list`'s (only `claude` today; `codex` and
  `gemini` are refused as planned). `model` is one of the models the provider's agent offered,
  as `threads_list` shows them; another is refused with the list. Without them, the parent's
  provider and the provider's default model.
- **Waiting.** Without `wait`, the call returns at once and the sub-agent works on its own: read
  its answer later with `threads_read`. With `wait`, the call returns when its turn ends, with
  `{ thread, status, outcome, answer }`: `answer` is the text of that turn after its message, at
  most 8000 characters (`truncated` when cut). It returns earlier with `status:
  "waiting_for_user"` when a card opens in the sub-agent's thread, or `status: "running"` when
  `timeout_secs` passes or the call is given up; the sub-agent goes on either way. Its result
  waking the parent on its own is **planned**.
- **Retries.** A call retried in the same turn with the same `client_request_id` and the same
  arguments joins the sub-agent the first one started (`joined: true`). The same id for another
  task, title, provider or model, or in another turn, is refused while that sub-agent is not
  archived: a new task needs a new id.
- **Untrusted text.** What a sub-agent writes may come from anything it read. It reaches the
  caller with its hidden characters (format, bidirectional, zero-width) shown as U+FFFD, capped,
  and with a `next` saying it is data, not instructions.

## Upstream tools

Each tool of an upstream MCP server becomes `<server>_<tool>`, deferred, with the server's own
description and schema. Its result passes through: structured content as it is, otherwise the
text blocks joined, with images, audio and resources replaced by a placeholder. A form the server
asks for while the tool runs is a card for the admin (see
[Add upstream MCP servers](../how-to/add-upstream-mcp-servers.md#when-a-server-asks-you-something)),
and the tool waits for it. A call its caller gives up is dropped, and rmcp tells the server so;
the server's card closes once no other call to it is in flight.

## Agent built-in tools

Fronde's own agent also gets Claude Code's `WebSearch` and `WebFetch`, unless
`[agent] builtin_tools` says otherwise, and always `AskUserQuestion`, which is never allowed up
front: each use becomes a form card in the chat (see
[Forms from the agent](../explanation/requests.md#forms-from-the-agent)). Other clients of `/mcp`
do not get them from Fronde.

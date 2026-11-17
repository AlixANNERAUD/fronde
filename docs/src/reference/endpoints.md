# Pages and endpoints

## Always served

| Method and path | Answer |
| --- | --- |
| `GET /health` | `200 ok`. No authentication. |
| `/mcp` | The hub's MCP server, over Streamable HTTP. Requires `Authorization: Bearer <token>`, else `401`: `server.token`, or the token Fronde gives one of its agent processes, which names that agent's thread and is refused once the process is gone. |

`/mcp` does not check the `Host` header: every request must carry the token, which a page on
another site cannot know.

## With `[web]`

| Method and path | Answer |
| --- | --- |
| `GET /` | The chat of the main thread. Without a session, a redirect to `/sign-in`. |
| `GET /t/<id>` | The chat of thread `<id>`. Without a session, a redirect to `/sign-in`. |
| `GET /threads` | The thread list, full screen (on a wide screen it also sits beside the chat). Without a session, a redirect to `/sign-in`. |
| `GET /new` | An empty chat whose first message starts a thread. Without a session, a redirect to `/sign-in`. |
| `GET /sign-in` | The sign-in page. |
| `POST /auth/sign-in` | Form field `password`. Right: a new session and a redirect to `/`. Wrong: a redirect to `/sign-in?error=1`, after one second. |
| `POST /auth/sign-out` | Ends the session, then redirects to `/sign-in`. |
| `GET /ws` | The chat's WebSocket, described below. |
| `GET /assets/…` | The interface's files, cached for a year: their names change with their content. |
| Other paths | Files of the built interface, else `404`. |

These pages and `/sign-in` serve the same page, the Dioxus app, which shows the right screen. When the
interface is not built, they answer `503` and say to run `mise run web-build`. Files are sent
compressed with Brotli or gzip when the browser accepts it.

### Sign-in throttle

After 5 wrong passwords in a row, sign-in is refused for 60 seconds, for every client at once,
with a redirect to `/sign-in?error=2`. There is one password, so the count is global.

### Sessions

The session cookie is `fronde_session`: `HttpOnly`, `SameSite=Lax`, and `Secure` when
`secure_cookie = true`. A session ends after 30 days without a signed-in request; each request
renews it. Sessions are kept in `fronde.db`, so they survive a restart. Signing out deletes the
session; an expired one is refused at once and its row deleted within the hour.

### Security headers

The pages, assets and forms carry:

| Header | Value |
| --- | --- |
| `Content-Security-Policy` | `frame-ancestors 'none'` |
| `X-Content-Type-Options` | `nosniff` |
| `Referrer-Policy` | `same-origin` |

### The chat WebSocket

`GET /ws` upgrades only when the `Origin` header names the same host as `Host`, else `403`.
Without a session, it upgrades and closes at once with code `4401`, which the page reads as
"sign in again": a refused upgrade would look like a network failure.

Messages are JSON objects tagged by `type`. A page's message is at most 64 KiB; a bigger one
closes the socket.

A socket streams one **thread** at a time, the one its last `hello` named. Every `thread` field
is optional where shown so: without it, a message is for the main thread (thread 1), so a page
that knows nothing of threads talks to the main thread as before, and never gets a thread
message. A thread id is only a selector: each one is checked against the signed-in user, and a
thread that does not exist is refused exactly like one the user may not see. The signed-in owner
is an admin and sees every thread; a user who is not an admin would see only the threads that are
for them (family users are planned).

From the page:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `hello` | `last_seq`, optional `log_id`, optional `thread` | Start streaming `thread`: the entries after `last_seq`, then live ones. A different `log_id` gets the whole log. A new `hello` on the same socket switches the thread streamed. A `hello` that names a thread also subscribes the socket to the thread list (`threads`). |
| `send` | `text`, optional `thread` | A message to the thread's agent. Refused with a notice while a turn runs, and with `thread_refused` when the thread is archived. |
| `cancel` | optional `thread` | Stop the thread's running turn, or its wait for an agent, and cancel the requests it asked that are still open. |
| `answer` | `id`, `response` | Answer the request `id`. `response` is tagged by `type`: `form` with `values` (each question's key to `{ "selected": [labels], "other"?, "note"? }`), `decline`, or `permission` with `choice`, the id of one of a permission card's options. The first answer wins. Request ids are global: a page answers a card of any thread. A late card's answer (a thread's question or secret whose asker no longer waits: Fronde restarted, or the call was cut) then also reaches that thread's agent as a `user` entry in its thread (see [Late answers](../explanation/requests.md#late-answers)). |
| `create` | `client_id`, `text` | Start a thread with its first message `text`, so that no thread is ever empty. Its title is the message's first 60 characters, on one line, without hidden characters. `client_id` (1 to 64 letters, digits, `-` or `_`) is the page's own: the same `client_id` again (a retry after a lost reply) gets the thread the first one made without sending `text` again, while that thread is not archived. |
| `rename` | `thread`, `title` | Rename a thread. `title` is trimmed and must be 1 to 80 characters on one line, without controls, bidi controls or other hidden characters; an archived thread is not renamed. |
| `archive` | `thread`, `archived` | Archive the thread (its turn stopped, its agent let go) or bring it back. An archived thread is read-only: streamed, but refusing messages and renames. The main thread is never archived. |

From the server:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `hello` | `log_id`, `last_seq`, `user`, optional `admin`, optional `thread` | The answer to the page's `hello`, before any entry. `user` is whom the page is signed in as: a closed card names who answered only when it was someone else. `thread` repeats the thread the page named; the entries that follow are its. |
| `entry` | `seq`, `at`, `event` | One numbered, timestamped event of the thread streamed: a user message (`user`: `text`, optional `by` naming who wrote it when it was not the page's user, such as a parent thread for a sub-agent's task), an assistant text chunk, a tool call, a permission request or answer from before permissions were cards (kept so old logs replay), a request to the user (`request`: `id`, `card` of kind `permission`, `form` or `secret`, optional `from`) or its end (`request_closed`: `id`, `outcome`, optional `by`; a card the thread's agent asked closes before its turn's end), the end of a turn, or a notice (among them « En attente : d'autres conversations utilisent l'agent. » when a message waits for an agent, and « Ta réponse partira à l'agent à la fin du tour en cours. » when a late answer waits for the running turn). A late answer shows as a `user` entry without `by`: « Réponse à ta question « … » : … » or « Secret enregistré : … ». |
| `refused` | `id`, `reason` | The page's answer to the request `id` was refused, and the request did not change: it is not this user's, it is closed already, or the answer does not fit the card (`reason` says what to fix, in French). Sent to that page only; never logged. |
| `threads` | `threads` | The threads the user may see, archived ones included, oldest first, each `{ id, title, parent?, status, open_requests, updated_at, archived? }`. Sent after a `hello` that named a thread, then whenever the list changes: a thread made, renamed or archived, a turn started, queued or ended, a card opened or closed. `status` is `idle` (no turn yet), `queued` (waiting for an agent), `running`, or how the last turn ended: `done`, `failed` or `cancelled`. `open_requests` counts the cards waiting for an answer: the « attend ta réponse » badge, the tray's other threads and, with the open cards of the thread shown, the tab's title « (N) Fronde ». `updated_at` is the thread's newest entry or last change. |
| `created` | `client_id`, `thread` | The thread a `create` made; the page says `hello` to it to stream it. |
| `thread_refused` | optional `thread`, optional `client_id`, `reason` | A thread message was refused, for this page only: « Conversation introuvable. » for a thread that does not exist or is not the user's, the thread is archived, the main thread cannot be archived, or the title or `client_id` is not valid (`reason` is French). |
| `usage` | `reserve`, `usage` | How much of the user's own subscription accounts is used, for the meter: `usage` holds one reading per account read so far, each `{ provider, known_since, windows }`, a window `{ kind, used?, status?, resets_at?, as_of }` (`kind` `session`, `weekly`, `weekly_model:<m>`, `extra` or `other:<id>`; `used` a fraction from 0 to 1; `status` `allowed`, `warning` or `rejected`); `reserve` is `[threads] reserve` in percent. Sent after each `hello`, once the log is replayed, and on each new reading; only to an admin, and only once there is a reading. |

When the socket drops, the page reconnects after 1 second, doubling the wait up to 30 seconds,
and at once when the page becomes visible again. It sends its last `seq`, so nothing is shown
twice and nothing is missed.

# Pages and endpoints

## Always served

| Method and path | Answer |
| --- | --- |
| `GET /health` | `200 ok`. No authentication. |
| `/mcp` | The hub's MCP server, over Streamable HTTP. Requires `Authorization: Bearer <token>`, else `401`. |

`/mcp` does not check the `Host` header: every request must carry the token, which a page on
another site cannot know.

## With `[web]`

| Method and path | Answer |
| --- | --- |
| `GET /` | The chat. Without a session, a redirect to `/sign-in`. |
| `GET /sign-in` | The sign-in page. |
| `POST /auth/sign-in` | Form field `password`. Right: a new session and a redirect to `/`. Wrong: a redirect to `/sign-in?error=1`, after one second. |
| `POST /auth/sign-out` | Ends the session, then redirects to `/sign-in`. |
| `GET /ws` | The chat's WebSocket, described below. |
| `GET /assets/…` | The interface's files, cached for a year: their names change with their content. |
| Other paths | Files of the built interface, else `404`. |

`/` and `/sign-in` serve the same page, the Dioxus app, which shows the right screen. When the
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

Messages are JSON objects tagged by `type`.

From the page:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `hello` | `last_seq`, optional `log_id` | Start streaming: the entries after `last_seq`, then live ones. A different `log_id` gets the whole log. |
| `send` | `text` | A message to Fronde. Refused with a notice while a turn runs. |
| `cancel` | | Stop the running turn. |
| `answer` | `id`, `response` | Answer the request `id`. `response` is tagged by `type`: `form` with `values` (each question's key to `{ "selected": [labels], "other"?, "note"? }`), `decline`, or `permission` with `choice` (`allow_once` or `reject_once`) for a permission request of the agent. The first answer wins. |

From the server:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `hello` | `log_id`, `last_seq` | The answer to the page's `hello`, before any entry. |
| `entry` | `seq`, `at`, `event` | One numbered, timestamped event of the conversation: a user message, an assistant text chunk, a tool call, a permission request or answer, a request to the user (`request`: `id`, `card`, optional `from`) or its end (`request_closed`: `id`, `outcome`, optional `by`), the end of a turn, or a notice. |
| `refused` | `id`, `reason` | The page's answer to the request `id` was refused, and the request did not change: it is not this user's, it is closed already, or the answer does not fit the card (`reason` says what to fix, in French). Sent to that page only; never logged. |

When the socket drops, the page reconnects after 1 second, doubling the wait up to 30 seconds,
and at once when the page becomes visible again. It sends its last `seq`, so nothing is shown
twice and nothing is missed.

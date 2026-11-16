# AGENTS.md

jarvis (code name) is a personal agent hub written in Rust: it drives coding agents through their
own subscriptions over ACP, serves its memory, calendar and tools over MCP, and offers a web chat.

## Layout

| Crate | Role |
| --- | --- |
| `crates/hub` (`jarvis-hub`) | Tools registry and search, memory, CalDAV calendar and reminders, upstream MCP servers, the MCP server |
| `crates/protocol` (`jarvis-protocol`) | Types shared by the conversation log, the server and the frontend (`Event`, `Entry`); serde and chrono only |
| `crates/agent` (`jarvis-agent`) | The ACP client that drives the coding agent (Claude Code through its ACP adapter); an in-process fake agent behind the `testing` feature |
| `crates/client` (`jarvis-client`) | The web UI's logic without any framework: `chat.rs` turns server messages into what the chat shows, `connection.rs` is the WebSocket reconnection state machine; depends on the protocol only |
| `crates/app` (`jarvis-app`) | The Leptos web chat: sign-in and chat pages, the `ChatPanel` island and its WebSocket; `chat_model.rs` turns log entries into what the chat shows |
| `crates/frontend` (`jarvis-frontend`) | The browser bundle (wasm32): hydrates the app's island |
| `crates/web` (`jarvis-web`) | The Dioxus web interface, a single-page app built by dx: sign-in and chat pages; `socket.rs` runs the chat's WebSocket with `jarvis-client`, behind `cfg(target_arch = "wasm32")` so the pages render natively in tests |
| `crates/server` | The `jarvis` binary and library: configuration check, healthcheck, HTTP routes, `/mcp` behind a bearer token, the chat pages and `/ws` |

`jarvis-app` builds with `ssr` for the server and with `hydrate` for the browser, never both:
server-side Cargo commands leave out `jarvis-frontend` and `jarvis-web`
(`$JARVIS_SERVER_PACKAGES`). UI classes come from Rust/UI
(`crates/app/src/components/LICENSE-rust-ui`); styles are Tailwind in `style/main.css`. The
server reads the built site from `JARVIS_SITE_ROOT` (default `target/site`).

Lint and tests run `jarvis-web` on its own, natively and for wasm32. It embeds
`crates/web/assets/tailwind.css` at compile time: the hidden `web-css` task generates it from
`crates/web/style/tailwind.css` with mise's Tailwind, and `lint`, `test`, `web-build` and
`web-dev` run it first.

## Commands

Everything goes through [mise](https://mise.jdx.dev), which pins the toolchain and routes Cargo
through mr-boxington.

| Command | Does |
| --- | --- |
| `mise install` | Installs the pinned tools |
| `mise run format` | Formats the code |
| `mise run lint` | rustfmt check and clippy with `-D warnings`, on the server code and the wasm bundle |
| `mise run test` | The test suite (nextest) |
| `mise run deny` | cargo-deny: advisories, licences, bans, sources |
| `mise run check` | What CI runs: lint, tests, cargo-deny |
| `mise run dev` | The server with live reload on http://127.0.0.1:3000 (password `jarvis`; config in `target/dev`) |
| `mise run build` | Release build: `target/release/jarvis` and the site in `target/site` |
| `mise run web-build` | Release build of the Dioxus interface into `target/dx/jarvis-web/release/web/public` |
| `mise run web-dev` | The Dioxus interface with hot reload (dx serve); `/ws`, `/sign-in` and `/sign-out` go to a jarvis server on 127.0.0.1:8484 |

Builds are heavy: run one cargo command at a time, through `mise exec -- cargo …` or a mise task.

`crates/server/tests/live.rs` chats with the real Claude adapter on the owner's subscription. It
is ignored and runs only by hand, with `JARVIS_LIVE_AGENT` set to the adapter's path:
`mise exec -- cargo nextest run -p jarvis --test live --run-ignored only --no-capture`.

## Rules

- Keep it as simple as possible: add a mechanism only when its absence hurts.
- Tool names match `^[a-z][a-z0-9_]{0,63}$`; tool descriptions are short imperative rules; a tool
  error always says how to fix it.
- Secrets never appear in the configuration file, logs or tool output: `{ file = "…" }` or
  `{ env = "…" }` only.
- Nothing read from an external source (web, email, document) is saved to memory automatically.
- Never rewrite calendar data the tools do not model: edit stored iCalendar in place.
- Tests never touch a real calendar, model or MCP server: use `wiremock`, in-process fakes and
  temporary directories.
- Rust/UI components are vendored by hand in `crates/web/src/components/ui/`, with
  `crates/web/src/components/LICENSE-rust-ui` (MIT) beside them: keep their classes and
  structure, and mark any change to them in place.
- Comments explain why, never restate the code. Docs are plain, concise English with no em dashes.
- Work and push directly on `main`. Commits follow Conventional Commits (`feat:`, `fix:`, `test:`,
  `build:`, `ci:`, `docs:`, `chore:`), imperative, in English. Do not mention AI tools and add no
  `Co-Authored-By` or "Generated with" lines.
- Working notes (`docs/superpowers/`) stay local and are never committed.

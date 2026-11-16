# AGENTS.md

jarvis (code name) is a personal agent hub written in Rust: it drives coding agents through their
own subscriptions over ACP, serves its memory, calendar and tools over MCP, and offers a web chat.

## Layout

| Crate | Role |
| --- | --- |
| `crates/hub` (`jarvis-hub`) | Tools registry and search, memory, CalDAV calendar and reminders, upstream MCP servers, the MCP server |
| `crates/protocol` (`jarvis-protocol`) | Types shared by the conversation log, the server and the frontend (`Event`, `Entry`); serde and chrono only |
| `crates/agent` (`jarvis-agent`) | The ACP client that drives the coding agent (Claude Code through its ACP adapter); an in-process fake agent behind the `testing` feature |
| `crates/app` (`jarvis-app`) | The Leptos web chat: sign-in and chat pages, the `ChatPanel` island and its WebSocket; `chat_model.rs` turns log entries into what the chat shows |
| `crates/frontend` (`jarvis-frontend`) | The browser bundle (wasm32): hydrates the app's island |
| `crates/server` | The `jarvis` binary and library: configuration check, HTTP routes, `/mcp` behind a bearer token, the chat pages and `/ws` |

`jarvis-app` builds with `ssr` for the server and with `hydrate` for the browser, never both:
server-side Cargo commands leave out `jarvis-frontend` (`$JARVIS_SERVER_PACKAGES`). UI classes
come from Rust/UI (`crates/app/src/components/LICENSE-rust-ui`); styles are Tailwind in
`style/main.css`. The server reads the built site from `JARVIS_SITE_ROOT` (default
`target/site`).

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

Builds are heavy: run one cargo command at a time, through `mise exec -- cargo …` or a mise task.

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
- Comments explain why, never restate the code. Docs are plain, concise English with no em dashes.
- Work and push directly on `main`. Commits follow Conventional Commits (`feat:`, `fix:`, `test:`,
  `build:`, `ci:`, `docs:`, `chore:`), imperative, in English. Do not mention AI tools and add no
  `Co-Authored-By` or "Generated with" lines.
- Working notes (`docs/superpowers/`) stay local and are never committed.

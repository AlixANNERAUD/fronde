# AGENTS.md

fronde (code name) is a personal agent hub written in Rust: it drives coding agents through their
own subscriptions over ACP, serves its memory, calendar and tools over MCP, and offers a web chat.

## Layout

| Crate | Role |
| --- | --- |
| `crates/hub` (`fronde-hub`) | Tools registry and search, memory, CalDAV calendar and reminders, upstream MCP servers, the MCP server |
| `crates/fs` (`fronde-fs`) | Private-file helpers shared by the hub and the agent: 0700 directories, 0600 files, durable atomic replacement (`atomic-write-file`: sync of file and directory) |
| `crates/protocol` (`fronde-protocol`) | Types shared by the conversation log, the server and the web interface (`Event`, `Entry`); serde and chrono only |
| `crates/agent` (`fronde-agent`) | The ACP client that drives the coding agent (Claude Code through its ACP adapter); an in-process fake agent behind the `testing` feature |
| `crates/client` (`fronde-client`) | The web UI's logic without any framework: `chat.rs` turns server messages into what the chat shows, `connection.rs` is the WebSocket reconnection state machine; depends on the protocol only |
| `crates/web` (`fronde-web`) | The Dioxus web interface, a single-page app built by dx: sign-in and chat pages; `socket.rs` runs the chat's WebSocket with `fronde-client`, behind `cfg(target_arch = "wasm32")` so the pages render natively in tests |
| `crates/wasm` (`fronde-wasm`) | Runs WebAssembly components with wasmtime: `host.rs` is what every world shares (engine, epoch ticker, WASI with no filesystem or sockets, per-call limits), `egress.rs` the HTTP gate (allowed origins, credentials added by the host), `dial.rs` its client (resolves the name, refuses local addresses unless allowed, connects to the checked address), `api.rs` which `fronde:component` version a component targets, `manifest.rs` the strict parser of a component's `fronde-component.toml`, `v0_1/` the bindings of version 0.1 (one module per served version), `tool.rs` the `tool` world, whose tools join the hub's registry |
| `crates/component-sdk` (`fronde-component`) | The guest SDK components depend on: the WIT it targets (a copy of `wit/v0.1`), `export_tool!`, settings, HTTP over WASI, fakes for native tests (`testing` feature). Stands alone: not in the root workspace, no dependency on another Fronde crate, publishable |
| `wit/` | The components' WIT, one frozen directory per released version of `fronde:component` (`v0.1`): shared `settings` interface, `tool` world |
| `components/` | Tool components, each a standalone Cargo project built for `wasm32-wasip2`, with its manifest `fronde-component.toml` beside `Cargo.toml` (`echo` is the host's test bench, `calendar` the CalDAV tools) |
| `crates/server` | The `fronde` binary and library: configuration check, healthcheck, HTTP routes, `/mcp` behind a bearer token, the sign-in forms (`/auth/sign-in`, `/auth/sign-out`), the interface's files and `/ws` |

Components are built by `mise run components`, which builds each project listed in
`$FRONDE_COMPONENTS` and copies the `.wasm` files to `components/dist` (mr-boxington prunes idle
target directories); `fronde-wasm`'s tests load them from there, and `mise run test` builds them
first. A component may not import `wasi:filesystem` or `wasi:sockets`. The SDK's `bindings` module
is the only place allowed `unsafe` code (the generated glue needs it); components forbid it.

The server has no Dioxus dependency: it serves the built interface as static files from
`FRONDE_SITE_ROOT` (default `target/dx/fronde-web/release/web/public`, written by
`mise run web-build`). In development, `mise run web-dev` serves the pages itself and proxies
`/ws` and `/auth` to `mise run dev` (`crates/web/Dioxus.toml`): keep every backend-only route
under one of those.

Server-side Cargo commands leave out `fronde-web` (`$FRONDE_SERVER_PACKAGES`), whose
dependencies are wasm-only. Lint and tests run it on its own, natively and for wasm32. It embeds
`crates/web/assets/tailwind.css` at compile time: the hidden `web-css` task generates it from
`crates/web/style/tailwind.css` with mise's Tailwind, and `lint`, `test`, `web-build` and
`web-dev` run it first.

## Commands

Everything goes through [mise](https://mise.jdx.dev), which pins the toolchain and routes Cargo
through mr-boxington.

| Command | Does |
| --- | --- |
| `mise install` | Installs the pinned tools |
| `mise run format` | Formats the code, components included |
| `mise run lint` | rustfmt check and clippy with `-D warnings`, on the server code, the web interface (native and wasm32) and the components (wasm32-wasip2) |
| `mise run components` | Release build of the components for wasm32-wasip2 into `components/dist` |
| `mise run test` | The test suite (nextest), after `components` |
| `mise run agent` | Installs the locked Claude ACP adapter (`agent/package-lock.json`) into `agent/node_modules`; Node.js comes from mise, which puts the adapter on `PATH` |
| `mise run deny` | cargo-deny: advisories, licences, bans, sources, for the root workspace and the components |
| `mise run check` | What CI runs: lint, tests, cargo-deny |
| `mise run dev` | The server on http://127.0.0.1:8484 for `web-dev` (password `fronde`; config and data in `target/dev`) |
| `mise run web-dev` | The Dioxus interface with hot reload on http://127.0.0.1:8080 (dx serve); `/ws` and `/auth` go to `mise run dev` |
| `mise run web-build` | Release build of the Dioxus interface into `target/dx/fronde-web/release/web/public` |
| `mise run build` | Release build: `web-build`, then `target/release/fronde` |
| `mise run docs` | Builds the user guide (mdBook, `docs/`) into `docs/book` |

Builds are heavy: run one cargo command at a time, through `mise exec -- cargo …` or a mise task.

`crates/server/tests/live.rs` chats with the real Claude adapter on the owner's subscription. It
is ignored and runs only by hand, with `FRONDE_LIVE_AGENT` set to the adapter's path (`agent/node_modules/.bin/claude-agent-acp`, after `mise run agent`):
`mise exec -- cargo nextest run -p fronde --test live --run-ignored only --no-capture`.

## Dioxus 0.7 usage

Pinned in `Cargo.toml`; `crates/web` is the app, `crates/client` its framework-free model.

- `rsx!` with `#[component]` functions; lists use `for` with a stable `key` (generation and seq,
  never the row version, which would remount the node on each chunk).
- Shared state is `Signal`s; the WebSocket lives in one `use_coroutine` (`socket.rs`) that owns the
  socket and the retry timer. Connection logic stays in `fronde-client`, native and tested.
- Components and pages take plain props (no context needed to render) so `dioxus-ssr` and
  VirtualDom tests drive them natively.
- Browser-only code (sockets, timers, `web_sys`) sits behind `cfg(target_arch = "wasm32")`, with a
  native stub for tests.
- Vendored rust-ui components keep upstream classes; changes carry a `// fronde:` marker.
- Tailwind is built by `mise run web-css`, not by `dx`; `dx` only bundles the wasm.
- Dev: `mise run web-dev` for the UI (proxies `/ws` and `/auth` to the backend), plus `mise run dev` for the backend
  in another terminal. Release: `mise run web-build`.

## Rules

- Keep it as simple as possible: add a mechanism only when its absence hurts.
- Components are independent projects that will move to their own repositories: each has its own
  `Cargo.toml`, `Cargo.lock` and release profile, and depends only on `fronde-component` and
  external crates, never on another Fronde crate. The SDK depends on no Fronde crate either.
- A released WIT directory (`wit/v0.1`, …) is frozen: never edit it, not even to add a function or
  an enum case. A change is a new directory with its own host module (`crates/wasm/src/v0_2/`),
  and the host keeps serving the earlier versions.
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
  structure, and mark any change to them in place with a `// fronde:` comment. Icons are Lucide's
  (ISC, `crates/web/src/components/LICENSE-lucide`), drawn inline in
  `crates/web/src/components/icons.rs`; add one there from Lucide's paths rather than adding an
  icon crate.
- Comments explain why, never restate the code. Docs are plain, concise English with no em dashes.
- The user guide in `docs/src` follows Diátaxis (tutorials, how-to guides, reference, explanation).
  A change to settings, endpoints, tools or behaviour updates the matching pages in the same commit.
- Work and push directly on `main`. Commits follow Conventional Commits (`feat:`, `fix:`, `test:`,
  `build:`, `ci:`, `docs:`, `chore:`), imperative, in English. Do not mention AI tools and add no
  `Co-Authored-By` or "Generated with" lines.
- Working notes (`docs/superpowers/`) stay local and are never committed.

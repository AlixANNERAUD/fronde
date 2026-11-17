# AGENTS.md

fronde (code name) is a personal agent hub written in Rust: it drives coding agents through their
own subscriptions over ACP, serves its memory and tools (a calendar component among them) over MCP,
and offers a web chat.

## Layout

| Crate | Role |
| --- | --- |
| `crates/hub` (`fronde-hub`) | Tools registry (`tools.rs`; a call carries a `CallContext`: who calls, the assistant with its running turn or an outside MCP client by its `clientInfo` name, for which user and stream, and the token rmcp cancels when the caller gives the call up (rmcp never stops the handler, so a tool that waits watches it), passed on by `use_tool`; `mcp.rs` builds it; `user.rs` the `user_*` tools, `user_ask` today, served only with the chat's `Requests`, the prefix `user_` reserved) and search (`search.rs`: FTS5 in an in-memory SQLite, rebuilt at the first search after the tools changed), memory (its full-text index is `fronde.db`'s `memory_fts`), upstream MCP servers, the MCP server; `source.rs` is where tools come from: the `ToolSource` trait and the one supervisor that starts every source (the hub's own tools, upstream servers, components), registers its tools and restarts it with backoff (a component that fails to start is given up, unless its error is a `Waiting`, such as a stored secret not set yet, which is retried with one growing backoff until it starts or fails otherwise, and is then given up); `origin.rs` the gate's shared rules: `Origin` (scheme, host normalised by the URL standard, port), parsed once from the configuration and once per request, which addresses are local or never allowed, which origins may carry credentials |
| `crates/fs` (`fronde-fs`) | Private-file helpers shared by the hub and the database (`fronde-db`): 0700 directories, 0600 files, durable atomic replacement (`atomic-write-file`: sync of file and directory) |
| `crates/db` (`fronde-db`) | The database, one SQLite file `<data>/fronde.db` through SeaORM: `open` (created 0600, WAL, `synchronous = FULL`, pending migrations applied in one transaction), `open_read_only` (`fronde check`: no migration, no write), `begin_write` (`BEGIN IMMEDIATE`, for every transaction that writes), `kv` (values by scope and key, optionally expiring), `approvals` (component approvals by source and both hashes, read by `fronde_wasm::Checks::load`), `secrets` (values sealed with XChaCha20-Poly1305, the associated data binding format version, scope, name and canonical origin; the key check in `kv` `secrets`/`key_check`, written with the first secret, sealed under its own domain label, an older check sealed again by the next write), `master_key` (`MasterKey`, format `fronde-key-v1:<base64url>`, kept by a `Backend`: `systemd` (`systemd.rs`, `$CREDENTIALS_DIRECTORY/fronde-master-key`, read only), `keychain` (`keychain.rs`, keyring-core with one store crate per platform, Secret Service over zbus with RustCrypto on Linux, entry `fronde` / `master-key@<hash of the data dir>`, each call on a blocking thread under a 2 s timeout; `FakeKeychain` on keyring-core's mock store behind the `testing` feature) or `file` (a 0600 plain file never followed through a link, by default `master.key` beside the configuration (`[secrets] master_key`, `Config::master_key_file`), never in the data directory so that its backups do not hold it; `migrate` moves a key an earlier version left in `<data>/master.key`, `locate` finds it without moving for `fronde check`); `Backends` (`backends.rs`, built by `Config::master_key_backends` from `[secrets] backend`) picks one: a named one alone, or `auto`: the backend recorded in `kv` `secrets`/`backend` (written by `secrets::set` with the key check, and by `upgrade_key_check` at start), else for the first key the systemd credential, then the keychain if it answers, then the file; never falls through once recorded or while anything is sealed; created only while nothing is sealed, never from the environment) |
| `crates/migration` (`fronde-migration`) | The schema's migrations (sea-orm-migration), one per change, never edited once pushed; `examples/migrate.rs` applies them to a SQLite URL for `mise run entities` |
| `crates/entity` (`fronde-entity`) | The SeaORM entities, generated from the migrated schema by `mise run entities`, never edited by hand |
| `crates/protocol` (`fronde-protocol`) | Types shared by the conversation log, the server and the web interface (`Event`, `Entry`); `request.rs` what a request's card shows (`Card`, `Field`), what the page answers (`Response`), how it ended (`Outcome`), and `check`, the rules an answer follows, run by the page before sending and by the server; serde and chrono only |
| `crates/requests` (`fronde-requests`) | Requests to the user: the `requests` table, written before its card is shown by a `Journal` (the assistant, over its conversation log), answered once through `Requests::answer` (identity, then the card's rules, then a compare-and-set on `pending`, then the journal, then the waiting asker), cancelled, or expired by a timer of its own (its `Waiter` also gives up at the expiry plus a grace, should the database fail to close it); a retry by `client_request_id` joins only a request asking the very same thing that is still current (`retries`: the assistant's same turn, an outside client's pending request or its timeout), else a new request, or `Reused` while another pending request holds the id (unique among pending rows only); `open` expires overdue rows, announces rows a crash left unshown and cancels a row it cannot read |
| `crates/agent` (`fronde-agent`) | The ACP client that drives the coding agent (Claude Code through its ACP adapter); the assistant is the requests' `Journal` and knows its running turn, whose pending requests it cancels on Stop and when the turn ends; `log.rs` the conversation, the database stream `conversation` (`streams`, `stream_entries`) mirrored in memory, each entry committed before it is broadcast; the agent's session id in `kv` (`agent`/`session`); `legacy.rs` the one-time import of the JSONL log and session file from before the database; an in-process fake agent behind the `testing` feature |
| `crates/client` (`fronde-client`) | The web UI's logic without any framework: `chat.rs` turns server messages into what the chat shows (a request is open until its `request_closed`, whatever the turns do), `connection.rs` is the WebSocket reconnection state machine; depends on the protocol only |
| `crates/web` (`fronde-web`) | The Dioxus web interface, a single-page app built by dx: sign-in and chat pages; `components/request_card.rs` a request's card (one answer as a radio group with arrow keys, several as boxes, "Autre…", a note, previews), checked with `fronde_protocol::request::check` before it is sent; `socket.rs` runs the chat's WebSocket with `fronde-client`, behind `cfg(target_arch = "wasm32")` so the pages render natively in tests |
| `crates/wasm` (`fronde-wasm`) | Runs WebAssembly components with wasmtime: `host.rs` is what every world shares (engine, epoch ticker, CPU metering and call timeout) and the tool world's linker (WASI with no filesystem or sockets) and per-sandbox limits, `egress.rs` the HTTP gate (allowed origins, credentials added by the host), `dial.rs` its client (resolves the host the gate matched, refuses local addresses, private ranges and metadata included, unless the origin is allowed as local, connects to the checked address), `api.rs` which `fronde:component` version a component targets, `manifest.rs` the strict parser of a component's `fronde-component.toml`, `v0_1/` the bindings of version 0.1 (one module per served version), `tool.rs` the `tool` world, whose tools join the hub's registry (it loads only a `Prepared`, which only `prepare` makes; `load_unapproved` exists for tests, behind the `testing` feature that only dev-dependencies enable), `source.rs` a `[[sources]]` component entry checked against its manifest, its approval (decided by `approve` alone: the hashes in the entry, which always win, or else `Approvals`: a component shipped with the build, which `build.rs` hashes from `components/dist` into the binary, or an approval stored in `fronde.db` for that source and both hashes, loaded before `prepare` by `Checks::load`; a refusal is a typed `NotApproved`), the other sources' tool names and the sandbox's grants (`prepare`), its `{ stored = "…" }` credentials (`StoredSecrets`: the rows still sealed and the master key, private fields filled only by `StoredSecrets::load`; `prepare` opens only those its credentials name, each only for the origin it was stored for; `ComponentSource` reads them again at each start, and the `Checks` kept for the run, from `Checks::load`, hold no key), and run as a `ToolSource`, `guest_log.rs` a component's standard error as warnings in the log, `python.rs` the script world: CPython for WASI preview 1 (`python/`, from `mise run python`, pinned by `PYTHON_WASM_SHA256`) in its own core linker with only WASI p1 and the stdlib read-only at `/lib`, a fresh instance per run, at most `MAX_RUNS` (2) at once, silent stubs for the p1 socket functions, the blocking-file setting only on the `/lib` preopen (with it on the context, `poll_oneoff` would sleep on the runtime's thread), `python/runner.py` the fixed runner that reads the JSON envelope and writes the reply, `PythonSource` the `[python]` source serving `code_run_python` (the `code_` prefix is reserved for it) |
| `crates/component-sdk` (`fronde-component`) | The guest SDK components depend on: the WIT it targets (a copy of `wit/v0.1`), `export_tool!`, settings, HTTP over WASI, fakes for native tests (`testing` feature). Stands alone: not in the root workspace, no dependency on another Fronde crate, publishable |
| `wit/` | The components' WIT, one frozen directory per released version of `fronde:component` (`v0.1`): shared `settings` interface, `tool` world |
| `components/` | Tool components, each a standalone Cargo project built for `wasm32-wasip2`, with its manifest `fronde-component.toml` beside `Cargo.toml` (`echo` is the host's test bench, `calendar` the CalDAV tools, whose tool contract its own tests pin in `tests/snapshots`) |
| `crates/server` | The `fronde` binary and library: configuration check (`check.rs`, which prints the components' hashes and opens the database read-only), `fronde secret` (`secret.rs`: set from standard input only, list, remove), `hardening.rs` (on Linux, `serve` makes itself non-dumpable with no core dumps unless `FRONDE_ALLOW_CORE_DUMPS=1`), healthcheck, the component sources it hands the hub, HTTP routes, `/mcp` behind a bearer token (`server.token` for outside clients, or the assistant's own, made at each start and kept in memory; the gate puts the `McpCaller` it found in the request's extensions, which rmcp hands to the hub's `CallContext`), the sign-in forms (`/auth/sign-in`, `/auth/sign-out`), web sessions in `fronde.db` (`sessions.rs`: `tower-sessions-sqlx-store` on the database's pool, its own `tower_sessions` table, not an entity, each row keyed by the SHA-256 of the session id), the purge of expired sessions and `kv` rows at start and every hour (restarted after a panic), the interface's files and `/ws` |

Components are built by `mise run components`, which builds each project listed in
`$FRONDE_COMPONENTS` and copies the `.wasm` files to `components/dist`, each with its manifest
beside it as `<name>.toml` (mr-boxington prunes idle target directories); the container image
ships them in `/app/components`; `fronde-wasm`'s tests load them from there, and `mise run test` builds them
first. Likewise `mise run python` fetches the Python runtime into `python/`, which the image ships in
`/app/python` and the tests load (`FRONDE_PYTHON_DIR` overrides it). `mise run build` builds them before the binary, which approves exactly those files. A component may not import `wasi:filesystem` or `wasi:sockets`. The SDK's `bindings` module
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
| `mise run python` | Fetches the pinned CPython WASI build (and tzdata) into `python/`, checks its hashes and byte-compiles the stdlib |
| `mise run test` | The test suite (nextest), after `components` and `python` |
| `mise run agent` | Installs the locked Claude ACP adapter (`agent/package-lock.json`) into `agent/node_modules`; Node.js comes from mise, which puts the adapter on `PATH` |
| `mise run deny` | cargo-deny: advisories, licences, bans, sources, for the root workspace and the components |
| `mise run check` | What CI runs: lint, tests, cargo-deny |
| `mise run dev` | The server on http://127.0.0.1:8484 for `web-dev` (password `fronde`; config and data in `target/dev`) |
| `mise run web-dev` | The Dioxus interface with hot reload on http://127.0.0.1:8080 (dx serve); `/ws` and `/auth` go to `mise run dev` |
| `mise run web-build` | Release build of the Dioxus interface into `target/dx/fronde-web/release/web/public` |
| `mise run build` | Release build: `web-build`, `components` and `python`, then `target/release/fronde` |
| `mise run docs` | Builds the user guide (mdBook, `docs/`) into `docs/book` |
| `mise run entities` | Regenerates `crates/entity` from the migrations with `sea-orm-cli` |

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
- Integrations with outside services (a REST API, CalDAV, any standard) are components, not
  hub code; an upstream MCP server is used as it is when a good one exists. Native Rust is for
  the core: memory, scheduling, the hub itself. Every source of tools is a `ToolSource` under the
  supervisor, configured as a `[[sources]]` entry.
- Components are independent projects that will move to their own repositories: each has its own
  `Cargo.toml`, `Cargo.lock` and release profile, and depends only on `fronde-component` and
  external crates, never on another Fronde crate. The SDK depends on no Fronde crate either.
- A released WIT directory (`wit/v0.1`, …) is frozen: never edit it, not even to add a function or
  an enum case. A change is a new directory with its own host module (`crates/wasm/src/v0_2/`),
  and the host keeps serving the earlier versions.
- Tool names match `^[a-z][a-z0-9_]{0,63}$`; tool descriptions are short imperative rules; a tool
  error always says how to fix it. A component's tools start with a prefix of its manifest's
  `tool_prefixes`, which no other source uses.
- Durable structured state lives in the one database, `fronde.db`, through SeaORM; raw SQL only
  for FTS5. A schema change is a new migration, never an edit of a pushed one, followed by
  `mise run entities`: entities are generated, never edited by hand.
  The bundled SQLite is held at `libsqlite3-sys` 0.37 by sqlx-sqlite, so `deny.toml` ignores
  RUSTSEC-2026-0459: drop that ignore as soon as a sqlx release accepts 0.39.
- Secrets never appear in the configuration file, logs or tool output: `{ file = "…" }` or
  `{ env = "…" }` only, or `{ stored = "…" }` (sealed in `fronde.db`) for a component's
  credential.
- Resolved secrets are `secrecy` types. `expose_secret` is called only at the sites listed, with
  their count per file, in `crates/server/tests/secrets.rs` (`expose_secret_call_sites`); a new use
  is added to that list on purpose, and reviewed.
- Nothing read from an external source (web, email, document) is saved to memory automatically.
- Never lose calendar data the tools do not model: edits keep every component, property and
  parameter they do not change; only formatting (folding, parameter order, quoting, a text value's
  transfer encoding) may change. The calendar uses the owner's calcard fork, which keeps values
  as read: fix a loss there, not with a workaround in the component.
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

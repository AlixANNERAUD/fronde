# AGENTS.md

jarvis (code name) is a personal agent hub written in Rust: it drives coding agents through their
own subscriptions over ACP, serves its memory, calendar and tools over MCP, and offers a web chat.

## Layout

| Crate | Role |
| --- | --- |
| `crates/hub` (`jarvis-hub`) | Tools registry and search, memory, CalDAV calendar and reminders, upstream MCP servers, the MCP server |
| `crates/server` | The `jarvis` binary and library: configuration check, HTTP routes, `/mcp` behind a bearer token |

## Commands

Everything goes through [mise](https://mise.jdx.dev), which pins the toolchain and routes Cargo
through mr-boxington.

| Command | Does |
| --- | --- |
| `mise install` | Installs the pinned tools |
| `mise run format` | Formats the code |
| `mise run lint` | rustfmt check and clippy with `-D warnings` |
| `mise run test` | The test suite (nextest) |
| `mise run deny` | cargo-deny: advisories, licences, bans, sources |
| `mise run check` | What CI runs: lint, tests, cargo-deny |

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

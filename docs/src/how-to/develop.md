# Develop and contribute

Everything goes through [mise](https://mise.jdx.dev), which pins the toolchain and routes Cargo
through mr-boxington. `AGENTS.md` at the root of the repository has the layout of the crates and
the rules the code follows.

## Set up

```sh
mise install
mise run agent   # the Claude adapter, to chat with a real agent
```

## Tasks

| Command | Does |
| --- | --- |
| `mise run format` | Formats the code, the component SDK and components included |
| `mise run lint` | rustfmt check and clippy with `-D warnings`, on the server, the web interface (native and wasm32), the component SDK and each component (wasm32-wasip2) |
| `mise run components` | Builds each Wasm component project into `components/dist` |
| `mise run test` | The test suite (nextest), after `components` |
| `mise run deny` | cargo-deny: advisories, licences, bans, sources, for the server, the component SDK and each component |
| `mise run check` | What CI runs: lint, tests, cargo-deny |
| `mise run dev` | The server on <http://127.0.0.1:8484>, password `fronde` |
| `mise run web-dev` | The web interface with hot reload on <http://127.0.0.1:8080> |
| `mise run web-build` | Release build of the web interface |
| `mise run build` | Release build: `web-build`, then `target/release/fronde` |
| `mise run docs` | Builds this guide into `docs/book` |

Builds are heavy: run one Cargo command at a time, through a task or `mise exec -- cargo …`.

## Work on the web interface

The interface is a [Dioxus](https://dioxuslabs.com) app in `crates/web`. Run the backend and the
interface side by side, in two terminals:

```sh
mise run dev       # the server on 127.0.0.1:8484, password `fronde`
mise run web-dev   # the interface with hot reload on 127.0.0.1:8080
```

Open <http://127.0.0.1:8080>. dx serves the pages and passes the chat's WebSocket (`/ws`) and the
sign-in forms (`/auth/…`) to the server, so any route only the backend answers must stay under one
of those. `mise run dev` writes its configuration, a random token and its data under `target/dev`,
and needs the Claude adapter on the `PATH` to chat.

## Tests

Tests never touch a real calendar, model or MCP server: they use `wiremock`, in-process fakes and
temporary directories. One test does talk to the real Claude adapter on your subscription. It is
ignored by default and runs only by hand:

```sh
mise run agent
FRONDE_LIVE_AGENT=$PWD/agent/node_modules/.bin/claude-agent-acp \
  mise exec -- cargo nextest run -p fronde --test live --run-ignored only --no-capture
```

The path must be absolute: tests run from their crate's directory. `FRONDE_LIVE_CLAUDE` optionally
names the Claude Code to run instead of the adapter's own.

## Documentation

This guide lives in `docs/src` and follows Diátaxis. A change to settings, endpoints, tools or
behaviour updates the matching pages in the same commit. Build it with `mise run docs` and open
`docs/book/index.html`.

## Commits

Work on `main`. Commits follow Conventional Commits (`feat:`, `fix:`, `docs:`…), in English and
the imperative mood.

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
| `mise run python` | Fetches the pinned CPython WASI build into `python/` (see below) |
| `mise run test` | The test suite (nextest), after `components` and `python` |
| `mise run deny` | cargo-deny: advisories, licences, bans, sources, for the server, the component SDK and each component |
| `mise run check` | What CI runs: lint, tests, cargo-deny |
| `mise run dev` | The server on <http://127.0.0.1:8484>, password `fronde` |
| `mise run web-dev` | The web interface with hot reload on <http://127.0.0.1:8080> |
| `mise run web-build` | Release build of the web interface |
| `mise run build` | Release build: `web-build`, `components` and `python`, then `target/release/fronde`, which approves the components of `components/dist` it was built with |
| `mise run docs` | Builds this guide into `docs/book` |
| `mise run entities` | Regenerates the database entities (`crates/entity`) from the migrations |

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

## The Python runtime

`code_run_python` runs CPython built for WASI (`wasm32-wasip1`). python.org publishes no WASI
build, so Fronde uses the one CPython's WASI maintainer builds from each release tag with
CPython's own `Platforms/WASI` script, at
[brettcannon/cpython-wasi-build](https://github.com/brettcannon/cpython-wasi-build).
`mise run python` downloads it over https into `python/` (git-ignored), with the
[tzdata](https://pypi.org/project/tzdata/) wheel for time zones, which the build lacks. It checks
both archives and `python.wasm` against hashes pinned in `mise.toml`, then byte-compiles the
standard library with the wasmtime CLI, so that runs do not compile the modules they import. It
does nothing when the same recipe, archives and `python.wasm` are already installed (a stamp,
`python/.installed`, records them). A lock, `python.lock`, keeps two runs from replacing
`python/` at once. The tests of `fronde-wasm` load the
runtime from `python/`, or from `FRONDE_PYTHON_DIR`.

To upgrade Python or tzdata:

1. Read the CPython changelog for WASI changes, and check that the new build still targets the
   WASI version the host links (`wasm32-wasip1` up to 3.15; 3.16 moves to WASI 0.3 components,
   which needs a component linker).
2. In the `python` task of `mise.toml`, change the URLs and the archives' sha256 (the GitHub
   release lists them, as does PyPI's JSON for the wheel), and the version in the path
   `lib/python3.15` if it changes.
3. Run `mise run python`. It stops at the check of `python.wasm`: put the new hash in both
   `mise.toml` and `PYTHON_WASM_SHA256` in `crates/wasm/src/python.rs`, which Fronde checks
   before it compiles the module.
4. Run `mise run test`.

## Change the database schema

The database is SQLite through [SeaORM](https://www.sea-ql.org/SeaORM/). A schema change is a new
migration in `crates/migration`, named `mYYYYMMDD_NNNNNN_purpose` and appended to
`Migrator::migrations`; a migration that was pushed is never edited, since databases that applied
it would never see the change. Then run `mise run entities`, which applies the migrations to a
throwaway database under `target/` and regenerates `crates/entity` with `sea-orm-cli`. The
entities are generated, never edited by hand. Fronde applies pending migrations at each start,
all in one transaction. sea-orm-migration runs a SQLite migration outside any transaction unless
it asks for one, so a migration of several statements also returns `Some(true)` from
`use_transaction`: it then applies whole or not at all wherever it runs.

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

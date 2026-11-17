# Enable Python

`code_run_python` lets the agent run short Python for exact arithmetic, dates and aggregates over
data, instead of working them out in its head. It runs in a sandbox with the standard library
only: no files, no network, no packages. See [Tools](../reference/tools.md#python) for what it
does and [Security model](../explanation/security.md#python-sandbox) for the sandbox.

## In Docker

The image ships the runtime in `/app/python`. Add to `/config/fronde.toml`:

```toml
[python]
dir = "/app/python"
```

Restart the container.

## From a checkout

Fetch the runtime, CPython for WASI and the time zone database, into `python/`:

```sh
mise run python
```

The task checks each download against a pinned SHA-256, and does nothing when the runtime is
already there. Then point the configuration at it, with an absolute path or one relative to the
configuration file:

```toml
[python]
dir = "/path/to/fronde/python"
```

`mise run dev` does both for you.

## Check it

```sh
fronde check --config fronde.toml
```

prints `python: approved (CPython 3.15.2)`. At start, the log says
`python: approved (CPython 3.15.2), compiled in … s`, and the hub's instructions to the agent
gain a line asking it to use `code_run_python` rather than mental arithmetic.

Fronde runs only the CPython build it pins. If `fronde check` says `python.wasm` is not the
approved build, run `mise run python` again (it replaces the directory), or copy `/app/python`
from the image of the same version.

## Turn it off

Remove the `[python]` section and restart: the tool is gone.

# Component interface (WIT)

Fronde can run WebAssembly components (WASI 0.2) that bring tools to the hub. Their interface is
the WIT package `fronde:component`, one frozen directory per released version: `wit/v0.1`
(`fronde:component@0.1.0`) is the only one so far.

A component is enabled by a `[[sources]]` entry of kind `component` (see
[Configuration](configuration.md#kind--component)). Fronde ships one, the calendar.

## Versions

A released version never changes. A new one gets its own directory (`wit/v0.2`) and the host
keeps serving the earlier ones, so a component built for 0.1 keeps working. When it loads a
component, the host reads the versions of the `fronde:component` interfaces the component imports
and exports, and refuses one built for a version it does not serve, naming the versions it does:

| Fronde | Serves |
| --- | --- |
| 0.1 | `fronde:component@0.1.x` |

A released version's directory under `wit/` is frozen and never edited, so within a version the
interfaces stay the same. A fix is a new version, which Fronde then serves beside the old one.

## The `tool` world

```wit
world tool {
  import settings;
  export tools;
}
```

A component in this world exports `tools` and may import `settings`, WASI and outgoing HTTP
(`wasi:http`). The hub refuses, when it loads the component, one that imports `wasi:filesystem`
or `wasi:sockets`.

## Interface `settings`

```wit
get: func(key: string) -> option<string>;
```

Non-secret settings the host chooses for the component: URLs, user names, a time zone. Credentials
never go through here.

## Interface `tools`

```wit
record tool {
  name: string,
  description: string,
  input-schema: string,   // JSON Schema of the arguments, as JSON text
  deferred: bool,         // hidden from the tool list, found through search_tools
}

record tool-output {
  value: string,          // JSON text
  next: option<string>,
  is-error: bool,
}

list-tools: func() -> list<tool>;
call-tool: func(name: string, arguments: string) -> tool-output;
```

The host calls `list-tools` once, when the component loads, and adds the tools to the hub's
registry, with the component's name as their source. Each tool call then runs `call-tool` with the arguments as
JSON text. A `value` that is not valid JSON is passed on as a string. Tool names follow the hub's
rule, `^[a-z][a-z0-9_]{0,63}$`.

## What a component may do

Every call runs in a fresh instance: nothing is kept from one call to the next.

| Resource | Rule |
| --- | --- |
| Filesystem, sockets, environment, arguments | None. |
| Outgoing HTTP | Only to the origins (`scheme://host:port`) the host allows for this component. Anything else is refused. Redirects are not followed: the new location is a new request, checked again. The host resolves the name itself and connects to the address it checked. Local addresses are refused unless that origin is allowed as local: loopback, unspecified, private (10/8, 172.16/12, 192.168/16, fc00::/7), shared (100.64/10), link-local and cloud metadata addresses (such as 169.254.169.254 and 168.63.129.16), including IPv4 carried in IPv6. |
| Credentials | Added by the host to requests for their origin, as `Authorization` (Basic or Bearer). The component cannot set `Authorization` or `Cookie` itself, and never sees the secret. A credential needs an https origin, or a loopback one. |
| Wall-clock time | 60 seconds per call, waiting on HTTP included. |
| Running time | 10 seconds per call (100 ticks of 100 ms during which guest code runs). Time spent waiting is free. |
| Memory | 64 MiB per linear memory. |
| Each HTTP request | 10 seconds to connect, 30 seconds from sending it to the end of the response body. |

A component that fails, traps or runs out of time becomes a tool error for that call, never a hub
failure.

## Building a component

A component is a Rust library crate (`crate-type = ["cdylib"]`) built for `wasm32-wasip2`, in a
Cargo project of its own: its own `Cargo.toml`, lock file and release profile. It depends on the
guest SDK, `fronde-component` (`crates/component-sdk`), which embeds the WIT it targets and hides
the generated bindings:

```rust
use fronde_component::{Tool, ToolOutput, Tools, export_tool, settings};
use serde_json::json;

struct Hello;

impl Tools for Hello {
    fn list_tools() -> Vec<Tool> {
        vec![Tool::new("hello_greet", "Greet the user.", json!({"type": "object"}))]
    }

    fn call_tool(name: &str, _arguments: &str) -> ToolOutput {
        match name {
            "hello_greet" => ToolOutput::ok(settings::get("greeting").unwrap_or("hello".into())),
            other => ToolOutput::error(format!("unknown tool {other}")),
        }
    }
}

export_tool!(Hello);
```

The SDK also brings `http`, a transport over WASI HTTP (`WasiHttp`, through the host's gate), and,
with its `testing` feature, an in-memory HTTP server, settings and a runner to test a component
natively. The crate needs no `unsafe` of its own: `#![forbid(unsafe_code)]` holds.

Beside its `Cargo.toml`, a component has a [manifest](component-manifest.md),
`fronde-component.toml`, which names the interface version it targets, the prefixes of its tool
names (`tool_prefixes = ["hello_"]` here) and what it asks for.

In this repository, `components/echo` (the host's test bench) and `components/calendar` are such
projects. `mise run components` builds each one and copies the `.wasm` files to `components/dist`.
See [Develop and contribute](../how-to/develop.md).

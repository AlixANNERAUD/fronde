# Component interface (WIT)

Fronde can run WebAssembly components (WASI 0.2) that bring tools to the hub. Their interface is
`wit/component.wit`, package `fronde:component@0.1.0`.

> **Status.** The component runtime (`crates/wasm`) and this interface exist and are tested with
> the `echo` test component. Loading components from the configuration is **planned**: no
> configuration section declares a component yet, so a running Fronde serves no component tools
> today.

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
| Outgoing HTTP | Only to the origins (`scheme://host:port`) the host allows for this component. Anything else is refused. Redirects are not followed: the new location is a new request, checked again. |
| Credentials | Added by the host to requests for their origin, as `Authorization` (Basic or Bearer). The component cannot set `Authorization` or `Cookie` itself, and never sees the secret. A credential needs an https origin, or a loopback one. |
| Wall-clock time | 60 seconds per call, waiting on HTTP included. |
| Running time | 10 seconds per call (100 ticks of 100 ms during which guest code runs). Time spent waiting is free. |
| Memory | 64 MiB per linear memory. |
| Each HTTP request | 10 seconds to connect, 30 seconds from sending it to the end of the response body. |

A component that fails, traps or runs out of time becomes a tool error for that call, never a hub
failure.

## Building a component

Components live in `components/`, a Cargo workspace of its own built for `wasm32-wasip2` with
`mise run components`, which copies the `.wasm` files to `components/dist`. See
[Develop and contribute](../how-to/develop.md).

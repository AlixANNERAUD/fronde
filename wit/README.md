# WIT

Each directory is one released version of the `fronde:component` package: `v0.1` is
`fronde:component@0.1.0`. **A released directory is frozen: never edit it**, not even to add a
function or an enum case. A change goes into a new directory (`v0.2`) with its own host bindings
(`crates/wasm/src/v0_2/`), and the host keeps serving every earlier version.
`crates/component-sdk/wit` is a copy of the version the SDK targets; `mise run lint` checks that
they match.

//! WebAssembly components (WASI 0.2, `wit/component.wit`) that extend the hub. Only the `tool`
//! world exists so far: components that bring tools.
//!
//! A component gets no filesystem, sockets, environment or arguments, and may only reach the
//! origins its spec allows; the host adds credentials to those requests itself. Every call runs in
//! a fresh store with a memory cap, a CPU budget and a wall-clock timeout, so a failing component
//! becomes a tool error, never a hub failure.

mod egress;
mod host;
mod tool;

pub use egress::{Credential, Origin};
pub use host::{ComponentHost, Limits};
use std::collections::BTreeMap;
use std::path::PathBuf;
pub use tool::LoadedComponent;

/// A component to load, and what it may reach.
#[derive(Debug, Clone)]
pub struct ComponentSpec {
    /// Source of its tools in the registry.
    pub name: String,
    pub path: PathBuf,
    pub allow: Vec<Origin>,
    /// Added by the host to requests for that origin, which must also be allowed.
    pub credentials: Vec<(Origin, Credential)>,
    /// What the guest reads through `settings.get`: never secrets.
    pub settings: BTreeMap<String, String>,
}

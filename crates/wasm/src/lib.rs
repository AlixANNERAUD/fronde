//! WebAssembly components (WASI 0.2) that extend the hub, built against a version of the
//! `fronde:component` WIT package (`wit/v0.1`, …), which the host checks when it loads them. Only
//! the `tool` world exists so far: components that bring tools.
//!
//! A component gets no filesystem, sockets, environment or arguments, and may only reach the
//! origins its spec allows; the host adds credentials to those requests itself. Every call runs in
//! a fresh store with a memory cap, a CPU budget and a wall-clock timeout, so a failing component
//! becomes a tool error, never a hub failure. Its standard error goes to the hub's log.
//!
//! A `[[sources]]` entry of kind `component` becomes a [`ComponentSource`]: [`prepare`] checks it
//! against the component's manifest and approval before the component loads.

mod api;
mod dial;
mod egress;
mod guest_log;
mod host;
mod manifest;
mod source;
mod tool;
mod v0_1;

pub use api::Api;
pub use egress::{Allowed, Credential};
pub use fronde_hub::origin::Origin;
pub use host::{ComponentHost, Limits};
pub use manifest::{CredentialRequest, Injection, LimitRequest, Manifest, NetworkRule, World};
pub use source::{Approval, Checks, ComponentSource, Hashes, Prepared, Shipped, prepare, shipped};
use std::collections::BTreeMap;
use std::path::PathBuf;
pub use tool::LoadedComponent;

/// A component to load, and what it may reach. It loads only as a [`Prepared`], which carries
/// its approval.
#[derive(Debug, Clone)]
pub struct ComponentSpec {
    /// Source of its tools in the registry.
    pub name: String,
    pub path: PathBuf,
    pub allow: Vec<Allowed>,
    /// Added by the host to requests for that origin, which must also be allowed.
    pub credentials: Vec<(Origin, Credential)>,
    /// What the guest reads through `settings.get`: never secrets.
    pub settings: BTreeMap<String, String>,
    /// Every tool the component exports must start with one of these.
    pub tool_prefixes: Vec<String>,
}

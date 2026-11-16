//! WebAssembly components (WASI 0.2) that extend the hub, built against a version of the
//! `fronde:component` WIT package (`wit/v0.1`, …), which the host checks when it loads them. Only
//! the `tool` world exists so far: components that bring tools.
//!
//! A component gets no filesystem, sockets, environment or arguments, and may only reach the
//! origins its spec allows; the host adds credentials to those requests itself. Every call runs in
//! a fresh store with a memory cap, a CPU budget and a wall-clock timeout, so a failing component
//! becomes a tool error, never a hub failure.

mod api;
mod dial;
mod egress;
mod host;
mod manifest;
mod tool;
mod v0_1;

pub use egress::{Credential, Origin};
pub use host::{ComponentHost, Limits};
pub use manifest::{CredentialRequest, Injection, LimitRequest, Manifest, NetworkRule, World};
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
    /// Whether the allowed origins may resolve to loopback, link-local or cloud metadata
    /// addresses, which are refused otherwise: only for a service on the host itself.
    pub allow_local: bool,
    /// Added by the host to requests for that origin, which must also be allowed.
    pub credentials: Vec<(Origin, Credential)>,
    /// What the guest reads through `settings.get`: never secrets.
    pub settings: BTreeMap<String, String>,
}

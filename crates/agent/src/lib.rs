//! Agent: drives a coding agent (Claude Code through its ACP adapter) as an ACP client.

/// Whether this build has the `testing` feature, whose test hooks a served Fronde must not
/// have: `fronde serve` refuses to start with it.
pub const TESTING: bool = cfg!(feature = "testing");

pub mod acp;
pub mod assistant;
pub mod confine;
pub mod elicitation;
#[cfg(feature = "testing")]
pub mod fake;
pub mod legacy;
pub mod log;
pub mod permission;
pub mod provider;
pub mod scope;
pub mod threads;
pub mod tokens;
pub mod usage;

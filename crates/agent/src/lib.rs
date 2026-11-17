//! Agent: drives a coding agent (Claude Code through its ACP adapter) as an ACP client.

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

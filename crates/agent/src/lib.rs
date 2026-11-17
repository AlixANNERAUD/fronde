//! Agent: drives a coding agent (Claude Code through its ACP adapter) as an ACP client.

pub mod acp;
pub mod assistant;
#[cfg(feature = "testing")]
pub mod fake;
pub mod legacy;
pub mod log;

//! Agent: drives a coding agent (Claude Code through its ACP adapter) as an ACP client.

pub mod acp;
#[cfg(feature = "testing")]
pub mod fake;

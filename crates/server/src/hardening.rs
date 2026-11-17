//! What `fronde serve` does to its own process before it reads a secret.

/// Set to `1` to keep core dumps and ptrace, for debugging.
pub const ALLOW_CORE_DUMPS: &str = "FRONDE_ALLOW_CORE_DUMPS";

/// On Linux, makes the process non-dumpable and sets `RLIMIT_CORE` to 0, so that a crash writes no
/// core with secrets in it. Non-dumpable also stops processes of the same user (the agent and its
/// tools among them) from reading `/proc/<pid>/environ` or `/proc/<pid>/mem` and from attaching
/// with ptrace. Does nothing elsewhere, or when `FRONDE_ALLOW_CORE_DUMPS=1`.
pub fn harden() -> anyhow::Result<()> {
    if std::env::var_os(ALLOW_CORE_DUMPS).is_some_and(|v| v == "1") {
        tracing::debug!("{ALLOW_CORE_DUMPS}=1: core dumps and ptrace left as they were");
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        use anyhow::Context;
        use rustix::process::{
            DumpableBehavior, Resource, Rlimit, set_dumpable_behavior, setrlimit,
        };
        set_dumpable_behavior(DumpableBehavior::NotDumpable)
            .context("cannot make the process non-dumpable")?;
        let none = Rlimit {
            current: Some(0),
            maximum: Some(0),
        };
        setrlimit(Resource::Core, none).context("cannot set RLIMIT_CORE to 0")?;
        tracing::debug!(
            "process made non-dumpable (no ptrace, no /proc access) and RLIMIT_CORE set to 0"
        );
    }
    Ok(())
}

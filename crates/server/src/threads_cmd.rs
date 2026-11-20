//! `fronde threads`: what is done to the threads while Fronde is stopped.

use anyhow::bail;
use fronde_hub::config::Config;
use std::io::Write;

/// `fronde threads reassign`: gives every conversation of `from` (a person's name, or
/// `removed:<name>`, which the threads of someone no longer configured go to), with everything
/// under it, to `to`, a configured user. Refused while Fronde runs on the data directory, and
/// for anyone not configured.
pub async fn reassign(
    config: &Config,
    from: &str,
    to: &str,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let users = config.user_names();
    if !users.iter().any(|user| user == to) {
        bail!(
            "--to {to:?} is not a user; name one of: {}",
            users.join(", ")
        );
    }
    fronde_hub::private::prepare_data_dir(&config.data.dir)?;
    let _running = fronde_db::running::claim(&config.data.dir, "fronde threads reassign")?;
    let db = fronde_db::open(&config.data.dir).await?;
    let given = fronde_agent::threads::reassign(&db, &config.data.dir, from, to).await;
    db.close().await?;
    let given = given?;
    writeln!(
        out,
        "gave {given} thread(s) of {from} to {to}, sub-agents included"
    )?;
    Ok(())
}

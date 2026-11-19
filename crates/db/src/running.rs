//! Which process is using a data directory for something no other may do at the same time:
//! `fronde serve`, or a rotation or move of the master key, which re-seals or moves what a
//! running Fronde reads. Each holds an exclusive lock on [`FILE`] in the data directory, across
//! processes, for as long as it runs, with its process id and what it does written in the file,
//! so that the next one can say who holds it. The operating system releases the lock when the
//! process ends, crash included: a leftover file holds nothing.

use anyhow::{Context as _, bail};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::Path;

/// The lock file, in the data directory.
pub const FILE: &str = "fronde.lock";

/// How often, and how long apart, [`claim`] tries a lock held for an instant again: 100 ms in
/// all.
const BRIEF_TRIES: u32 = 10;
const BRIEF_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

/// The data directory, held until dropped.
#[derive(Debug)]
#[must_use = "the data directory is released when this is dropped"]
pub struct Running {
    _file: std::fs::File,
}

/// Hold `data_dir` for `what` (such as `fronde serve`), or say which process holds it.
pub fn claim(data_dir: &Path, what: &str) -> anyhow::Result<Running> {
    let path = data_dir.join(FILE);
    let shown = path.display();
    let mut file = fronde_fs::private_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open the lock file {shown}"))?;
    // `fronde check` locks the file shared for an instant to see whether it is held
    // ([`is_free`]): a lock refused for that long is tried again before naming a holder.
    let mut locked = file.try_lock();
    for _ in 0..BRIEF_TRIES {
        if !matches!(locked, Err(std::fs::TryLockError::WouldBlock)) {
            break;
        }
        std::thread::sleep(BRIEF_WAIT);
        locked = file.try_lock();
    }
    match locked {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut holder = String::new();
            // Read through the lock: it is advisory. The holder may not have written yet.
            let _ = file.read_to_string(&mut holder);
            let holder = holder.trim();
            let who = match holder.split_once(' ') {
                Some((pid, what)) => format!("process {pid} ({what})"),
                None if !holder.is_empty() => format!("process {holder}"),
                None => "another process".to_owned(),
            };
            bail!(
                "Fronde is running on the data directory {}: {who} holds {shown}. Stop it first \
                 (`systemctl --user stop fronde` for the service), then try again",
                data_dir.display()
            );
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("cannot lock {shown}"));
        }
    }
    let written = file
        .set_len(0)
        .and_then(|()| file.rewind())
        .and_then(|()| writeln!(file, "{} {what}", std::process::id()));
    written.with_context(|| format!("cannot write to the lock file {shown}"))?;
    Ok(Running { _file: file })
}

/// Whether no process holds `data_dir` now, told without creating or writing anything: the lock
/// file, if there is one, is opened to read and locked shared for an instant. `false` when it
/// cannot be told.
pub fn is_free(data_dir: &Path) -> bool {
    match std::fs::File::open(data_dir.join(FILE)) {
        Ok(file) => file.try_lock_shared().is_ok(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

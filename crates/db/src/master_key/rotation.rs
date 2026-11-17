//! Rotating the master key, and moving it to another backend, without ever leaving the stored
//! secrets with no key that opens them, whatever step a crash interrupts.
//!
//! A rotation goes in five steps, each durable before the next starts:
//!
//! 1. the database records that a rotation is under way (`secrets`/`rotation` in `kv`, the
//!    backend's name);
//! 2. the new key goes to the backend's next slot (`master.key.next`, or the keychain entry's
//!    user followed by `.next`), and is read back;
//! 3. one write transaction seals every secret and the key check again with the new key;
//! 4. the new key replaces the current one (the file is replaced atomically);
//! 5. the next slot is emptied, then the record removed.
//!
//! Until step 3 commits, the current key opens the key check; from then on, the next slot does,
//! until step 4 puts the same key in the current slot. [`finish`], run at start and before any
//! rotation or move, ends an interrupted one: the key that opens the key check becomes or stays
//! the current one, and the next slot is emptied. Both keys are never lost: the one that opens is
//! removed from nowhere before it is in the current slot.
//!
//! A move copies the same key to the other backend, reads it back there, checks that it opens
//! the key check, records the new backend in the same write transaction as [`crate::secrets`]
//! does, and only then removes the key from the old backend: a crash leaves it in one or both,
//! with the record naming one that holds it.

use super::backends::Slot;
use super::{Backend, Backends, MasterKey};
use anyhow::{Context as _, bail};
use sea_orm::DatabaseConnection;

/// Where the record of a rotation under way lives in `kv`, beside the key check.
const ROTATION_KEY: &str = "rotation";

/// The steps of a rotation and of a move, after each of which a test may stop it as a crash
/// would ([`rotate_crashing_after`], [`move_crashing_after`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The database records that a rotation is under way.
    Marked,
    /// The new key is in the next slot.
    Staged,
    /// Every secret and the key check are sealed with the new key.
    Resealed,
    /// The new key is the current one.
    Promoted,
    /// The next slot is empty; the record of the rotation goes next.
    Cleaned,
    /// The key is in the new backend.
    Copied,
    /// The database records the new backend.
    Recorded,
}

impl Step {
    /// A rotation's steps, in order.
    pub const ROTATION: [Step; 5] = [
        Step::Marked,
        Step::Staged,
        Step::Resealed,
        Step::Promoted,
        Step::Cleaned,
    ];
    /// A move's steps, in order, before the key is removed from the old backend.
    pub const MOVE: [Step; 2] = [Step::Copied, Step::Recorded];
}

/// What a rotation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rotated {
    /// How many stored secrets were sealed again.
    pub secrets: usize,
    /// Where the new key is kept.
    pub place: String,
}

/// What [`finish`] did with a rotation it found interrupted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finished {
    /// The new key opened the key check: it is now the current key.
    Promoted,
    /// The current key still opened the key check: the new key, which sealed nothing, is gone.
    Undone,
    /// Only the record of the rotation was left: it is gone.
    Cleared,
}

/// What a move did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub from: Backend,
    pub to: Backend,
    /// Where the key was kept.
    pub from_place: String,
    /// Where the key is kept now.
    pub to_place: String,
    /// Whether it was removed from the old backend: never from a systemd credential, which the
    /// unit loads.
    pub removed: bool,
}

/// The backend of a rotation under way, as recorded at its first step; `None` when there is none.
pub(crate) async fn interrupted(db: &DatabaseConnection) -> anyhow::Result<Option<Backend>> {
    crate::kv::get(db, crate::secrets::CHECK_SCOPE, ROTATION_KEY)
        .await
        .context("cannot read whether a rotation of the master key is under way")?
        .map(|name| super::parse_recorded(&name))
        .transpose()
}

async fn mark(db: &DatabaseConnection, backend: Backend) -> anyhow::Result<()> {
    crate::kv::set(
        db,
        crate::secrets::CHECK_SCOPE,
        ROTATION_KEY,
        backend.name().as_bytes().to_vec(),
        None,
    )
    .await
    .context("cannot record the rotation of the master key")
}

async fn unmark(db: &DatabaseConnection) -> anyhow::Result<()> {
    crate::kv::delete(db, crate::secrets::CHECK_SCOPE, ROTATION_KEY)
        .await
        .context("cannot remove the record of the rotation of the master key")?;
    Ok(())
}

/// Stops at `step` as a crash would, when `crash` names it.
fn crash(crash: Option<Step>, step: Step) -> anyhow::Result<()> {
    match crash {
        Some(at) if at == step => bail!("stopped after {step:?}, as a crash would"),
        _ => Ok(()),
    }
}

/// End a rotation that a crash interrupted, if the database records one: the key that opens the
/// key check becomes or stays the current key, and the next slot is emptied. `None` when no
/// rotation was under way. Called by `fronde serve` at start, before the key is loaded, and
/// before a rotation or a move.
pub async fn finish(
    db: &DatabaseConnection,
    backends: &Backends,
) -> anyhow::Result<Option<Finished>> {
    let Some(backend) = interrupted(db).await? else {
        return Ok(None);
    };
    let entry = backends.entry(db).await?;
    let next = backends.read_slot(backend, &entry, Slot::Next).await?;
    let next_place = backends.slot_place(backend, &entry, Slot::Next);
    let current_place = backends.slot_place(backend, &entry, Slot::Current);
    let Some(next) = next else {
        unmark(db).await?;
        return Ok(Some(Finished::Cleared));
    };
    let current = backends.read_slot(backend, &entry, Slot::Current).await?;
    let current_opens = match &current {
        Some(current) => crate::secrets::check_key(db, current).await.is_ok(),
        None => false,
    };
    let finished = if current_opens || !crate::secrets::any_stored(db).await? {
        let same = current.is_some_and(|current| current.encode() == next.encode());
        if !same {
            tracing::warn!(
                "a rotation of the master key was interrupted before the stored secrets were \
                 sealed with the new key: the key {current_place} still opens them, and the new \
                 one is removed from {next_place}"
            );
        }
        backends.delete_slot(backend, &entry, Slot::Next).await?;
        if same {
            Finished::Cleared
        } else {
            Finished::Undone
        }
    } else {
        crate::secrets::check_key(db, &next)
            .await
            .with_context(|| {
                format!(
                    "a rotation of the master key was interrupted, and neither the key \
                 {current_place} nor the new one {next_place} opens the stored secrets; restore \
                 the key from a backup"
                )
            })?;
        backends
            .write_slot(backend, &entry, Slot::Current, &next)
            .await?;
        backends.delete_slot(backend, &entry, Slot::Next).await?;
        tracing::warn!(
            "a rotation of the master key was interrupted after the stored secrets were sealed \
             with the new key: the new key is now the one {current_place}; back it up"
        );
        Finished::Promoted
    };
    unmark(db).await?;
    Ok(Some(finished))
}

/// Make a new master key, seal every stored secret again with it and keep it where the current
/// one is, in place of it. Fronde must not be running (the caller holds the data directory, see
/// [`crate::running`]). A systemd credential is refused: Fronde never writes it.
pub async fn rotate(db: &DatabaseConnection, backends: &Backends) -> anyhow::Result<Rotated> {
    rotate_with(db, backends, None).await
}

/// [`rotate`], stopped after `step` as a crash would: nothing done after it is undone.
#[cfg(feature = "testing")]
pub async fn rotate_crashing_after(
    db: &DatabaseConnection,
    backends: &Backends,
    step: Step,
) -> anyhow::Result<Rotated> {
    rotate_with(db, backends, Some(step)).await
}

async fn rotate_with(
    db: &DatabaseConnection,
    backends: &Backends,
    stop: Option<Step>,
) -> anyhow::Result<Rotated> {
    let _lock = backends.lock().await?;
    finish(db, backends).await?;
    let Some(old) = backends.load(db).await? else {
        bail!("no secret is stored: there is no master key to rotate yet");
    };
    let backend = old.backend();
    if backend == Backend::Systemd {
        bail!(
            "the master key is the systemd credential {}, which Fronde never writes; to rotate \
             it, encrypt a new key and seal the secrets again: move the key to a file \
             (`fronde secret move-key --to file`), rotate it there, then make the credential \
             from the new file with `systemd-creds encrypt`",
            super::CREDENTIAL
        );
    }
    let entry = backends.entry(db).await?;
    let new = MasterKey::generate(backend)?;
    mark(db, backend).await?;
    crash(stop, Step::Marked)?;
    backends
        .write_slot(backend, &entry, Slot::Next, &new)
        .await?;
    crash(stop, Step::Staged)?;
    let secrets = crate::secrets::reseal(db, &old, &new).await?;
    crash(stop, Step::Resealed)?;
    backends
        .write_slot(backend, &entry, Slot::Current, &new)
        .await?;
    crash(stop, Step::Promoted)?;
    backends.delete_slot(backend, &entry, Slot::Next).await?;
    crash(stop, Step::Cleaned)?;
    unmark(db).await?;
    Ok(Rotated {
        secrets,
        place: backends.slot_place(backend, &entry, Slot::Current),
    })
}

/// Keep the master key in `to` instead of where it is now: the same key, copied there, read back
/// and checked against the stored secrets, recorded as the backend in use, then removed from the
/// old one. Refused when `[secrets] backend` names another backend, when `to` cannot be written
/// (a locked keychain) or already holds another key, and for a systemd credential, which only
/// `systemd-creds` makes. Fronde must not be running.
pub async fn move_to(
    db: &DatabaseConnection,
    backends: &Backends,
    to: Backend,
) -> anyhow::Result<Moved> {
    move_with(db, backends, to, None).await
}

/// [`move_to`], stopped after `step` as a crash would.
#[cfg(feature = "testing")]
pub async fn move_crashing_after(
    db: &DatabaseConnection,
    backends: &Backends,
    to: Backend,
    step: Step,
) -> anyhow::Result<Moved> {
    move_with(db, backends, to, Some(step)).await
}

async fn move_with(
    db: &DatabaseConnection,
    backends: &Backends,
    to: Backend,
    stop: Option<Step>,
) -> anyhow::Result<Moved> {
    let _lock = backends.lock().await?;
    finish(db, backends).await?;
    if !crate::secrets::any_stored(db).await? {
        bail!(
            "no secret is stored, so there is no master key to move; set [secrets] backend = \
             \"{to}\" and the first secret stored makes its key there"
        );
    }
    if let Some(named) = backends.named()
        && named != to
    {
        bail!(
            "[secrets] backend = \"{named}\" in the configuration: Fronde would keep reading the \
             key there. Set it to \"auto\" or \"{to}\" first, then move the key"
        );
    }
    let entry = backends.entry(db).await?;
    // The key is where the database says, whatever the configuration names now.
    let from = crate::secrets::recorded_backend(db)
        .await?
        .unwrap_or(Backend::File);
    let from_place = backends.slot_place(from, &entry, Slot::Current);
    let to_place = backends.slot_place(to, &entry, Slot::Current);
    if from == to {
        bail!("the master key is already kept in the {to} backend ({to_place})");
    }
    if to == Backend::Systemd {
        bail!(
            "Fronde never writes a systemd credential. Make it from the key yourself: with the \
             key in a file, run `systemd-creds encrypt --name={0} <key file> <credential>`, add \
             LoadCredentialEncrypted={0}:<credential> to the unit, set [secrets] backend = \
             \"systemd\", start Fronde under the unit (it then records the move), and only then \
             remove the key file",
            super::CREDENTIAL
        );
    }
    let key = backends
        .read_slot(from, &entry, Slot::Current)
        .await?
        .with_context(|| format!("the master key is missing from {from_place}"))?;
    crate::secrets::check_key(db, &key)
        .await
        .with_context(|| format!("master key {from_place}"))?;
    match backends.read_slot(to, &entry, Slot::Current).await? {
        Some(there) if there.encode() == key.encode() => {}
        Some(_) => bail!(
            "{to_place} already holds another master key; Fronde does not replace it, since \
             it may be the only copy of a key you still need. Remove it yourself, then move the \
             key again"
        ),
        None => backends.write_slot(to, &entry, Slot::Current, &key).await?,
    }
    crash(stop, Step::Copied)?;
    let placed = backends
        .read_slot(to, &entry, Slot::Current)
        .await?
        .with_context(|| format!("the master key just written to {to_place} is not there"))?;
    // Checks the key against the stored secrets again and records the backend, and the keychain
    // entry, in one write transaction.
    crate::secrets::upgrade_key_check(db, &placed)
        .await
        .with_context(|| format!("master key {to_place}"))?;
    crash(stop, Step::Recorded)?;
    let removed = backends.delete_slot(from, &entry, Slot::Current).await?;
    Ok(Moved {
        from,
        to,
        from_place,
        to_place,
        removed,
    })
}

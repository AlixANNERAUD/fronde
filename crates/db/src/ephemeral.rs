//! Ephemeral secrets: values held in Fronde's memory only, never in the database, a log or the
//! conversation, each behind an opaque reference (`sr_` and 128 random bits) that works only in
//! the conversation and for the asker it was given to, until it expires (1 hour by default, 24
//! at most) and, when single use, once. Values are `SecretString`s, wiped when they are taken,
//! expire, are forgotten with their conversation or turn, and when the store is dropped.
//!
//! Every refusal is the same [`RefError`], so a probe learns nothing about which references
//! exist.

use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use secrecy::SecretString;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How long an ephemeral secret lives unless told otherwise.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
/// The longest an ephemeral secret may live.
pub const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const PREFIX: &str = "sr_";

/// Whose secret it is: the stream (conversation) it was given in, the asker it was given to
/// (`assistant`, `mcp:<client>`), and, when it is for one turn only, that turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EphemeralScope {
    pub topic: String,
    pub asker: String,
    pub turn: Option<u64>,
}

impl EphemeralScope {
    /// Whether a consumer acting in `scope` may take a secret held for `self`: the same
    /// conversation and asker, and the same turn when the secret is for one turn.
    fn admits(&self, scope: &Self) -> bool {
        self.topic == scope.topic
            && self.asker == scope.asker
            && (self.turn.is_none() || self.turn == scope.turn)
    }
}

/// The reference an asker gets instead of the value: `sr_` and 22 base64url characters.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SecretRef(String);

impl SecretRef {
    fn new() -> anyhow::Result<Self> {
        let mut bits = [0u8; 16];
        getrandom::fill(&mut bits).map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
        Ok(Self(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bits))))
    }

    /// The reference as the asker is given it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Only its prefix: a reference is no secret, but nothing needs it in a log.
impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretRef(sr_…)")
    }
}

/// A reference that does not give a value, whatever the reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefError;

impl fmt::Display for RefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "this secret reference is unknown, expired or not for this conversation; ask the \
             user again with user_secret",
        )
    }
}

impl std::error::Error for RefError {}

struct Held {
    scope: EphemeralScope,
    value: SecretString,
    expires_at: DateTime<Utc>,
    once: bool,
}

/// The ephemeral secrets of this Fronde process.
#[derive(Default)]
pub struct EphemeralSecrets {
    held: Mutex<HashMap<String, Held>>,
}

impl fmt::Debug for EphemeralSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EphemeralSecrets")
            .field("held", &self.held().len())
            .finish()
    }
}

impl EphemeralSecrets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Holds `value` for `scope` during `ttl` (at most [`MAX_TTL`]); `once`: the first take
    /// removes it. Returns its reference and when it expires.
    pub fn put(
        &self,
        scope: EphemeralScope,
        value: SecretString,
        ttl: Duration,
        once: bool,
    ) -> anyhow::Result<(SecretRef, DateTime<Utc>)> {
        if ttl.is_zero() || ttl > MAX_TTL {
            bail!("an ephemeral secret lives more than 0 and at most {MAX_TTL:?}, not {ttl:?}");
        }
        let now = Utc::now();
        let expires_at = now + chrono::Duration::from_std(ttl)?;
        let reference = SecretRef::new()?;
        let mut held = self.held();
        sweep(&mut held, now);
        held.insert(
            reference.0.clone(),
            Held {
                scope,
                value,
                expires_at,
                once,
            },
        );
        Ok((reference, expires_at))
    }

    /// The value behind `reference`, for a consumer acting in `scope`. A single-use secret is
    /// removed by this call. Any refusal is the same [`RefError`].
    pub fn take(&self, reference: &str, scope: &EphemeralScope) -> Result<SecretString, RefError> {
        self.take_at(reference, scope, Utc::now())
    }

    fn take_at(
        &self,
        reference: &str,
        scope: &EphemeralScope,
        now: DateTime<Utc>,
    ) -> Result<SecretString, RefError> {
        let mut held = self.held();
        sweep(&mut held, now);
        let entry = held.get(reference).ok_or(RefError)?;
        if !entry.scope.admits(scope) {
            return Err(RefError);
        }
        if entry.once {
            return held
                .remove(reference)
                .map(|held| held.value)
                .ok_or(RefError);
        }
        Ok(entry.value.clone())
    }

    /// Forgets the secrets held for one turn of `topic`; returns how many.
    pub fn forget_turn(&self, topic: &str, turn: u64) -> usize {
        self.forget_where(|scope| scope.topic == topic && scope.turn == Some(turn))
    }

    /// Forgets every secret of `topic`, when its conversation starts over; returns how many.
    pub fn forget_topic(&self, topic: &str) -> usize {
        self.forget_where(|scope| scope.topic == topic)
    }

    /// Forgets the secret behind `reference`, whoever holds it; true if there was one. For the
    /// one who put it, when what it was for fell through.
    pub fn forget(&self, reference: &str) -> bool {
        self.held().remove(reference).is_some()
    }

    /// Forgets the expired secrets; returns how many. Every put and take does it too.
    pub fn sweep(&self) -> usize {
        sweep(&mut self.held(), Utc::now())
    }

    /// Forgets every secret: at shutdown.
    pub fn clear(&self) {
        self.held().clear();
    }

    /// How many secrets are held.
    pub fn len(&self) -> usize {
        self.held().len()
    }

    pub fn is_empty(&self) -> bool {
        self.held().is_empty()
    }

    fn forget_where(&self, matches: impl Fn(&EphemeralScope) -> bool) -> usize {
        let mut held = self.held();
        let before = held.len();
        held.retain(|_, held| !matches(&held.scope));
        before - held.len()
    }

    fn held(&self) -> MutexGuard<'_, HashMap<String, Held>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn sweep(held: &mut HashMap<String, Held>, now: DateTime<Utc>) -> usize {
    let before = held.len();
    held.retain(|_, held| held.expires_at > now);
    before - held.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret as _;

    fn scope(topic: &str, turn: Option<u64>) -> EphemeralScope {
        EphemeralScope {
            topic: topic.into(),
            asker: "assistant".into(),
            turn,
        }
    }

    fn put(store: &EphemeralSecrets, scope: EphemeralScope, value: &str, once: bool) -> String {
        let (reference, _) = store
            .put(scope, SecretString::from(value), DEFAULT_TTL, once)
            .unwrap();
        reference.as_str().to_owned()
    }

    #[test]
    fn ephemeral_ref_roundtrip() {
        let store = EphemeralSecrets::new();
        let (reference, expires_at) = store
            .put(
                scope("conversation", None),
                SecretString::from("s3cret"),
                DEFAULT_TTL,
                false,
            )
            .unwrap();
        let text = reference.as_str();
        assert!(text.starts_with("sr_") && text.len() == 25, "{text}");
        assert!(
            text[3..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        let left = expires_at - Utc::now();
        assert!(left <= chrono::Duration::hours(1) && left > chrono::Duration::minutes(59));
        // Any turn of the conversation, when the secret is for the conversation.
        let value = store.take(text, &scope("conversation", Some(4))).unwrap();
        assert_eq!(value.expose_secret(), "s3cret");
        // Not single use: still there.
        assert!(store.take(text, &scope("conversation", None)).is_ok());
        assert!(!format!("{reference:?}").contains(&text[3..]));
        assert_ne!(
            put(&store, scope("conversation", None), "x", true),
            text,
            "each reference is new"
        );
    }

    #[test]
    fn ephemeral_ref_other_conversation_refused() {
        let store = EphemeralSecrets::new();
        let reference = put(&store, scope("conversation", Some(2)), "s3cret", true);
        let other_asker = EphemeralScope {
            asker: "mcp:Codex".into(),
            ..scope("conversation", Some(2))
        };
        for wrong in [
            scope("other", Some(2)),
            scope("conversation", Some(3)),
            scope("conversation", None),
            other_asker,
        ] {
            assert_eq!(store.take(&reference, &wrong).unwrap_err(), RefError);
        }
        // A refused take does not use it up.
        assert!(
            store
                .take(&reference, &scope("conversation", Some(2)))
                .is_ok()
        );
    }

    #[test]
    fn ephemeral_ref_expires() {
        let store = EphemeralSecrets::new();
        let reference = put(&store, scope("conversation", None), "s3cret", false);
        let later = Utc::now() + chrono::Duration::hours(1) + chrono::Duration::seconds(1);
        assert_eq!(
            store
                .take_at(&reference, &scope("conversation", None), later)
                .unwrap_err(),
            RefError
        );
        // Gone, not only refused.
        assert!(store.is_empty());
        assert!(
            store
                .put(
                    scope("conversation", None),
                    SecretString::from("x"),
                    MAX_TTL + Duration::from_secs(1),
                    true
                )
                .is_err()
        );
        assert!(
            store
                .put(
                    scope("conversation", None),
                    SecretString::from("x"),
                    Duration::ZERO,
                    true
                )
                .is_err()
        );
        let (_, expires_at) = store
            .put(
                scope("conversation", None),
                SecretString::from("x"),
                Duration::from_millis(1),
                true,
            )
            .unwrap();
        while Utc::now() <= expires_at {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(store.sweep(), 1);
    }

    #[test]
    fn ephemeral_ref_once_is_used_once() {
        let store = EphemeralSecrets::new();
        let reference = put(&store, scope("conversation", Some(1)), "s3cret", true);
        let value = store
            .take(&reference, &scope("conversation", Some(1)))
            .unwrap();
        assert_eq!(value.expose_secret(), "s3cret");
        assert_eq!(
            store
                .take(&reference, &scope("conversation", Some(1)))
                .unwrap_err(),
            RefError
        );
        assert!(store.is_empty());
    }

    #[test]
    fn ephemeral_ref_forged_refused() {
        let store = EphemeralSecrets::new();
        let reference = put(&store, scope("conversation", None), "s3cret", true);
        let mut forged = reference.clone();
        forged.pop();
        forged.push(if reference.ends_with('A') { 'B' } else { 'A' });
        for wrong in [
            forged.as_str(),
            "sr_AAAAAAAAAAAAAAAAAAAAAA",
            "",
            &reference[3..],
        ] {
            let error = store.take(wrong, &scope("conversation", None)).unwrap_err();
            // The same message for every refusal.
            assert_eq!(
                error.to_string(),
                "this secret reference is unknown, expired or not for this conversation; ask \
                 the user again with user_secret"
            );
        }
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn cancel_forgets_turn_secrets() {
        let store = EphemeralSecrets::new();
        let turn = put(&store, scope("conversation", Some(5)), "a", true);
        let other_turn = put(&store, scope("conversation", Some(6)), "b", true);
        let conversation = put(&store, scope("conversation", None), "c", true);
        assert_eq!(store.forget_turn("conversation", 5), 1);
        assert!(store.take(&turn, &scope("conversation", Some(5))).is_err());
        assert!(
            store
                .take(&other_turn, &scope("conversation", Some(6)))
                .is_ok()
        );
        let elsewhere = put(&store, scope("other", None), "d", true);
        let dropped = put(&store, scope("other", None), "e", true);
        assert!(store.forget(&dropped) && !store.forget(&dropped));
        assert_eq!(store.forget_topic("conversation"), 1);
        assert!(
            store
                .take(&conversation, &scope("conversation", None))
                .is_err()
        );
        assert_eq!(store.len(), 1);
        store.clear();
        assert!(store.take(&elsewhere, &scope("other", None)).is_err());
        assert!(store.is_empty());
    }
}

//! The hub tokens of Fronde's agent processes: one per process, made when it starts, given to it
//! only through its environment, and revoked when Fronde lets the process go. The hub's gate
//! knows a thread by the token its agent holds, and by nothing else.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::anyhow;
use fronde_protocol::ThreadId;
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use subtle::{ConditionallySelectable, ConstantTimeEq};

/// The live tokens, by lease. Cheap to clone: every clone is the same set.
#[derive(Clone, Default)]
pub struct Tokens {
    live: Arc<Mutex<Live>>,
}

#[derive(Default)]
struct Live {
    next: u64,
    /// The SHA-256 digest of each live token, and its thread; never the token itself.
    by_lease: HashMap<u64, ([u8; 32], ThreadId)>,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("live", &self.live().by_lease.len())
            .finish()
    }
}

impl Tokens {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new token for an agent process of `thread`: 256 random bits, valid until the lease is
    /// dropped.
    pub fn issue(&self, thread: ThreadId) -> anyhow::Result<TokenLease> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| anyhow!("no randomness available: {e}"))?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let digest = digest(token.as_bytes());
        let lease = {
            let mut live = self.live();
            live.next += 1;
            let lease = live.next;
            live.by_lease.insert(lease, (digest, thread));
            lease
        };
        Ok(TokenLease {
            tokens: self.clone(),
            lease,
            thread,
            token: Some(SecretString::from(token)),
        })
    }

    /// The thread whose live token `given` is, if any. Every live token's digest is compared,
    /// each in constant time and without stopping at a match, so the time taken tells nothing
    /// of which token was close, nor whether one matched. It grows with the number of live
    /// tokens: all the timing reveals is how many agents are running.
    pub fn lookup(&self, given: &[u8]) -> Option<ThreadId> {
        let given = digest(given);
        let live = self.live();
        let mut found = subtle::Choice::from(0);
        let mut thread = 0i64;
        for (digest, id) in live.by_lease.values() {
            let same = given.ct_eq(digest);
            thread.conditional_assign(&id.0, same);
            found |= same;
        }
        bool::from(found).then_some(ThreadId(thread))
    }

    /// How many tokens are live.
    pub fn len(&self) -> usize {
        self.live().by_lease.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// A live token: revoked when dropped. It holds the token itself only until
/// [`TokenLease::take_token`] hands it to the agent's start; then only its digest is kept.
pub struct TokenLease {
    tokens: Tokens,
    lease: u64,
    thread: ThreadId,
    token: Option<SecretString>,
}

impl TokenLease {
    /// The token, for the agent's environment only; the lease keeps it no longer. `None` once
    /// taken.
    pub fn take_token(&mut self) -> Option<SecretString> {
        self.token.take()
    }

    /// Revokes the token now, before the lease is dropped: for an agent Fronde lets go while a
    /// turn still holds its session.
    pub fn revoke(&self) {
        self.tokens.live().by_lease.remove(&self.lease);
    }

    pub fn thread(&self) -> ThreadId {
        self.thread
    }
}

impl fmt::Debug for TokenLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenLease")
            .field("thread", &self.thread)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl Drop for TokenLease {
    fn drop(&mut self) {
        self.tokens.live().by_lease.remove(&self.lease);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret as _;

    #[test]
    fn a_token_names_its_thread_until_its_lease_is_dropped() {
        let tokens = Tokens::new();
        let mut first = tokens.issue(ThreadId(1)).unwrap();
        let mut second = tokens.issue(ThreadId(2)).unwrap();
        let mut again = tokens.issue(ThreadId(2)).unwrap();
        let token = |lease: &mut TokenLease| {
            lease
                .take_token()
                .unwrap()
                .expose_secret()
                .as_bytes()
                .to_vec()
        };
        let (first_token, second_token, again_token) =
            (token(&mut first), token(&mut second), token(&mut again));
        // Taken once: the lease keeps only the digest.
        assert!(first.take_token().is_none());
        assert_eq!(first_token.len(), 64);
        assert_ne!(second_token, again_token);
        assert_eq!(tokens.lookup(&first_token), Some(ThreadId(1)));
        assert_eq!(tokens.lookup(&second_token), Some(ThreadId(2)));
        assert_eq!(tokens.lookup(&again_token), Some(ThreadId(2)));
        assert_eq!(tokens.lookup(b""), None);
        assert_eq!(tokens.lookup(&first_token[..63]), None);
        drop(second);
        assert_eq!(tokens.lookup(&second_token), None);
        assert_eq!(tokens.lookup(&again_token), Some(ThreadId(2)));
        assert_eq!(tokens.len(), 2);
        let shown = format!("{again:?}");
        assert!(!shown.contains(std::str::from_utf8(&again_token).unwrap()));
        // Revoked before the lease goes, and again when it goes.
        again.revoke();
        assert_eq!(tokens.lookup(&again_token), None);
        assert_eq!(tokens.len(), 1);
        drop((first, again));
        assert!(tokens.is_empty());
    }
}

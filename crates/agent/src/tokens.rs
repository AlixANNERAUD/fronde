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
            token: SecretString::from(token),
        })
    }

    /// The thread whose live token `given` is, if any. Every live token's digest is compared,
    /// each in constant time and without stopping at a match, so the time taken tells nothing
    /// of which token was close, nor whether one matched.
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

/// A live token: revoked when dropped.
pub struct TokenLease {
    tokens: Tokens,
    lease: u64,
    thread: ThreadId,
    token: SecretString,
}

impl TokenLease {
    /// The token, for the agent's environment only.
    pub fn token(&self) -> &SecretString {
        &self.token
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
        let first = tokens.issue(ThreadId(1)).unwrap();
        let second = tokens.issue(ThreadId(2)).unwrap();
        let again = tokens.issue(ThreadId(2)).unwrap();
        let token = |lease: &TokenLease| lease.token().expose_secret().as_bytes().to_vec();
        assert_eq!(token(&first).len(), 64);
        assert_ne!(token(&second), token(&again));
        assert_eq!(tokens.lookup(&token(&first)), Some(ThreadId(1)));
        assert_eq!(tokens.lookup(&token(&second)), Some(ThreadId(2)));
        assert_eq!(tokens.lookup(&token(&again)), Some(ThreadId(2)));
        assert_eq!(tokens.lookup(b""), None);
        assert_eq!(tokens.lookup(&token(&first)[..63]), None);
        let revoked = token(&second);
        drop(second);
        assert_eq!(tokens.lookup(&revoked), None);
        assert_eq!(tokens.lookup(&token(&again)), Some(ThreadId(2)));
        assert_eq!(tokens.len(), 2);
        assert!(!format!("{again:?}").contains(again.token().expose_secret()));
        drop((first, again));
        assert!(tokens.is_empty());
    }
}

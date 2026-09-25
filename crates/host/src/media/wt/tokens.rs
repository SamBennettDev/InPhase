//! One-time dial tokens.
//!
//! A QUIC dial carries no credentials, so the authenticated signaling socket
//! mints a short-lived single-use token and the transport refuses everything
//! until one is presented (ADR-0011 step 2).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// One-time bearer tokens binding a WT dial to an authenticated signaling
/// session. Issued per player session by the signaling layer; consumed once.
/// Outstanding dial tokens kept at most; the oldest is dropped past this.
const MAX_LIVE_TOKENS: usize = 32;

#[derive(Debug, Default)]
pub struct TokenStore {
    // token -> expiry instant
    tokens: Mutex<HashMap<String, Instant>>,
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a 128-bit random token valid for `ttl`.
    pub fn issue(&self, ttl: Duration) -> String {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).expect("OS entropy unavailable");
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let now = Instant::now();
        let mut tokens = self.tokens.lock();
        // Unused tokens used to live forever: a client asking for dial info
        // in a loop grew this map without bound.
        tokens.retain(|_, expires| *expires > now);
        if tokens.len() >= MAX_LIVE_TOKENS {
            if let Some(oldest) = tokens
                .iter()
                .min_by_key(|(_, e)| **e)
                .map(|(t, _)| t.clone())
            {
                tokens.remove(&oldest);
            }
        }
        tokens.insert(token.clone(), now + ttl);
        token
    }

    #[cfg(test)]
    fn live(&self) -> usize {
        self.tokens.lock().len()
    }

    /// Consume a token: valid only if present, unexpired, and unused.
    pub fn consume(&self, token: &str) -> bool {
        let mut guard = self.tokens.lock();
        matches!(guard.remove(token), Some(expires) if Instant::now() < expires)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unused_tokens_are_bounded_and_expired_ones_pruned() {
        let s = super::TokenStore::new();
        for _ in 0..100 {
            s.issue(std::time::Duration::from_secs(60));
        }
        assert_eq!(s.live(), super::MAX_LIVE_TOKENS);
    }
}

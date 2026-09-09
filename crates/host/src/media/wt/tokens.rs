//! One-time dial tokens.
//!
//! A QUIC dial carries no credentials, so the authenticated signaling socket
//! mints a short-lived single-use token and the transport refuses everything
//! until one is presented (ADR-0011 step 2).

use parking_lot::Mutex;
use rand::Rng;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// One-time bearer tokens binding a WT dial to an authenticated signaling
/// session. Issued per player session by the signaling layer; consumed once.
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
        self.tokens
            .lock()
            .insert(token.clone(), Instant::now() + ttl);
        token
    }

    /// Consume a token: valid only if present, unexpired, and unused.
    pub fn consume(&self, token: &str) -> bool {
        let mut guard = self.tokens.lock();
        match guard.remove(token) {
            Some(expires) if Instant::now() < expires => true,
            _ => false,
        }
    }
}

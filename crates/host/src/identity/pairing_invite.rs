//! One-time pairing invitations.
//!
//! In addition to the Host-screen PIN, the dashboard can mint a short-lived
//! invitation the operator shares as a QR code or a typed short code. The
//! browser presents the invitation over the host's own HTTPS — the TLS channel
//! is already confidential and host-authenticated (a real Tailscale /
//! Let's Encrypt cert), so no PAKE or HMAC proof is needed: holding the secret
//! (or its short code) within the TTL *is* the proof.
//!
//! The first controller on a fresh Host also needs an explicit **Approve** click
//! on the dashboard before the invitation completes.

use std::time::{SystemTime, UNIX_EPOCH};

use hkdf::Hkdf;
use parking_lot::Mutex;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::hex;

const DEFAULT_TTL_SECS: u64 = 120;
const MAX_LIVE_INVITES: usize = 8;
/// Crockford-ish base32 without the ambiguous letters.
const B32: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Invite {
    id: String,
    secret: [u8; 32],
    short_code: String,
    created_unix: u64,
    not_after_unix: u64,
    needs_approval: bool,
    approved: bool,
    used: bool,
}

impl Invite {
    fn expired(&self) -> bool {
        now() > self.not_after_unix
    }
    fn live(&self) -> bool {
        !self.used && !self.expired()
    }
}

/// Public view for the dashboard / status polling.
#[derive(serde::Serialize)]
pub struct InviteView {
    pub id: String,
    pub secret_b64url: String,
    pub short_code: String,
    pub not_after_unix: u64,
    pub needs_approval: bool,
    pub approved: bool,
    pub used: bool,
}

pub enum ConsumeError {
    NotFound,
    Expired,
    AlreadyUsed,
    NeedsApproval,
}

impl ConsumeError {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConsumeError::NotFound => "invite not found",
            ConsumeError::Expired => "invite expired",
            ConsumeError::AlreadyUsed => "invite already used",
            ConsumeError::NeedsApproval => "waiting for approval on the PC",
        }
    }
}

pub struct InviteStore {
    inner: Mutex<Vec<Invite>>,
}

impl InviteStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
        }
    }

    /// Mint a new invitation. `needs_approval` should be `acl.is_first()`.
    pub fn create(&self, needs_approval: bool) -> InviteView {
        let mut g = self.inner.lock();
        g.retain(|i| i.live());
        if g.len() >= MAX_LIVE_INVITES {
            g.remove(0);
        }
        let mut idb = [0u8; 8];
        let mut secret = [0u8; 32];
        let _ = getrandom::getrandom(&mut idb);
        let _ = getrandom::getrandom(&mut secret);
        let id = hex(&idb);
        let short_code = short_code_from(&secret);
        let created_unix = now();
        let not_after_unix = created_unix + DEFAULT_TTL_SECS;
        g.push(Invite {
            id: id.clone(),
            secret,
            short_code: short_code.clone(),
            created_unix,
            not_after_unix,
            needs_approval,
            approved: false,
            used: false,
        });
        InviteView {
            id,
            secret_b64url: b64url(&secret),
            short_code,
            not_after_unix,
            needs_approval,
            approved: false,
            used: false,
        }
    }

    /// Invitations still on screen (for the dashboard).
    pub fn pending(&self) -> Vec<InviteView> {
        let g = self.inner.lock();
        g.iter()
            .filter(|i| i.live() || (i.used && now() < i.created_unix + 30))
            .map(|i| InviteView {
                id: i.id.clone(),
                secret_b64url: b64url(&i.secret),
                short_code: i.short_code.clone(),
                not_after_unix: i.not_after_unix,
                needs_approval: i.needs_approval,
                approved: i.approved,
                used: i.used,
            })
            .collect()
    }

    /// Operator clicked "Approve" for the first controller.
    pub fn approve(&self, id: &str) -> bool {
        let mut g = self.inner.lock();
        match g.iter_mut().find(|i| i.id == id && i.live()) {
            Some(i) => {
                i.approved = true;
                true
            }
            None => false,
        }
    }

    pub fn status(&self, id: &str) -> Option<InviteView> {
        let g = self.inner.lock();
        g.iter().find(|i| i.id == id).map(|i| InviteView {
            id: i.id.clone(),
            secret_b64url: String::new(),
            short_code: i.short_code.clone(),
            not_after_unix: i.not_after_unix,
            needs_approval: i.needs_approval,
            approved: i.approved,
            used: i.used,
        })
    }

    /// Consume the invitation identified by its `secret` (base64url, from the QR
    /// fragment) **or** its `short_code` (typed). Constant-time match, one-shot.
    pub fn consume(&self, secret_or_code: &str) -> Result<(), ConsumeError> {
        let token = secret_or_code.trim();
        let code = token.to_ascii_uppercase();
        let secret = b64url_decode(token);

        let mut g = self.inner.lock();
        let inv = g
            .iter_mut()
            .find(|i| {
                i.short_code == code
                    || secret
                        .map(|s| bool::from(s.ct_eq(&i.secret)))
                        .unwrap_or(false)
            })
            .ok_or(ConsumeError::NotFound)?;
        if inv.expired() {
            return Err(ConsumeError::Expired);
        }
        if inv.used {
            return Err(ConsumeError::AlreadyUsed);
        }
        if inv.needs_approval && !inv.approved {
            return Err(ConsumeError::NeedsApproval);
        }
        inv.used = true;
        Ok(())
    }
}

impl Default for InviteStore {
    fn default() -> Self {
        Self::new()
    }
}

fn short_code_from(secret: &[u8; 32]) -> String {
    // 9 base32 chars ≈ 44 bits, derived from the secret so it isn't a second
    // thing to store.
    let hk = Hkdf::<Sha256>::new(None, secret);
    let mut okm = [0u8; 9];
    hk.expand(b"inphase-pair-code", &mut okm).expect("9 bytes");
    okm.iter()
        .map(|b| B32[(*b as usize) % B32.len()] as char)
        .collect()
}

fn b64url(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn b64url_decode(s: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    let v = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()?;
    v.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_and_code_roundtrip() {
        let store = InviteStore::new();
        let v = store.create(false);
        // typed short code works
        assert!(store.consume(&v.short_code).is_ok());
        // and is one-shot
        assert!(matches!(
            store.consume(&v.short_code),
            Err(ConsumeError::AlreadyUsed)
        ));

        let v2 = store.create(false);
        assert!(store.consume(&v2.secret_b64url).is_ok());
    }

    #[test]
    fn approval_gate() {
        let store = InviteStore::new();
        let v = store.create(true);
        assert!(matches!(
            store.consume(&v.short_code),
            Err(ConsumeError::NeedsApproval)
        ));
        assert!(store.approve(&v.id));
        assert!(store.consume(&v.short_code).is_ok());
    }

    #[test]
    fn unknown_rejected() {
        let store = InviteStore::new();
        store.create(false);
        assert!(matches!(
            store.consume("NOTACODE1"),
            Err(ConsumeError::NotFound)
        ));
    }
}

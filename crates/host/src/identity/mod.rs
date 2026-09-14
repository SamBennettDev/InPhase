//! Host identity + the authoritative controller ACL.
//!
//! * [`HostIdentity`] — a long-term Ed25519 key generated on first run. The
//!   public half is the **Host ID**. At rest the 32-byte seed is DPAPI-wrapped
//!   (`CryptProtectData`, current-user scope) on Windows; a plain 0600-ish file
//!   is the dev fallback. A CNG/TPM-backed non-exportable key is the intended
//!   upgrade (TODO).
//! * [`acl::ControllerAcl`] — the list of browser controllers this Host trusts,
//!   persisted locally.
//!
//! This module only *holds* identity + trust state. Enforcing it in the
//! signaling handshake (the per-connect Ed25519 device challenge) lives in
//! [`crate::http::signal`].

pub mod acl;
pub mod pairing_invite;

use std::path::PathBuf;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;

use crate::config::Config;

/// On-disk identity container, DPAPI-wrapped as a whole.
/// `magic || version || ed25519_seed(32)`.
///
/// `FORMAT_V1` also carried a 32-byte X25519 "Noise static" seed (the abandoned
/// an earlier design); it is read and discarded, `V2` is written back.
const MAGIC: &[u8; 4] = b"INPH";
const FORMAT_V1_NOISE: u8 = 1;
const FORMAT_V2: u8 = 2;

pub struct HostIdentity {
    signing: SigningKey,
}

impl HostIdentity {
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_at(&key_path())
    }

    /// Testable variant with an explicit path.
    pub fn load_or_create_at(path: &std::path::Path) -> Result<Self> {
        if let Ok(blob) = std::fs::read(path) {
            if let Ok(plain) = unwrap_at_rest(&blob) {
                if let Some((id, seed_len)) = Self::parse(&plain) {
                    // Migrate a legacy V1 (had a trailing X25519 seed) file forward.
                    if seed_len != FORMATTED_V2_LEN {
                        id.persist(path)?;
                        tracing::info!("rewrote host identity file (dropped unused Noise static)");
                    }
                    return Ok(id);
                }
            }
            tracing::warn!("host identity file unreadable — regenerating");
        }

        let mut ed = [0u8; 32];
        getrandom::getrandom(&mut ed).context("OS RNG for host key")?;
        let id = Self::from_seed(ed);
        id.persist(path)?;
        tracing::info!(host_id = %id.host_id(), "generated host identity");
        Ok(id)
    }

    fn from_seed(mut ed: [u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&ed);
        ed.fill(0);
        Self { signing }
    }

    /// Returns the identity and the length of the parsed container so the caller
    /// can decide whether to rewrite it in the current format.
    fn parse(plain: &[u8]) -> Option<(Self, usize)> {
        // raw 32-byte Ed25519 seed (the earliest file)
        if plain.len() == 32 {
            let mut ed = [0u8; 32];
            ed.copy_from_slice(plain);
            return Some((Self::from_seed(ed), 32));
        }
        if plain.len() < 4 + 1 + 32 || &plain[..4] != MAGIC {
            return None;
        }
        let want = match plain[4] {
            FORMAT_V2 => 4 + 1 + 32,
            FORMAT_V1_NOISE => 4 + 1 + 32 + 32,
            _ => return None,
        };
        if plain.len() != want {
            return None;
        }
        let mut ed = [0u8; 32];
        ed.copy_from_slice(&plain[5..37]);
        Some((Self::from_seed(ed), plain.len()))
    }

    fn persist(&self, path: &std::path::Path) -> Result<()> {
        let mut plain = Vec::with_capacity(FORMATTED_V2_LEN);
        plain.extend_from_slice(MAGIC);
        plain.push(FORMAT_V2);
        plain.extend_from_slice(self.signing.to_bytes().as_slice());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let blob = wrap_at_rest(&plain)?;
        plain.fill(0);
        std::fs::write(path, &blob).with_context(|| format!("writing {}", path.display()))?;
        harden_perms(path);
        Ok(())
    }

    /// Lowercase hex of the 32-byte Ed25519 public key.
    pub fn host_id(&self) -> String {
        hex(self.signing.verifying_key().as_bytes())
    }
}

const FORMATTED_V2_LEN: usize = 4 + 1 + 32;

fn key_path() -> PathBuf {
    Config::config_dir().join("host-identity.key")
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    s.as_bytes().chunks_exact(2).map(|pair| {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        Some(((hi << 4) | lo) as u8)
    }).collect()
}

// ---- at-rest protection --------------------------------------------------

#[cfg(windows)]
pub(crate) fn wrap_at_rest(plain: &[u8]) -> Result<Vec<u8>> {
    dpapi::protect(plain).context("DPAPI protection failed; refusing to persist an unprotected key")
}

#[cfg(windows)]
pub(crate) fn unwrap_at_rest(blob: &[u8]) -> Result<Vec<u8>> {
    // Try DPAPI; if it fails the file may predate DPAPI wrapping — accept the
    // raw 32 bytes.
    match dpapi::unprotect(blob) {
        Ok(v) => Ok(v),
        Err(_) if blob.len() == 32 => Ok(blob.to_vec()),
        Err(e) => Err(e),
    }
}

#[cfg(not(windows))]
pub(crate) fn wrap_at_rest(plain: &[u8]) -> Result<Vec<u8>> {
    Ok(plain.to_vec())
}

#[cfg(not(windows))]
pub(crate) fn unwrap_at_rest(blob: &[u8]) -> Result<Vec<u8>> {
    Ok(blob.to_vec())
}

#[cfg(windows)]
fn harden_perms(_path: &std::path::Path) {
    // The %APPDATA% path is already per-user; DPAPI current-user scope is the
    // real protection. A restrictive DACL is a TODO alongside TPM.
}

#[cfg(not(windows))]
fn harden_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(windows)]
mod dpapi {
    use anyhow::{anyhow, Result};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as *mut _));
        v
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>> {
        unsafe {
            let mut out = CRYPT_INTEGER_BLOB::default();
            CryptProtectData(
                &blob(plain),
                windows::core::w!("InPhase host identity"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| anyhow!("CryptProtectData: {e}"))?;
            Ok(take(out))
        }
    }

    pub fn unprotect(blob_in: &[u8]) -> Result<Vec<u8>> {
        unsafe {
            let mut out = CRYPT_INTEGER_BLOB::default();
            CryptUnprotectData(
                &blob(blob_in),
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| anyhow!("CryptUnprotectData: {e}"))?;
            Ok(take(out))
        }
    }
}

#[cfg(test)]
mod hex_tests {
    #[test]
    fn malformed_unicode_hex_cannot_panic() {
        for value in ["aéa", "😀", "あa", "zz", "0"] {
            assert!(super::unhex(value).is_none(), "{value}");
        }
        assert_eq!(super::unhex("00aAFF"), Some(vec![0, 170, 255]));
    }
}

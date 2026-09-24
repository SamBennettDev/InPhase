//! ACME (Let's Encrypt) for the public HTTPS listener (ADR-0011 §3).
//!
//! A self-signed cert can never be fully trusted — that is the browser's
//! contract. The zero-prompt path is a publicly-trusted certificate, and
//! Let's Encrypt issues those only for domain names, never for bare IPs.
//! The bridge: sslip.io — a magic DNS domain where any hostname encoding an
//! IP (`2605-a601-800b-2100-0-0-0-100.sslip.io` → `2605:a601:800b:2100::100`)
//! resolves to that IP, no registration, no DNS config. That name is a
//! normal domain to Let's Encrypt, so the host can hold a real certificate
//! with HTTP-01/TLS-ALPN-01 validation and auto-renewal.
//!
//! Fail-safe by construction: the v6 listener always serves through
//! [`DualResolver`] — the ACME cert once one exists, the local-CA leaf
//! before that. A failed issuance degrades to exactly today's behaviour.

use anyhow::Context;
use futures::StreamExt;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Serves the ACME certificate once issued, the local-CA leaf before that.
/// rustls calls `resolve` per handshake; whichever layer answers first wins.
pub struct DualResolver {
    /// Filled when (and if) the ACME state machine starts for this boot.
    pub acme: Arc<OnceLock<Arc<rustls_acme::ResolvesServerCertAcme>>>,
    /// The local-CA leaf chain (pem on disk), the always-available fallback.
    pub local: Arc<CertifiedKey>,
    pub logged: AtomicBool,
}

impl std::fmt::Debug for DualResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DualResolver")
            .field("acme_ready", &self.acme.get().is_some())
            .finish()
    }
}

impl ResolvesServerCert for DualResolver {
    fn resolve(&self, hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        if let Some(acme) = self.acme.get() {
            if let Some(cert) = acme.resolve(hello) {
                if !self.logged.swap(true, Ordering::Relaxed) {
                    tracing::info!(
                        "acme certificate in use - the public hostname is now trusted by browsers"
                    );
                }
                return Some(cert);
            }
        }
        Some(self.local.clone())
    }
}

/// `2605:a601:800b:2100::100` → `2605-a601-800b-2100-0-0-0-100.sslip.io`.
/// sslip.io treats each dash-separated group as hex; the fully-expanded form
/// with explicit zero groups parses unambiguously.
pub fn sslip_hostname(ipv6: &str) -> Option<String> {
    let trimmed = ipv6.trim().trim_start_matches('[').trim_end_matches(']');
    // A SocketAddr string carries a port after the bracketed address.
    let addr = trimmed.rsplit_once(']').map_or(trimmed, |(a, _)| a);
    let addr: std::net::Ipv6Addr = addr.parse().ok()?;
    let name = addr
        .segments()
        .iter()
        .map(|s| format!("{s:x}"))
        .collect::<Vec<_>>()
        .join("-");
    Some(format!("{name}.sslip.io"))
}

/// The local-CA leaf, parsed once for the dual resolver's fallback arm.
pub fn load_local_certified_key(
    cert_pem: &Path,
    key_pem: &Path,
) -> anyhow::Result<Arc<CertifiedKey>> {
    let cert_pem = std::fs::read(cert_pem).context("reading the leaf cert pem")?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &*cert_pem)
        .collect::<Result<_, _>>()
        .context("parsing the leaf cert pem")?;
    let key_pem = std::fs::read(key_pem).context("reading the leaf key pem")?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &*key_pem)
        .transpose()
        .context("parsing the leaf key pem")?
        .context("leaf key pem has no key")?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key)
        .context("no signer for the leaf key")?;
    Ok(Arc::new(CertifiedKey::new(certs, signer)))
}

/// Start the ACME state machine for `hostname` (issuance + renewal forever)
/// and return its cert resolver — dropped into a [`DualResolver`] slot.
pub fn spawn_acme(hostname: String, cache_dir: &Path) -> Arc<rustls_acme::ResolvesServerCertAcme> {
    let config = rustls_acme::AcmeConfig::new([hostname.clone()])
        .cache(rustls_acme::caches::DirCache::new(cache_dir.to_path_buf()))
        .directory_lets_encrypt(true);
    let mut state = config.state();
    let resolver = state.resolver();
    tokio::spawn(async move {
        tracing::info!(%hostname, "acme: requesting a Let's Encrypt certificate (TLS-ALPN-01 on 443)");
        while let Some(ev) = state.next().await {
            match ev {
                Ok(ok) => tracing::debug!("acme: {ok:?}"),
                Err(err) => tracing::warn!("acme: {err:?}"),
            }
        }
    });
    resolver
}

/// Watches the portmap for the host's public IPv6 and starts ACME once it is
/// known. A configured `hostname` short-circuits the wait (started eagerly).
/// Runs for the process lifetime; single-shot by construction.
pub fn spawn_autostart(
    configured: String,
    cache_dir: PathBuf,
    slot: Arc<OnceLock<Arc<rustls_acme::ResolvesServerCertAcme>>>,
    remote: crate::portmap::Shared,
) {
    tokio::spawn(async move {
        if !configured.is_empty() {
            if slot
                .set(spawn_acme(configured.clone(), &cache_dir))
                .is_err()
            {
                return;
            }
            tracing::info!(%configured, "acme enabled for the configured hostname");
            return;
        }
        // Every second for the first two minutes, then every 30 s. The router
        // mapping lands ~6 s after boot, and until the ACME resolver is in the
        // slot the public listener serves the local-CA leaf - which a device
        // that only trusts the public chain refuses. A flat 30 s poll (whose
        // first tick was skipped) left every restart with a ~30 s window of
        // ERR_CERT_AUTHORITY_INVALID on the sslip.io URL (2026-09-24, Mac).
        // The check reads an in-memory value; polling it is free.
        let mut polls = 0u32;
        loop {
            let wait = if polls < 120 { 1 } else { 30 };
            polls = polls.saturating_add(1);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            if slot.get().is_some() {
                return;
            }
            let ext = remote.read().external.clone();
            let Some(ext) = ext else { continue };
            let Some(addr) = ext.split("]:").next() else {
                continue;
            };
            let addr = addr.trim_start_matches('[');
            let Some(host) = sslip_hostname(addr) else {
                tracing::debug!(%addr, "acme: external address is not IPv6 - sslip.io name unavailable");
                return;
            };
            if slot.set(spawn_acme(host.clone(), &cache_dir)).is_ok() {
                tracing::info!(
                    "public hostname: https://{host}/ - once the Let's Encrypt certificate lands, browsers trust this host with no prompts"
                );
            }
            return;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sslip_name_encodes_the_ipv6() {
        assert_eq!(
            sslip_hostname("2605:a601:800b:2100::100").as_deref(),
            Some("2605-a601-800b-2100-0-0-0-100.sslip.io")
        );
        // SocketAddr strings and brackets from the portmap status parse too.
        assert_eq!(
            sslip_hostname("[2605:a601:800b:2100::100]:4433").as_deref(),
            Some("2605-a601-800b-2100-0-0-0-100.sslip.io")
        );
        assert!(sslip_hostname("not an address").is_none());
    }
}

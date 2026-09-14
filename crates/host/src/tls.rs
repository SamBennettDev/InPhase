//! HTTPS for the host's own origin.
//!
//! A browser only gets a **secure context** — and therefore WebCrypto, Keyboard
//! Lock, the Gamepad API and the audio-output picker — when it loads the page
//! over HTTPS with a certificate it trusts.
//!
//! [`local_ca`] is how: the host generates its own CA once and a leaf cert for
//! `<machine>.local` + the LAN IPs. The installer adds the CA to the machine
//! current-user trust store (with consent); other devices install it once from
//! `GET /ca.crt`.

use std::path::PathBuf;
use std::time::Duration;

/// Paths to a PEM cert chain + private key on disk.
#[derive(Clone, Debug)]
pub struct CertPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Interval between local-CA leaf renewal checks.
pub const RENEW_INTERVAL: Duration = Duration::from_secs(12 * 3600);

/// The machine's short hostname, lowercased.
pub fn machine_hostname() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "inphase-host".into());
    raw.split('.').next().unwrap_or(&raw).to_ascii_lowercase()
}

// ===================================================================
// Local CA
// ===================================================================

pub mod local_ca {
    use std::path::Path;

    use anyhow::{Context, Result};
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose,
    };
    use time::{Duration, OffsetDateTime};

    use super::{machine_hostname, CertPaths};
    use crate::config::Config;

    const CA_CN: &str = "InPhase Local CA";
    const LEAF_DAYS: i64 = 395;
    const RENEW_WHEN_DAYS_LEFT: i64 = 60;

    /// Deterministic CA parameters — the DN must match the on-disk `ca.crt` so
    /// leaves chain to the copy other devices trust.
    fn ca_params() -> CertificateParams {
        let mut p = CertificateParams::new(Vec::<String>::new()).expect("no sans");
        let now = OffsetDateTime::now_utc();
        p.not_before = now - Duration::days(1);
        p.not_after = now + Duration::days(3650);
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(DnType::CommonName, CA_CN);
        p.distinguished_name
            .push(DnType::OrganizationName, "InPhase");
        p.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        p
    }

    /// Load the CA (`ca.crt` + DPAPI-wrapped `ca.key`) or create it.
    /// Returns `(ca_cert_pem, ca_key)`.
    pub fn load_or_create_ca(dir: &Path) -> Result<(String, KeyPair)> {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let crt = dir.join("ca.crt");
        let key = dir.join("ca.key");

        if crt.is_file() && key.is_file() {
            if let (Ok(blob), Ok(pem)) = (std::fs::read(&key), std::fs::read_to_string(&crt)) {
                let unwrapped =
                    crate::identity::unwrap_at_rest(&blob).unwrap_or_else(|_| blob.clone());
                if let Ok(kp) = KeyPair::from_pem(&String::from_utf8_lossy(&unwrapped)) {
                    return Ok((pem, kp));
                }
            }
            anyhow::bail!("local CA unreadable; restore its matching certificate and key instead of silently replacing device trust");
        }

        let kp = KeyPair::generate().context("generate CA key")?;
        let cert = ca_params().self_signed(&kp).context("self-sign CA")?;
        let pem = cert.pem();
        std::fs::write(&crt, &pem)?;
        std::fs::write(
            &key,
            crate::identity::wrap_at_rest(kp.serialize_pem().as_bytes())?,
        )?;
        harden(&key);
        tracing::info!(path = %crt.display(), "generated local CA");
        Ok((pem, kp))
    }

    /// Names/IPs the leaf cert should cover, and the preferred one for the URL.
    pub fn collect_sans(cfg: &Config) -> (String, Vec<String>) {
        // One endpoint description (review §8): identities come from the
        // EndpointPlan, not from an independent interface scan here.
        let plan = crate::endpoint::EndpointPlan::detect(
            cfg,
            machine_hostname(),
            crate::net::stable_global_ipv6(cfg),
        );
        let primary = cfg
            .tls
            .domain
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{}.local", plan.hostname));
        (primary, plan.certificate_identities(&cfg.tls.extra_sans))
    }

    /// Ensure a leaf cert (chained to the CA) covering `sans` exists on disk.
    /// Regenerates when missing, near expiry, or the SAN set changed.
    pub fn ensure_leaf(
        ca_pem: &str,
        ca_key: &KeyPair,
        primary: &str,
        sans: &[String],
        dir: &Path,
    ) -> Result<CertPaths> {
        let _ = ca_pem;
        let crt = dir.join("host.crt");
        let key = dir.join("host.key");
        let sans_file = dir.join("host.sans");
        let want = sans.join("\n");

        let current = crt.is_file()
            && key.is_file()
            && std::fs::read_to_string(&sans_file)
                .map(|s| s.trim() == want.trim())
                .unwrap_or(false)
            && days_left(&crt)
                .map(|d| d > RENEW_WHEN_DAYS_LEFT)
                .unwrap_or(false);
        if current {
            return Ok(CertPaths {
                cert: crt,
                key: key.clone(),
            });
        }

        let issue = || -> Result<()> {
            let ca_cert = ca_params()
                .self_signed(ca_key)
                .context("reconstruct CA for signing")?;
            let leaf_key = KeyPair::generate().context("generate leaf key")?;
            let mut lp = CertificateParams::new(sans.to_vec()).context("leaf SANs")?;
            let now = OffsetDateTime::now_utc();
            lp.not_before = now - Duration::hours(1);
            lp.not_after = now + Duration::days(LEAF_DAYS);
            lp.distinguished_name.push(DnType::CommonName, primary);
            lp.key_usages = vec![
                KeyUsagePurpose::DigitalSignature,
                KeyUsagePurpose::KeyEncipherment,
            ];
            lp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            lp.use_authority_key_identifier_extension = true;
            let leaf = lp
                .signed_by(&leaf_key, &ca_cert, ca_key)
                .context("sign leaf")?;

            std::fs::write(&crt, format!("{}{}", leaf.pem(), ca_pem))?;
            std::fs::write(&key, leaf_key.serialize_pem())?;
            harden(&key);
            std::fs::write(&sans_file, &want)?;
            Ok(())
        };

        match issue() {
            Ok(()) => {
                tracing::info!(cn = primary, sans = sans.len(), "issued local TLS leaf");
                Ok(CertPaths { cert: crt, key })
            }
            // A stale-but-usable cert on disk beats no HTTPS at all — this
            // happens when the LAN IP set changed (so the SANs differ) but the
            // cert dir was created by the elevated installer and the running
            // user cannot overwrite it. `inphase-host --trust-ca` (elevated)
            // refreshes it; the installer also grants the dir write access.
            Err(e) if crt.is_file() && key.is_file() => {
                tracing::warn!(
                    "could not reissue the local TLS leaf ({e:#}); serving the existing \
                     cert — its SAN list may be stale. Run `inphase-host --trust-ca` \
                     elevated to refresh it."
                );
                Ok(CertPaths { cert: crt, key })
            }
            Err(e) => Err(e),
        }
    }

    /// Explicit setup adds the CA to the current user's trust store. Normal
    /// startup checks trust without changing it or prompting in the background.
    pub async fn trust_ca(dir: &Path, force: bool) -> bool {
        let crt = dir.join("ca.crt");
        if !crt.is_file() {
            return false;
        }
        #[cfg(windows)]
        {
            use tokio::process::Command;
            use tokio::time::{timeout, Duration};

            async fn certutil(args: &[&str]) -> bool {
                let fut = Command::new("certutil").args(args).output();
                matches!(timeout(Duration::from_secs(3), fut).await, Ok(Ok(o)) if o.status.success())
            }
            async fn try_addstore(args: &[&str], crt: &std::path::Path) -> bool {
                let fut = Command::new("certutil").args(args).arg(crt).output();
                matches!(timeout(Duration::from_secs(3), fut).await, Ok(Ok(o)) if o.status.success())
            }

            // Per-user boot: if a Root store already carries our CA, leave it —
            // adding to Root needs elevation and would just log noise.
            if !force {
                return certutil(&["-user", "-store", "Root", "InPhase Local CA"]).await;
            }

            // --trust-ca runs as the user who will run the host.
            if try_addstore(&["-user", "-addstore", "-f", "Root"], &crt).await
            {
                tracing::info!("local CA trusted");
                return true;
            }
            tracing::info!(
                "the local CA is not in a trusted Root store — browsers on this PC \
                 will warn until the installer runs, or: certutil -addstore -f Root \"{}\" (elevated)",
                crt.display()
            );
            false
        }
        #[cfg(not(windows))]
        {
            let _ = (crt, force);
            false
        }
    }

    /// Crude PEM `notAfter` reader (avoids pulling an X.509 parser): counts days
    /// from the leaf's own file mtime + `LEAF_DAYS` as a lower bound.
    fn days_left(crt: &Path) -> Option<i64> {
        let modified = std::fs::metadata(crt).ok()?.modified().ok()?;
        let age = std::time::SystemTime::now()
            .duration_since(modified)
            .ok()?
            .as_secs() as i64
            / 86_400;
        Some(LEAF_DAYS - age)
    }

    #[cfg(windows)]
    fn harden(_p: &Path) {}
    #[cfg(not(windows))]
    fn harden(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
}

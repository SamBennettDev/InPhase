//! One description of the machine's serving endpoint (architecture review §8).
//!
//! Every subsystem that formats a URL, mints a certificate identity, or opens
//! a router mapping reads this plan instead of scanning interfaces itself.
//! Independent selectors were how the URL, the certificate SANs, and the PCP
//! mapping ended up disagreeing about which address this host lives at.

/// The endpoint this host serves from right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointPlan {
    /// Stable machine name (no domain) — `machine_hostname()`.
    pub hostname: String,
    /// The selected stable global IPv6 address, when the machine has one.
    pub ipv6: Option<std::net::Ipv6Addr>,
    /// Canonical origin host: the configured domain, else the IPv6 literal,
    /// else `<host>.local`.
    pub origin_host: String,
    /// TCP port the HTTPS page is served on.
    pub https_port: u16,
    /// UDP port the WebTransport endpoint listens on.
    pub wt_port: u16,
    /// Bumped whenever the selection changes; callbacks, tokens and mapping
    /// renewals carry it so stale endpoints can be recognised and dropped.
    pub generation: u64,
}

static LAST_SELECTION: std::sync::Mutex<Option<(String, u64)>> = std::sync::Mutex::new(None);
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl EndpointPlan {
    /// Build the plan from the one address selector (`net::stable_global_ipv6`)
    /// plus the machine name. Nothing else scans interfaces.
    pub fn detect(
        cfg: &crate::config::Config,
        hostname: String,
        ipv6: Option<std::net::Ipv6Addr>,
    ) -> Self {
        // Preference order is stability-first (review §8): a configured
        // domain, then the mDNS name - which survives ISP prefix rotation -
        // and only then the IPv6 literal, which dies with the prefix.
        let origin_host = match cfg.tls.domain.as_deref().filter(|s| !s.is_empty()) {
            Some(d) => d.to_string(),
            None => format!("{hostname}.local"),
        };
        let key = format!("{hostname}|{ipv6:?}|{}|{}", cfg.tls.port, cfg.media.wt_port);
        let generation = {
            let mut last = LAST_SELECTION.lock().expect("selection lock");
            match *last {
                Some((ref k, g)) if *k == key => g,
                _ => {
                    let g = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    *last = Some((key, g));
                    g
                }
            }
        };
        Self {
            hostname,
            ipv6,
            origin_host,
            https_port: cfg.tls.port,
            wt_port: cfg.media.wt_port,
            generation,
        }
    }

    /// The canonical HTTPS origin users and clients dial.
    pub fn canonical_origin(&self) -> String {
        Self::https_origin(&self.origin_host, self.https_port)
    }

    /// Address shown for pairing / "connect another device".
    ///
    /// A configured domain wins. Otherwise the sslip.io name of the stable
    /// global IPv6 (or a configured ACME hostname) so a phone can open a
    /// publicly-trusted URL instead of `https://<pc>.local`.
    pub fn advertised_origin(&self, acme_hostname: &str) -> String {
        let host = if !self.origin_host.ends_with(".local") && self.origin_host != self.hostname {
            self.origin_host.clone()
        } else if !acme_hostname.is_empty() {
            acme_hostname.to_string()
        } else if let Some(v6) = self.ipv6 {
            crate::acme::sslip_hostname(&v6.to_string()).unwrap_or_else(|| self.origin_host.clone())
        } else {
            self.origin_host.clone()
        };
        Self::https_origin(&host, self.https_port)
    }

    fn https_origin(host: &str, port: u16) -> String {
        if port == 443 {
            format!("https://{host}")
        } else {
            format!("https://{host}:{port}")
        }
    }

    /// Every identity the leaf certificate must cover: the canonical origin,
    /// the stable hostname forms, loopback, and the selected IPv6 literal.
    pub fn certificate_identities(&self, extra: &[String]) -> Vec<String> {
        let mut ids = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
            self.hostname.clone(),
            format!("{}.local", self.hostname),
        ];
        if let Some(v6) = self.ipv6 {
            ids.push(v6.to_string());
        }
        if let Some(stripped) = self.origin_host.strip_prefix('[') {
            if let Some(v6) = stripped.strip_suffix(']') {
                ids.push(v6.to_string());
            }
        } else {
            ids.push(self.origin_host.clone());
        }
        ids.extend(extra.iter().cloned());
        ids.sort();
        ids.dedup();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> crate::config::Config {
        crate::config::Config::default()
    }

    #[test]
    fn mdns_origin_survives_prefix_rotation_and_v6_stays_certified() {
        let mut c = cfg();
        c.tls.domain = None;
        let p = EndpointPlan::detect(
            &c,
            "gaming-pc".into(),
            Some("2001:db8:1234:5678::100".parse().unwrap()),
        );
        // The mDNS name is canonical: the IPv6 prefix rotates, the name does
        // not (review §8). The literal remains a certificate identity.
        assert_eq!(p.canonical_origin(), "https://gaming-pc.local");
        assert_eq!(
            p.advertised_origin(""),
            "https://2001-db8-1234-5678-0-0-0-100.sslip.io"
        );
        assert_eq!(
            p.advertised_origin("custom.sslip.io"),
            "https://custom.sslip.io"
        );
        let ids = p.certificate_identities(&[]);
        assert!(ids.contains(&"2001:db8:1234:5678::100".to_string()));
        assert!(ids.contains(&"gaming-pc.local".to_string()));
    }

    #[test]
    fn domain_wins_and_nonstandard_port_is_shown() {
        let mut c = cfg();
        c.tls.domain = Some("stream.example.com".into());
        c.tls.port = 8443;
        let p = EndpointPlan::detect(&c, "gaming-pc".into(), None);
        assert_eq!(p.canonical_origin(), "https://stream.example.com:8443");
        assert_eq!(
            p.advertised_origin("ignored.sslip.io"),
            "https://stream.example.com:8443"
        );
        assert!(p
            .certificate_identities(&[])
            .contains(&"stream.example.com".to_string()));
    }

    #[test]
    fn generation_is_stable_while_the_selection_holds() {
        let c = cfg();
        let v6: std::net::Ipv6Addr = "2001:db8:1234:5678::100".parse().unwrap();
        let a = EndpointPlan::detect(&c, "gaming-pc".into(), Some(v6));
        let b = EndpointPlan::detect(&c, "gaming-pc".into(), Some(v6));
        assert_eq!(
            a.generation, b.generation,
            "same selection, same generation"
        );
        let c2 = EndpointPlan::detect(
            &c,
            "gaming-pc".into(),
            Some("2001:db8:1234:5678::200".parse().unwrap()),
        );
        assert_ne!(
            a.generation, c2.generation,
            "address change bumps the generation"
        );
    }
}

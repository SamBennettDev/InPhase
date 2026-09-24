//! Windows Firewall rules for the LAN/tailnet server (§8.1, Phase 4).
//!
//! Uses `netsh advfirewall` — one shell-out at startup, no COM surface, easy for
//! a user to inspect/remove. The exe manages its own rules so a bare `.exe` run
//! (no installer) works. All rules are **remote-address-scoped** to
//! `allowed_remote_cidrs` (LocalSubnet + the Tailscale CGNAT range by default),
//! so `profile=any` is safe — even on a "Public" network nothing outside those
//! ranges can reach the host.
//!
//! * **`InPhase App`** — program-scoped allow for this exe (any port), so
//!   Windows never shows the "allow network access?" prompt.
//! * **`InPhase <port>`** — an explicit TCP port rule per listening port
//!   (`http_port`, `tls.port`), so the ports are visible in the Firewall UI.
//!
//! Adding a rule needs elevation. The installer does it once (`--setup-firewall`
//! elevated); the per-user host only re-adds a rule that is actually missing, so
//! a normal sign-in launch neither prompts nor logs a spurious failure.

use tracing::{debug, info};

const APP_RULE: &str = "InPhase App";

/// `recreate` = true when we know we can write firewall rules (the elevated
/// installer step, or the tray toggle changing the remote scope). At a normal
/// per-user startup it is false: existing rules are left alone.
pub fn ensure_firewall_rule(
    ports: &[u16],
    allowed_remote: &[String],
    recreate: bool,
) -> anyhow::Result<()> {
    let remote = if allowed_remote.is_empty() {
        "localsubnet".to_string()
    } else {
        allowed_remote.join(",")
    };

    let mut failures: Vec<String> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        let exe = exe.to_string_lossy().to_string();
        if let Err(e) = add_rule(
            APP_RULE,
            &[
                "dir=in",
                "action=allow",
                &format!("program={exe}"),
                "enable=yes",
                "profile=any",
                &format!("remoteip={remote}"),
            ],
            recreate,
        ) {
            failures.push(format!("program rule: {e:#}"));
        }
    }

    for &port in ports {
        // Both transports get an explicit rule: TCP for the page, UDP for
        // WebTransport video. Relying on the program-scoped rule to carry
        // UDP meant a missing/expired program rule silently killed video
        // while the page still loaded (review §9).
        for (proto, tag) in [("TCP", "tcp"), ("UDP", "udp")] {
            let name = format!("InPhase {port} {tag}");
            if let Err(e) = add_rule(
                &name,
                &[
                    "dir=in",
                    "action=allow",
                    &format!("protocol={proto}"),
                    &format!("localport={port}"),
                    "profile=any",
                    &format!("remoteip={remote}"),
                ],
                recreate,
            ) {
                failures.push(format!("{name}: {e:#}"));
            }
        }
    }

    // Tidy the pre-1.0 rule name if it lingers.
    if recreate {
        netsh_delete("InPhase LAN");
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "firewall rules not installed: {}",
            failures.join("; ")
        ))
    }
}

fn add_rule(name: &str, args: &[&str], recreate: bool) -> anyhow::Result<()> {
    if !recreate && rule_exists(name) {
        debug!(rule = name, "firewall rule already present");
        return Ok(());
    }
    netsh_delete(name);
    let mut cmd = crate::proc::command("netsh");
    cmd.args([
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &format!("name={name}"),
    ]);
    cmd.args(args);
    match cmd.status() {
        Ok(s) if s.success() => {
            info!(rule = name, "firewall rule installed");
            Ok(())
        }
        // Needs elevation but the rule is already installed and correct -
        // that is success for this purpose (the installer ran elevated).
        _ if rule_exists(name) => {
            debug!(rule = name, "already installed (kept; add needs elevation)");
            Ok(())
        }
        other => {
            let msg = format!(
                "netsh failed ({other:?}) - the host may be unreachable until \
                 the rule is added by hand or the installer is re-run"
            );
            tracing::warn!(rule = name, "{msg}");
            Err(anyhow::anyhow!(msg))
        }
    }
}

fn rule_exists(name: &str) -> bool {
    crate::proc::command("netsh")
        .args([
            "advfirewall",
            "firewall",
            "show",
            "rule",
            &format!("name={name}"),
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Rule Name"))
        .unwrap_or(false)
}

fn netsh_delete(name: &str) {
    let _ = crate::proc::command("netsh")
        .args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={name}"),
        ])
        .output();
}

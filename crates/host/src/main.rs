// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett
//
// The host is a windowless GUI-subsystem binary: it lives in the system tray,
// has no console, and needs no launcher script. CLI subcommands (`--doctor`
// etc.) attach to the parent terminal — see `attach_console_for_cli`.
#![cfg_attr(windows, windows_subsystem = "windows")]

//! InPhase host entry point (architecture report §3.1: *"Run InPhase as a normal
//! per-user application launched at sign-in, not as a Windows service."*).

use std::io::Write;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use inphase_host::{config::Config, HostRuntime};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Windowless subsystem → CLI subcommands would have nowhere to print.
    // Borrow the parent terminal (or open one) when the user asked for output.
    attach_console_for_cli(&args);
    // Must run before gst::init() anywhere downstream: point GStreamer at the
    // private runtime bundled next to the exe, if there is one.
    point_at_bundled_runtime();
    init_tracing();
    // Panic messages must reach host.log: the host runs windowless with no
    // stderr to print to, so without this a task panic kills the process
    // silently (seen live: a WT session death took the whole host down).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string());
        tracing::error!("PANIC at {loc}: {msg}");
        default_hook(info);
    }));
    inphase_host::platform::ignore_stray_ctrl_c();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("inphase-host {}", inphase_host::HOST_VERSION);
        return Ok(());
    }
    if args.iter().any(|a| a == "--print-config") {
        println!("{}", Config::default().to_toml());
        return Ok(());
    }
    // Support / bring-up: what does the OS report as this host's route out, and
    // can the router open a port on it?
    if args.iter().any(|a| a == "--net-probe") {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(inphase_host::portmap::print_probe());
    }
    // Installer step (run elevated): add the inbound Windows Firewall rules so a
    // background sign-in launch doesn't depend on a user approving a prompt.
    if args.iter().any(|a| a == "--setup-firewall") {
        let cfg = Config::load().unwrap_or_default();
        return match inphase_host::app::apply_firewall_rules(&cfg, true) {
            Ok(()) => {
                println!("InPhase firewall rules installed");
                Ok(())
            }
            Err(e) => Err(e),
        };
    }
    // Installer / support preflight. Uses the on-disk
    // config if present so it checks the real ports, else defaults.
    if args.iter().any(|a| a == "--doctor") {
        let cfg = Config::load().unwrap_or_default();
        let json = args.iter().any(|a| a == "--json");
        std::process::exit(inphase_host::doctor::run(&cfg, json));
    }
    // Installer step (run elevated): create the local CA + leaf cert and add the
    // CA to the machine trust store, so browsers on this PC trust the host's
    // HTTPS with no warning. Idempotent.
    if args.iter().any(|a| a == "--trust-ca") {
        let cfg = Config::load().unwrap_or_default();
        let dir = cfg.tls.cert_dir_path();
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(async move {
            let (ca_pem, ca_key) = inphase_host::tls::local_ca::load_or_create_ca(&dir)?;
            let (primary, sans) = inphase_host::tls::local_ca::collect_sans(&cfg);
            inphase_host::tls::local_ca::ensure_leaf(&ca_pem, &ca_key, &primary, &sans, &dir)?;
            let ok = inphase_host::tls::local_ca::trust_ca(&dir, true).await;
            println!(
                "local CA at {}\ntrust store: {}",
                dir.join("ca.crt").display(),
                if ok {
                    "installed"
                } else {
                    "NOT installed — run this elevated"
                }
            );
            Ok(())
        });
    }
    if let Some(i) = args.iter().position(|a| a == "--support-bundle") {
        let cfg = Config::load().unwrap_or_default();
        let path = args
            .get(i + 1)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("inphase-support.json"));
        inphase_host::doctor::write_support_bundle(&cfg, &path)
            .with_context(|| format!("writing support bundle to {}", path.display()))?;
        println!("wrote {}", path.display());
        return Ok(());
    }

    let cfg = Config::load().context("loading configuration")?;
    // A restart is the single most common explanation for "it stopped working",
    // so make it unmissable in host.log and carry the build id: matching this
    // against a client's build_id is what separates "the host restarted" from
    // "the page is stale" without any further archaeology.
    tracing::info!(
        version = inphase_host::HOST_VERSION,
        build_id = inphase_host::BUILD_ID,
        pid = std::process::id(),
        path = %Config::config_path().display(),
        "=== InPhase host starting ==="
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let host = HostRuntime::boot(cfg).context("host boot")?;
        host.run().await
    })
}

/// The GUI-subsystem binary has no console. When a CLI subcommand is present,
/// attach to the terminal that launched us (or open one for a shortcut) so
/// `println!` from `--doctor`, `--version`, `--net-probe` etc. is visible.
#[cfg(windows)]
fn attach_console_for_cli(args: &[String]) {
    const CLI_FLAGS: &[&str] = &[
        "--version",
        "-V",
        "--print-config",
        "--net-probe",
        "--doctor",
        "--trust-ca",
        "--setup-firewall",
        "--support-bundle",
    ];
    if !args.iter().any(|a| CLI_FLAGS.contains(&a.as_str())) {
        return;
    }
    use windows::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            let _ = AllocConsole();
        }
    }
    // Rust's stdio on Windows resolves `GetStdHandle` per write, so once the
    // console is attached, `println!` reaches it with no further plumbing.
}

#[cfg(not(windows))]
fn attach_console_for_cli(_args: &[String]) {}

fn init_tracing() {
    // §26: never log PINs, cookies, SDP secrets or raw input events. The code
    // paths that touch those log only metadata; this just sets levels/format.
    let filter = EnvFilter::try_from_env("INPHASE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,inphase_host=debug"));

    // A file layer so the log survives without a console to redirect. The host
    // runs windowless from the sign-in launcher; there is no stdout to capture.
    let file_layer = log_file().map(|f| {
        let w = Arc::new(Mutex::new(f));
        fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .compact()
            .with_writer(move || LockWriter(w.clone()))
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).compact())
        .with(file_layer)
        .init();
}

/// `%LOCALAPPDATA%\InPhase\host.log`, opened for append. Rolled once it passes
/// ~8 MB so it cannot grow without bound.
fn log_file() -> Option<std::fs::File> {
    let dir = dirs_local_appdata()?.join("InPhase");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("host.log");
    if std::fs::metadata(&path)
        .map(|m| m.len() > 8_000_000)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, dir.join("host.log.1"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

#[cfg(windows)]
fn dirs_local_appdata() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
}
#[cfg(not(windows))]
fn dirs_local_appdata() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
}

/// `MakeWriter` closure target: locks the shared file for each write.
struct LockWriter(Arc<Mutex<std::fs::File>>);
impl Write for LockWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().map(|mut f| f.write(b)).unwrap_or(Ok(0))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().map(|mut f| f.flush()).unwrap_or(Ok(()))
    }
}

/// If a `runtime\gstreamer` tree sits next to the exe (the installed bundle),
/// point GStreamer at it and nothing else. A dev build has no such directory,
/// so this is a no-op and the system GStreamer is used as before.
fn point_at_bundled_runtime() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(root) = exe.parent().map(|p| p.join("runtime").join("gstreamer")) else {
        return;
    };
    if !root.join("lib").join("gstreamer-1.0").is_dir() {
        return;
    }
    std::env::set_var("GST_PLUGIN_PATH", root.join("lib").join("gstreamer-1.0"));
    std::env::set_var("GST_PLUGIN_SYSTEM_PATH", "");
    if let Some(local) = dirs_local_appdata() {
        std::env::set_var(
            "GST_REGISTRY",
            local.join("InPhase").join("gst-registry.bin"),
        );
    }
    // Prepend the bundled bin dir so the loader finds the private DLLs first.
    let bin = root.join("bin");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut parts = vec![bin];
    parts.extend(std::env::split_paths(&path));
    if let Ok(joined) = std::env::join_paths(parts) {
        std::env::set_var("PATH", joined);
    }
}

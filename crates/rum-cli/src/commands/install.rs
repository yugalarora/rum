//! `rum install [-y] <packages...>`
//!
//! Resolves the dependency closure, downloads the RPMs (checksum-verified),
//! shows the transaction, and commits it. When rum runs as root it commits
//! natively through librpm (`rpmtsRun`) via [`rum_rpm::Transaction`]; otherwise
//! (or when `RUM_USE_RPM_BINARY=1`) it shells out to `sudo rpm -U`. Either way
//! rpm performs its own dependency/conflict check and ordering before writing
//! the rpmdb, so a resolver mistake fails cleanly rather than corrupting the
//! system.

use std::path::PathBuf;

use super::{confirm, download};
use crate::sys;
use rum_rpm::Transaction;

pub fn run(packages: &[String], assume_yes: bool) -> anyhow::Result<()> {
    if packages.is_empty() {
        anyhow::bail!("`rum install` needs at least one package name");
    }

    // Resolve first (no download yet), so we can show the transaction and
    // confirm before fetching anything — matching dnf's order.
    let resolution = download::resolve_packages(packages, true)?;
    if resolution.is_empty() {
        println!("Nothing to do.");
        return Ok(());
    }

    let nevras = resolution.nevras();
    println!("\nInstalling {} package(s):", nevras.len());
    for n in &nevras {
        println!("  {n}");
    }
    println!("\nTotal download size: {}", human(resolution.total_bytes()));

    if !assume_yes && !confirm() {
        println!("Operation cancelled.");
        return Ok(());
    }

    // The resolve just peaked heap usage building the pool; hand freed pages
    // back to the OS so the native rpm transaction has headroom on small hosts.
    sys::release_free_memory();

    // Download into rum's package cache, then commit.
    let pkgdir = sys::effective_cachedir("/var/cache/rum").join("packages");
    let fetched = download::fetch(&resolution, &pkgdir)?;
    commit_install(&fetched.files)
}

fn commit_install(files: &[PathBuf]) -> anyhow::Result<()> {
    let use_binary = std::env::var_os("RUM_USE_RPM_BINARY").is_some();
    if sys::is_root() && !use_binary {
        commit_install_native(files)
    } else {
        commit_install_rpm_binary(files)
    }
}

/// Commit natively through librpm (`rpmtsRun`), no subprocess. Requires root.
fn commit_install_native(files: &[PathBuf]) -> anyhow::Result<()> {
    let mut tx =
        Transaction::new().map_err(|e| anyhow::anyhow!("cannot start transaction: {e}"))?;
    for f in files {
        tx.add_install(f).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    tx.run(false).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("Complete!");
    Ok(())
}

/// Fallback path: `sudo rpm -Uvh` (used when rum is not root, or when
/// `RUM_USE_RPM_BINARY` is set). `-U` installs new and upgrades existing, in one
/// ordered transaction with scriptlets; `-h` shows a progress hash.
fn commit_install_rpm_binary(files: &[PathBuf]) -> anyhow::Result<()> {
    let mut cmd = sys::privileged("rpm");
    cmd.arg("-Uvh");
    for f in files {
        cmd.arg(f);
    }
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch rpm: {e}"))?;

    if status.success() {
        println!("Complete!");
        Ok(())
    } else {
        anyhow::bail!("rpm transaction failed (exit {:?})", status.code())
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

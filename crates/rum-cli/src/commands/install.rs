//! `rum install [-y] <packages...>`
//!
//! Resolves the dependency closure, downloads the RPMs (checksum-verified),
//! shows the transaction, and commits it via the system `rpm` (which is
//! librpm) in a single transaction. rpm performs its own dependency/conflict
//! check (`rpmtsCheck`) and ordering before writing the rpmdb, so a resolver
//! mistake fails cleanly rather than corrupting the system.
//!
//! (A native librpm `rpmtsRun` FFI path is a planned hardening follow-up; the
//! rpm binary is used here for a battle-tested transaction engine.)

use std::path::PathBuf;

use super::{confirm, download};
use crate::sys;

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

    // Download into rum's package cache, then commit.
    let pkgdir = sys::effective_cachedir("/var/cache/rum").join("packages");
    let fetched = download::fetch(&resolution, &pkgdir)?;
    commit_install(&fetched.files)
}

fn commit_install(files: &[PathBuf]) -> anyhow::Result<()> {
    // `rpm -U` installs new packages and upgrades existing ones, in one
    // ordered transaction with scriptlets. `-h` shows a progress hash.
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

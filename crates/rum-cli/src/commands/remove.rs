//! `rum remove [-y] <packages...>`
//!
//! Erases packages via the system `rpm` (librpm). rpm refuses to remove a
//! package that others still depend on (unless forced), so this is safe by
//! default. We show what will be removed, confirm, then run `rpm -e`.

use super::confirm;
use crate::sys;
use rum_rpm::Rpmdb;

pub fn run(packages: &[String], assume_yes: bool) -> anyhow::Result<()> {
    if packages.is_empty() {
        anyhow::bail!("`rum remove` needs at least one package name");
    }

    // Confirm the named packages are actually installed, and show NEVRAs.
    let db = Rpmdb::open().map_err(|e| anyhow::anyhow!("cannot open rpmdb: {e}"))?;
    let mut matched = Vec::new();
    let mut missing = Vec::new();
    for spec in packages {
        let found = db.by_name(spec);
        if found.is_empty() {
            missing.push(spec.clone());
        } else {
            for p in found {
                matched.push(p.nevra());
            }
        }
    }
    if !missing.is_empty() {
        anyhow::bail!("not installed: {}", missing.join(", "));
    }

    println!("Removing {} package(s):", matched.len());
    matched.sort();
    for n in &matched {
        println!("  {n}");
    }

    if !assume_yes && !confirm() {
        println!("Operation cancelled.");
        return Ok(());
    }

    let mut cmd = sys::privileged("rpm");
    cmd.arg("-e");
    for p in packages {
        cmd.arg(p);
    }
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch rpm: {e}"))?;

    if status.success() {
        println!("Complete!");
        Ok(())
    } else {
        anyhow::bail!("rpm removal failed (exit {:?})", status.code())
    }
}

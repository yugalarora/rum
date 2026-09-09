//! System integration glue: build the variable set and resolve the cache
//! directory the way dnf/yum do, using the rpmdb where needed.

use std::path::{Path, PathBuf};

use rum_config::{Config, Paths, Vars};
use rum_rpm::Rpmdb;

/// Load system config with a dnf-accurate `$releasever`.
///
/// dnf derives `$releasever` from the version of the rpmdb Provides
/// `system-release(releasever)` (its `distroverpkg`), NOT from /etc/os-release.
/// On Amazon Linux 2023 these differ (`2023` vs `2023.12.20260831`), and the
/// os-release value yields 403s against the versioned mirror paths. We query
/// the rpmdb and fall back to whatever `Vars::detect` found otherwise.
pub fn load_config() -> anyhow::Result<Config> {
    let mut vars = Vars::detect();

    if let Ok(db) = Rpmdb::open() {
        if let Some(rv) = db.releasever() {
            let (major, minor) = match rv.split_once('.') {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (rv.clone(), String::new()),
            };
            vars.insert("releasever", rv);
            vars.insert("releasever_major", major);
            vars.insert("releasever_minor", minor);
        }
    }

    Config::load_with(&Paths::default(), vars)
        .map_err(|e| anyhow::anyhow!("failed to load configuration: {e}"))
}

/// The cache directory to actually use.
///
/// Only root can write the shared system cache (default `/var/cache/rum`).
/// When invoked as a normal user (e.g. read-only queries), fall back to a
/// per-user cache under `$XDG_CACHE_HOME`/`~/.cache`, mirroring how dnf uses a
/// user cache when it can't write the system one.
pub fn effective_cachedir(configured: &str) -> PathBuf {
    if is_root() {
        return PathBuf::from(configured);
    }
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return Path::new(&x).join("rum");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Path::new(&home).join(".cache").join("rum");
        }
    }
    std::env::temp_dir().join("rum")
}

#[cfg(unix)]
fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid is always safe and never fails.
    unsafe { geteuid() == 0 }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

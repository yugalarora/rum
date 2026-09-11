//! System integration glue: build the variable set and resolve the cache
//! directory the way dnf/yum do, using the rpmdb where needed.

use std::path::{Path, PathBuf};

use rum_config::{Config, Paths, RepoSource, Vars};
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

    let mut config = Config::load_with(&Paths::default(), vars)
        .map_err(|e| anyhow::anyhow!("failed to load configuration: {e}"))?;
    substitute_region(&mut config);
    Ok(config)
}

/// Red Hat RHUI repo URLs contain the literal token `REGION`, which the AWS
/// `amazon-id` dnf plugin rewrites to the instance's region. Replicate that:
/// if any repo references `REGION`, detect the region via IMDS and substitute.
fn substitute_region(config: &mut Config) {
    let uses_region = config.repos.iter().any(|r| match &r.source {
        RepoSource::BaseUrls(urls) => urls.iter().any(|u| u.contains("REGION")),
        RepoSource::MirrorList(u) | RepoSource::MetaLink(u) => u.contains("REGION"),
    });
    if !uses_region {
        return;
    }
    let Some(region) = rum_repo::detect_aws_region() else {
        // Leave URLs as-is; the repo will fail with a clear DNS error rather
        // than silently using a wrong endpoint.
        return;
    };
    for repo in &mut config.repos {
        repo.source = match &repo.source {
            RepoSource::BaseUrls(urls) => {
                RepoSource::BaseUrls(urls.iter().map(|u| u.replace("REGION", &region)).collect())
            }
            RepoSource::MirrorList(u) => RepoSource::MirrorList(u.replace("REGION", &region)),
            RepoSource::MetaLink(u) => RepoSource::MetaLink(u.replace("REGION", &region)),
        };
    }
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
pub fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid is always safe and never fails.
    unsafe { geteuid() == 0 }
}

#[cfg(not(unix))]
pub fn is_root() -> bool {
    false
}

/// Return freed heap pages to the OS. glibc's allocator keeps large freed
/// arenas resident, so after the memory-heavy resolve the RSS stays high; on a
/// constrained host that leaves too little headroom for the native rpm
/// transaction (which mmaps/unpacks payloads). Calling this between resolve and
/// commit lets the transaction start from a low resident set. No-op off glibc.
#[cfg(all(unix, target_env = "gnu"))]
pub fn release_free_memory() {
    extern "C" {
        fn malloc_trim(pad: usize) -> std::os::raw::c_int;
    }
    // SAFETY: malloc_trim is always safe to call; it only releases free memory.
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(all(unix, target_env = "gnu")))]
pub fn release_free_memory() {}

/// Build a command that runs `prog`, escalating via sudo when not already root
/// (state-changing rpm operations need write access to the rpmdb).
pub fn privileged(prog: &str) -> std::process::Command {
    if is_root() {
        std::process::Command::new(prog)
    } else {
        let mut c = std::process::Command::new("sudo");
        c.arg(prog);
        c
    }
}

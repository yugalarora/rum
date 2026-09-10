//! Shared helper: load config and sync all enabled repos into one package list.

use std::collections::HashMap;
use std::time::Instant;

use rum_repo::{AvailablePackage, Http, RepoMetadata, SyncOptions};

use crate::sys;

/// Result of syncing every enabled repo.
pub struct Synced {
    /// Parsed metadata per successful repo. Packages are held rkyv-archived;
    /// read them zero-copy via [`Synced::metas`] (query commands) or
    /// materialize owned packages via [`Synced::owned_packages`] (resolve).
    metas: Vec<RepoMetadata>,
    /// Per-repo (id, count, from_cache) for reporting.
    pub repos: Vec<RepoStat>,
    /// repo id -> resolved base URL (for building package download URLs).
    pub base_urls: HashMap<String, String>,
    /// repo id -> HTTP client honouring that repo's TLS settings (for
    /// downloading packages, e.g. from mutual-TLS RHUI repos).
    pub clients: HashMap<String, Http>,
    pub elapsed: std::time::Duration,
}

impl Synced {
    /// Zero-copy view of every synced repo's archived packages.
    pub fn metas(&self) -> &[RepoMetadata] {
        &self.metas
    }

    /// Materialize all packages, aggregated across repos, as owned values.
    /// Used by the resolve/download path (which indexes and mutates them).
    pub fn owned_packages(&self) -> Vec<AvailablePackage> {
        let mut v = Vec::new();
        for m in &self.metas {
            v.extend(m.to_owned_packages());
        }
        v
    }
}

pub struct RepoStat {
    pub id: String,
    pub count: usize,
    pub from_cache: bool,
    pub error: Option<String>,
}

/// Load system config and sync all enabled repos in parallel.
///
/// `force_refresh` ignores cache freshness. Repos that fail are reported in
/// `RepoStat.error` rather than aborting the whole operation.
pub fn sync_enabled(force_refresh: bool) -> anyhow::Result<Synced> {
    let config = sys::load_config()?;

    let enabled = config.enabled_repos();

    // Per-repo HTTP clients (carry TLS client certs for mutual-TLS repos).
    let mut clients = HashMap::new();
    for r in &enabled {
        if let Ok(http) = Http::for_repo(r) {
            clients.insert(r.id.clone(), http);
        }
    }

    let opts = SyncOptions {
        cachedir: sys::effective_cachedir(&config.main.cachedir),
        force_refresh,
    };

    let start = Instant::now();
    let results = rum_repo::sync_all(&enabled, &opts);
    let elapsed = start.elapsed();

    let mut metas = Vec::new();
    let mut repos = Vec::new();
    let mut base_urls = HashMap::new();
    for (id, res) in results {
        match res {
            Ok(md) => {
                base_urls.insert(id.clone(), md.base_url.clone());
                repos.push(RepoStat {
                    id,
                    count: md.len(),
                    from_cache: md.from_cache,
                    error: None,
                });
                metas.push(md);
            }
            Err(e) => {
                tracing::warn!(repo = %id, "sync failed: {e}");
                repos.push(RepoStat {
                    id,
                    count: 0,
                    from_cache: false,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    // config is loaded and used to drive the sync; not returned yet (install
    // will need it in a later milestone).
    let _ = config;

    Ok(Synced {
        metas,
        repos,
        base_urls,
        clients,
        elapsed,
    })
}

/// Lazily load file ownership from every enabled repo's filelists.xml, filtered
/// to `wanted` paths. Used as a fallback when a resolve fails on a file-based
/// dependency whose path is not in primary.xml. Returns `(pkgid, files)`.
pub fn load_filelists(wanted: &std::collections::HashSet<String>) -> Vec<(String, Vec<String>)> {
    let Ok(config) = sys::load_config() else {
        return Vec::new();
    };
    let opts = SyncOptions {
        cachedir: sys::effective_cachedir(&config.main.cachedir),
        force_refresh: false,
    };
    let mut out = Vec::new();
    for r in config.enabled_repos() {
        match rum_repo::load_filelists(r, &opts, wanted) {
            Ok(mut entries) => out.append(&mut entries),
            Err(e) => tracing::warn!(repo = %r.id, "filelists load failed: {e}"),
        }
    }
    out
}

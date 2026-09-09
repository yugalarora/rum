//! Shared helper: load config and sync all enabled repos into one package list.

use std::collections::HashMap;
use std::time::Instant;

use rum_repo::{AvailablePackage, Http, SyncOptions};

use crate::sys;

/// Result of syncing every enabled repo.
pub struct Synced {
    pub packages: Vec<AvailablePackage>,
    /// Per-repo (id, count, from_cache) for reporting.
    pub repos: Vec<RepoStat>,
    /// repo id -> resolved base URL (for building package download URLs).
    pub base_urls: HashMap<String, String>,
    /// repo id -> HTTP client honouring that repo's TLS settings (for
    /// downloading packages, e.g. from mutual-TLS RHUI repos).
    pub clients: HashMap<String, Http>,
    pub elapsed: std::time::Duration,
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

    let mut packages = Vec::new();
    let mut repos = Vec::new();
    let mut base_urls = HashMap::new();
    for (id, res) in results {
        match res {
            Ok(md) => {
                base_urls.insert(id.clone(), md.base_url);
                repos.push(RepoStat {
                    id,
                    count: md.packages.len(),
                    from_cache: md.from_cache,
                    error: None,
                });
                packages.extend(md.packages);
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
        packages,
        repos,
        base_urls,
        clients,
        elapsed,
    })
}

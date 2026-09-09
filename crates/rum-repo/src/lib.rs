//! Repository metadata layer for rum.
//!
//! Responsibilities:
//!   * resolve a repo's mirrors (baseurl / mirrorlist / metalink)
//!   * fetch `repodata/repomd.xml`, then the `primary` metadata it references
//!   * verify checksums, decompress, and parse into available packages
//!   * cache the result under the configured cachedir, honouring
//!     `metadata_expire`
//!   * do all of the above across repos in parallel
//!
//! This is the "librepo" replacement, in pure Rust (rustls TLS, pure-Rust
//! decompressors). It performs no writes to the system, only to rum's own
//! cache directory.

mod checksum;
mod decompress;
mod http;
mod primary;
mod repomd;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use checksum::{Checksum, ChecksumKind};
pub use http::{detect_aws_region, Http};
pub use primary::AvailablePackage;
pub use repomd::{RepoMd, RepoMdData};

use rum_config::Repo;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("http error fetching {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("no usable mirrors from {0}")]
    NoMirrors(String),
    #[error("all mirrors failed for repo `{0}`")]
    AllMirrorsFailed(String),
    #[error("repo `{repo}` has no `primary` metadata in repomd.xml")]
    NoPrimary { repo: String },
    #[error("checksum mismatch for {file} in repo `{repo}`")]
    ChecksumMismatch { repo: String, file: String },
    #[error("xml parse error: {0}")]
    Xml(String),
    #[error("tls configuration error: {0}")]
    Tls(String),
    #[error("decompression error: {0}")]
    Decompress(#[from] decompress::DecompressError),
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The parsed metadata for one repository after a sync.
#[derive(Debug)]
pub struct RepoMetadata {
    pub repo_id: String,
    pub packages: Vec<AvailablePackage>,
    /// True if served from the local cache without hitting the network.
    pub from_cache: bool,
    /// The base URL the metadata was fetched from; package `location` hrefs are
    /// relative to this. Needed to build RPM download URLs.
    pub base_url: String,
}

/// Options controlling a sync.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Root of rum's metadata cache (from MainConfig.cachedir).
    pub cachedir: PathBuf,
    /// Ignore cache freshness and re-download.
    pub force_refresh: bool,
}

impl SyncOptions {
    pub fn new(cachedir: impl Into<PathBuf>) -> Self {
        SyncOptions {
            cachedir: cachedir.into(),
            force_refresh: false,
        }
    }
}

/// Sync a single repository, returning its available packages.
pub fn sync_repo(repo: &Repo, opts: &SyncOptions) -> Result<RepoMetadata, RepoError> {
    let dir = opts.cachedir.join(&repo.id);
    let repomd_path = dir.join("repomd.xml");
    let primary_path = dir.join("primary.xml");
    let baseurl_path = dir.join("baseurl");

    // Fast path: fresh cache with a parsed-primary file and a recorded base URL.
    if !opts.force_refresh && primary_path.exists() && is_fresh(&repomd_path, repo.metadata_expire)
    {
        if let Ok(base_url) = std::fs::read_to_string(&baseurl_path) {
            let xml = read_file(&primary_path)?;
            let packages = primary::parse(&xml, &repo.id)?;
            tracing::debug!(repo = %repo.id, count = packages.len(), "loaded from cache");
            return Ok(RepoMetadata {
                repo_id: repo.id.clone(),
                packages,
                from_cache: true,
                base_url: base_url.trim().to_string(),
            });
        }
        // No cached base URL (older cache): fall through and refresh.
    }

    // Refresh: build an HTTP client honouring this repo's TLS settings, then
    // resolve mirrors and fetch repomd.xml from the first that works.
    let http = Http::for_repo(repo)?;
    let bases = http.resolve_baseurls(&repo.source)?;
    let (base, repomd_bytes) = fetch_repomd(&http, &repo.id, &bases)?;
    let repomd_str = String::from_utf8_lossy(&repomd_bytes);
    let md = RepoMd::parse(&repomd_str)?;

    let primary_entry = md.get("primary").ok_or_else(|| RepoError::NoPrimary {
        repo: repo.id.clone(),
    })?;

    // Download the primary file and verify its (compressed) checksum.
    let primary_url = join_url(&base, &primary_entry.location);
    let compressed = http.get_bytes(&primary_url)?;
    if !primary_entry.checksum.verify(&compressed) {
        return Err(RepoError::ChecksumMismatch {
            repo: repo.id.clone(),
            file: primary_entry.location.clone(),
        });
    }

    // Decompress and, if advertised, verify the decompressed checksum too.
    let plain = decompress::decompress(&primary_entry.location, &compressed)?;
    if let Some(oc) = &primary_entry.open_checksum {
        if !oc.verify(&plain) {
            return Err(RepoError::ChecksumMismatch {
                repo: repo.id.clone(),
                file: format!("{} (decompressed)", primary_entry.location),
            });
        }
    }

    let packages = primary::parse(&plain, &repo.id)?;
    tracing::info!(repo = %repo.id, count = packages.len(), %base, "refreshed metadata");

    // Persist to cache (best-effort ordering: primary last so a present
    // primary.xml always has a matching repomd.xml alongside it).
    write_cache(&dir, &repomd_path, &repomd_bytes, &primary_path, &plain)?;
    let _ = std::fs::write(&baseurl_path, &base);

    Ok(RepoMetadata {
        repo_id: repo.id.clone(),
        packages,
        from_cache: false,
        base_url: base,
    })
}

/// Sync many repos in parallel. Returns one result per repo (in the input
/// order); a failing repo does not abort the others, matching dnf's behaviour
/// of continuing with the repos that are reachable.
pub fn sync_all<'a>(
    repos: &'a [&'a Repo],
    opts: &SyncOptions,
) -> Vec<(String, Result<RepoMetadata, RepoError>)> {
    // Bound parallelism so we don't open an unreasonable number of sockets on
    // hosts with very many repos.
    let max_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(repos.len().max(1))
        .min(16);

    let mut results: Vec<(String, Result<RepoMetadata, RepoError>)> =
        Vec::with_capacity(repos.len());

    std::thread::scope(|scope| {
        // Simple static chunking across worker threads.
        let chunks: Vec<&[&Repo]> = repos
            .chunks(repos.len().div_ceil(max_threads).max(1))
            .collect();
        let mut handles = Vec::new();
        for chunk in chunks {
            handles.push(scope.spawn(move || {
                // Each repo builds its own client (it may carry a distinct TLS
                // client certificate, e.g. Red Hat RHUI).
                chunk
                    .iter()
                    .map(|r| (r.id.clone(), sync_repo(r, opts)))
                    .collect::<Vec<_>>()
            }));
        }
        for h in handles {
            if let Ok(mut part) = h.join() {
                results.append(&mut part);
            }
        }
    });

    // Restore input order (chunking preserves it, but be explicit).
    results.sort_by_key(|(id, _)| repos.iter().position(|r| &r.id == id).unwrap_or(usize::MAX));
    results
}

fn fetch_repomd(
    http: &Http,
    repo_id: &str,
    bases: &[String],
) -> Result<(String, Vec<u8>), RepoError> {
    let mut last_err = None;
    for base in bases {
        let url = format!("{base}/repodata/repomd.xml");
        match http.get_bytes(&url) {
            Ok(bytes) => return Ok((base.clone(), bytes)),
            Err(e) => {
                tracing::debug!(repo = %repo_id, %base, "mirror failed: {e}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| RepoError::AllMirrorsFailed(repo_id.to_string())))
}

/// Is the cached file present and younger than `expire` seconds?
/// `expire < 0` means never expire; `expire == 0` means always stale.
fn is_fresh(path: &Path, expire: i64) -> bool {
    if expire < 0 {
        return path.exists();
    }
    if expire == 0 {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age <= Duration::from_secs(expire as u64),
        // Modified in the future (clock skew): treat as fresh.
        Err(_) => true,
    }
}

fn join_url(base: &str, href: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        href.trim_start_matches('/')
    )
}

fn read_file(path: &Path) -> Result<Vec<u8>, RepoError> {
    std::fs::read(path).map_err(|source| RepoError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write_cache(
    dir: &Path,
    repomd_path: &Path,
    repomd: &[u8],
    primary_path: &Path,
    primary: &[u8],
) -> Result<(), RepoError> {
    std::fs::create_dir_all(dir).map_err(|source| RepoError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    std::fs::write(repomd_path, repomd).map_err(|source| RepoError::Io {
        path: repomd_path.to_path_buf(),
        source,
    })?;
    std::fs::write(primary_path, primary).map_err(|source| RepoError::Io {
        path: primary_path.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joining() {
        assert_eq!(
            join_url("https://a/repo", "repodata/x.gz"),
            "https://a/repo/repodata/x.gz"
        );
        assert_eq!(
            join_url("https://a/repo/", "/repodata/x.gz"),
            "https://a/repo/repodata/x.gz"
        );
    }

    #[test]
    fn freshness_rules() {
        // Non-existent path is never fresh.
        assert!(!is_fresh(Path::new("/nonexistent/repomd.xml"), 3600));

        // Use a freshly-created temp file (mtime ~= now).
        let tmp = std::env::temp_dir().join(format!("rum-fresh-{}", std::process::id()));
        std::fs::write(&tmp, b"x").unwrap();

        // expire == 0 is always stale even if the file exists.
        assert!(!is_fresh(&tmp, 0));
        // A long window on a just-written file is fresh.
        assert!(is_fresh(&tmp, 3600));
        // Never-expire on an existing file is fresh.
        assert!(is_fresh(&tmp, -1));

        std::fs::remove_file(&tmp).ok();
    }
}

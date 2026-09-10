//! `rum download [--resolve] [--destdir DIR] <packages...>`
//!
//! Resolves the requested packages (optionally pulling their full dependency
//! closure with --resolve), then downloads the RPMs in parallel, verifying each
//! against its repo checksum. Does not install anything.

use std::path::{Path, PathBuf};

use super::repo_sync;
use rum_repo::{AvailablePackage, Http};
use rum_solve::{resolve_sat, Candidate, Dep, Evr};

pub fn run(packages: &[String], with_deps: bool, destdir: &Path) -> anyhow::Result<()> {
    if packages.is_empty() {
        anyhow::bail!("`rum download` needs at least one package name");
    }
    let resolution = resolve_packages(packages, with_deps)?;
    if resolution.is_empty() {
        println!("Nothing to download.");
        return Ok(());
    }
    println!(
        "Downloading {} package(s), {} total, to {}",
        resolution.ids.len(),
        human(resolution.total_bytes()),
        destdir.display()
    );
    let fetched = fetch(&resolution, destdir)?;
    println!(
        "\nDownloaded {} package(s), {} in {:.2}s.",
        fetched.files.len(),
        human(fetched.total_bytes),
        fetched.elapsed.as_secs_f64()
    );
    Ok(())
}

/// A resolved transaction: which packages to act on, plus the repo data needed
/// to fetch them. Produced by [`resolve_packages`] *without* downloading, so
/// callers can show the transaction and confirm before any bytes move.
pub struct Resolution {
    synced: repo_sync::Synced,
    /// Indices into `synced.packages`, in resolved order.
    pub ids: Vec<usize>,
}

impl Resolution {
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
    /// NEVRAs of the resolved packages, sorted.
    pub fn nevras(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .ids
            .iter()
            .map(|&i| self.synced.packages[i].nevra())
            .collect();
        v.sort();
        v
    }
    pub fn total_bytes(&self) -> u64 {
        self.ids.iter().map(|&i| self.synced.packages[i].size).sum()
    }
}

/// The result of downloading a resolved package set.
pub struct Fetched {
    pub files: Vec<PathBuf>,
    pub total_bytes: u64,
    pub elapsed: std::time::Duration,
}

/// Resolve `packages` (optionally with their dependency closure) against the
/// enabled repos. Does NOT download anything.
pub fn resolve_packages(packages: &[String], with_deps: bool) -> anyhow::Result<Resolution> {
    let mut synced = repo_sync::sync_enabled(false)?;
    for r in &synced.repos {
        if let Some(e) = &r.error {
            eprintln!("warning: repo `{}` skipped: {e}", r.id);
        }
    }

    if !with_deps {
        let mut ids = Vec::new();
        for spec in packages {
            match best_match(spec, &synced.packages) {
                Some(i) if !ids.contains(&i) => ids.push(i),
                Some(_) => {}
                None => anyhow::bail!("no package found matching `{spec}`"),
            }
        }
        return Ok(Resolution { synced, ids });
    }

    let installed = installed_provides();
    let first = {
        let candidates = build_candidates(&synced.packages);
        resolve_sat(packages, &candidates, &installed)
    };

    match first {
        Ok(r) => Ok(Resolution {
            synced,
            ids: r.to_install,
        }),
        Err(e) => {
            // Resolution failed. If any file-path requirement is unmet, its
            // owner's path may only be in filelists.xml (not primary). Fetch
            // filelists for the wanted paths, attach them, and retry once.
            let wanted = unmet_file_requires(&synced.packages, &installed);
            if wanted.is_empty() {
                return Err(anyhow::anyhow!("dependency resolution failed: {e}"));
            }
            augment_with_filelists(&mut synced, &wanted);
            let candidates = build_candidates(&synced.packages);
            let ids = resolve_sat(packages, &candidates, &installed)
                .map_err(|e| anyhow::anyhow!("dependency resolution failed: {e}"))?
                .to_install;
            Ok(Resolution { synced, ids })
        }
    }
}

fn build_candidates(pkgs: &[AvailablePackage]) -> Vec<Candidate> {
    pkgs.iter()
        .enumerate()
        .map(|(i, p)| to_candidate(i, p))
        .collect()
}

/// File-path requirements (`/...`) across all packages that nothing currently
/// provides (via primary provides/files or the installed system).
fn unmet_file_requires(
    pkgs: &[AvailablePackage],
    installed: &[(String, Option<Evr>)],
) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    let mut providable: HashSet<&str> = HashSet::new();
    for p in pkgs {
        providable.insert(p.name.as_str());
        for pr in &p.provides {
            providable.insert(pr.name.as_str());
        }
        for f in &p.files {
            providable.insert(f.as_str());
        }
    }
    for (n, _) in installed {
        providable.insert(n.as_str());
    }
    let mut wanted = HashSet::new();
    for p in pkgs {
        for r in &p.requires {
            if r.name.starts_with('/') && !providable.contains(r.name.as_str()) {
                wanted.insert(r.name.clone());
            }
        }
    }
    wanted
}

/// Fetch filelists for the wanted paths and attach them to the owning packages
/// (matched by pkgid == primary checksum), so file deps resolve on retry.
fn augment_with_filelists(
    synced: &mut repo_sync::Synced,
    wanted: &std::collections::HashSet<String>,
) {
    use std::collections::HashMap;
    let by_pkgid: HashMap<String, usize> = synced
        .packages
        .iter()
        .enumerate()
        .map(|(i, p)| (p.checksum.hex.clone(), i))
        .collect();

    for (pkgid, files) in repo_sync::load_filelists(wanted) {
        if let Some(&i) = by_pkgid.get(&pkgid) {
            synced.packages[i].files.extend(files);
        }
    }
}

/// Download a resolved set to `destdir`, verifying checksums.
pub fn fetch(resolution: &Resolution, destdir: &Path) -> anyhow::Result<Fetched> {
    std::fs::create_dir_all(destdir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", destdir.display()))?;

    let mut jobs: Vec<Job> = Vec::new();
    for &i in &resolution.ids {
        let p = &resolution.synced.packages[i];
        let base = resolution
            .synced
            .base_urls
            .get(&p.repo_id)
            .cloned()
            .unwrap_or_default();
        if base.is_empty() {
            anyhow::bail!(
                "no base URL known for repo `{}` (run `rum makecache`)",
                p.repo_id
            );
        }
        jobs.push(Job {
            url: join_url(&base, &p.location),
            dest: destdir.join(basename(&p.location)),
            checksum: p.checksum.clone(),
            nevra: p.nevra(),
            repo_id: p.repo_id.clone(),
        });
    }

    let total_bytes = resolution.total_bytes();
    let start = std::time::Instant::now();
    let failures = download_all(&jobs, &resolution.synced.clients);
    let elapsed = start.elapsed();

    if !failures.is_empty() {
        for f in &failures {
            eprintln!("FAILED {f}");
        }
        anyhow::bail!("{} package(s) failed to download", failures.len());
    }

    Ok(Fetched {
        files: jobs.iter().map(|j| j.dest.clone()).collect(),
        total_bytes,
        elapsed,
    })
}

struct Job {
    url: String,
    dest: PathBuf,
    checksum: rum_repo::Checksum,
    nevra: String,
    repo_id: String,
}

/// Download all jobs across a bounded set of worker threads, using each repo's
/// own HTTP client (so mutual-TLS repos present their client certificate).
fn download_all(jobs: &[Job], clients: &std::collections::HashMap<String, Http>) -> Vec<String> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(jobs.len().max(1))
        .min(16);

    let default = Http::new();
    let mut failures = Vec::new();
    std::thread::scope(|scope| {
        let chunk_size = jobs.len().div_ceil(threads).max(1);
        let mut handles = Vec::new();
        for chunk in jobs.chunks(chunk_size) {
            let default = &default;
            handles.push(scope.spawn(move || {
                let mut errs = Vec::new();
                for job in chunk {
                    let http = clients.get(&job.repo_id).unwrap_or(default);
                    if let Err(e) = download_one(http, job) {
                        errs.push(format!("{}: {e}", job.nevra));
                    }
                }
                errs
            }));
        }
        for h in handles {
            if let Ok(mut errs) = h.join() {
                failures.append(&mut errs);
            }
        }
    });
    failures
}

fn download_one(http: &Http, job: &Job) -> anyhow::Result<()> {
    let bytes = http
        .get_bytes(&job.url)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !job.checksum.verify(&bytes) {
        anyhow::bail!("checksum mismatch");
    }
    std::fs::write(&job.dest, &bytes)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", job.dest.display()))?;
    Ok(())
}

/// Convert a repo package into a resolver candidate: provides come from the
/// package's Provides plus its advertised files (which satisfy file deps).
fn to_candidate(id: usize, p: &AvailablePackage) -> Candidate {
    let mut provides = p.provides.clone();
    for f in &p.files {
        provides.push(Dep::unversioned(f.clone()));
    }
    Candidate {
        id,
        name: p.name.clone(),
        arch: p.arch.clone(),
        evr: Evr::new(Some(p.epoch), p.version.clone(), p.release.clone()),
        provides,
        requires: p.requires.clone(),
        recommends: p.recommends.clone(),
    }
}

/// Installed provides (capabilities + files) as (name, optional EVR).
fn installed_provides() -> Vec<(String, Option<Evr>)> {
    match rum_rpm::Rpmdb::open() {
        Ok(db) => db
            .all_provides()
            .into_iter()
            .map(|(name, ver)| (name, ver.map(|s| Evr::parse(&s))))
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn best_match(spec: &str, pkgs: &[AvailablePackage]) -> Option<usize> {
    pkgs.iter()
        .enumerate()
        .filter(|(_, p)| p.name == spec || p.name_arch() == *spec)
        .max_by(|(_, a), (_, b)| {
            Evr::new(Some(a.epoch), a.version.clone(), a.release.clone()).compare(&Evr::new(
                Some(b.epoch),
                b.version.clone(),
                b.release.clone(),
            ))
        })
        .map(|(i, _)| i)
}

fn join_url(base: &str, href: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        href.trim_start_matches('/')
    )
}

fn basename(location: &str) -> &str {
    location.rsplit('/').next().unwrap_or(location)
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

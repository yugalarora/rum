//! `rum download [--resolve] [--destdir DIR] <packages...>`
//!
//! Resolves the requested packages (optionally pulling their full dependency
//! closure with --resolve), then downloads the RPMs in parallel, verifying each
//! against its repo checksum. Does not install anything.

use std::path::{Path, PathBuf};

use super::repo_sync;
use rum_repo::{AvailablePackage, Http};
use rum_solve::{resolve, Candidate, Dep, Evr};

pub fn run(packages: &[String], with_deps: bool, destdir: &Path) -> anyhow::Result<()> {
    if packages.is_empty() {
        anyhow::bail!("`rum download` needs at least one package name");
    }
    let fetched = resolve_and_fetch(packages, with_deps, destdir, true)?;
    println!(
        "\nDownloaded {} package(s), {} in {:.2}s.",
        fetched.files.len(),
        human(fetched.total_bytes),
        fetched.elapsed.as_secs_f64()
    );
    Ok(())
}

/// The result of resolving and downloading a package set.
pub struct Fetched {
    /// On-disk paths of the downloaded RPMs, in resolved order.
    pub files: Vec<PathBuf>,
    /// NEVRAs corresponding to `files`.
    pub nevras: Vec<String>,
    pub total_bytes: u64,
    pub elapsed: std::time::Duration,
}

/// Resolve `packages` (optionally with deps), download the RPMs to `destdir`
/// verifying checksums, and return their paths. Shared by `download` and
/// `install`.
pub fn resolve_and_fetch(
    packages: &[String],
    with_deps: bool,
    destdir: &Path,
    announce: bool,
) -> anyhow::Result<Fetched> {
    let synced = repo_sync::sync_enabled(false)?;
    for r in &synced.repos {
        if let Some(e) = &r.error {
            eprintln!("warning: repo `{}` skipped: {e}", r.id);
        }
    }

    // Build the candidate universe once. `id` indexes back into `pkgs`.
    let pkgs = synced.packages;
    let candidates: Vec<Candidate> = pkgs.iter().enumerate().map(|(i, p)| to_candidate(i, p)).collect();

    let ids: Vec<usize> = if with_deps {
        let installed = installed_provides();
        let resolved = resolve(packages, &candidates, &installed)
            .map_err(|e| anyhow::anyhow!("dependency resolution failed: {e}"))?;
        resolved.to_install
    } else {
        let mut ids = Vec::new();
        for spec in packages {
            match best_match(spec, &pkgs) {
                Some(i) if !ids.contains(&i) => ids.push(i),
                Some(_) => {}
                None => anyhow::bail!("no package found matching `{spec}`"),
            }
        }
        ids
    };

    if ids.is_empty() {
        return Ok(Fetched {
            files: Vec::new(),
            nevras: Vec::new(),
            total_bytes: 0,
            elapsed: std::time::Duration::ZERO,
        });
    }

    std::fs::create_dir_all(destdir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", destdir.display()))?;

    let mut jobs: Vec<Job> = Vec::new();
    for &i in &ids {
        let p = &pkgs[i];
        let base = synced.base_urls.get(&p.repo_id).cloned().unwrap_or_default();
        if base.is_empty() {
            anyhow::bail!("no base URL known for repo `{}` (run `rum makecache`)", p.repo_id);
        }
        jobs.push(Job {
            url: join_url(&base, &p.location),
            dest: destdir.join(basename(&p.location)),
            checksum: p.checksum.clone(),
            size: p.size,
            nevra: p.nevra(),
        });
    }

    let total_bytes: u64 = jobs.iter().map(|j| j.size).sum();
    if announce {
        println!(
            "Downloading {} package(s), {} total, to {}",
            jobs.len(),
            human(total_bytes),
            destdir.display()
        );
    }

    let start = std::time::Instant::now();
    let failures = download_all(&jobs);
    let elapsed = start.elapsed();

    if !failures.is_empty() {
        for f in &failures {
            eprintln!("FAILED {f}");
        }
        anyhow::bail!("{} package(s) failed to download", failures.len());
    }

    Ok(Fetched {
        files: jobs.iter().map(|j| j.dest.clone()).collect(),
        nevras: jobs.iter().map(|j| j.nevra.clone()).collect(),
        total_bytes,
        elapsed,
    })
}

struct Job {
    url: String,
    dest: PathBuf,
    checksum: rum_repo::Checksum,
    size: u64,
    nevra: String,
}

/// Download all jobs across a bounded set of worker threads.
fn download_all(jobs: &[Job]) -> Vec<String> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(jobs.len().max(1))
        .min(16);

    let mut failures = Vec::new();
    std::thread::scope(|scope| {
        let chunk_size = jobs.len().div_ceil(threads).max(1);
        let mut handles = Vec::new();
        for chunk in jobs.chunks(chunk_size) {
            handles.push(scope.spawn(move || {
                let http = Http::new();
                let mut errs = Vec::new();
                for job in chunk {
                    if let Err(e) = download_one(&http, job) {
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
    std::fs::write(&job.dest, &bytes).map_err(|e| anyhow::anyhow!("write {}: {e}", job.dest.display()))?;
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
            Evr::new(Some(a.epoch), a.version.clone(), a.release.clone())
                .compare(&Evr::new(Some(b.epoch), b.version.clone(), b.release.clone()))
        })
        .map(|(i, _)| i)
}

fn join_url(base: &str, href: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), href.trim_start_matches('/'))
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

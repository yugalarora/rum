//! `rum download [--resolve] [--destdir DIR] <packages...>`
//!
//! Resolves the requested packages (optionally pulling their full dependency
//! closure with --resolve), then downloads the RPMs in parallel, verifying each
//! against its repo checksum. Does not install anything.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::repo_sync;
use rum_repo::{AvailablePackage, Http, RepoMetadata};
use rum_solve::{resolve_sat_with, CandidateRef, CandidateSource, Dep, Evr, NameView};

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
    /// The full available-package set (owned; the resolve path indexes and
    /// mutates it, e.g. attaching filelists).
    packages: Vec<AvailablePackage>,
    /// repo id -> base URL, and repo id -> HTTP client, for fetching.
    base_urls: HashMap<String, String>,
    clients: HashMap<String, Http>,
    /// Indices into `packages`, in resolved order.
    pub ids: Vec<usize>,
}

impl Resolution {
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
    /// NEVRAs of the resolved packages, sorted.
    pub fn nevras(&self) -> Vec<String> {
        let mut v: Vec<String> = self.ids.iter().map(|&i| self.packages[i].nevra()).collect();
        v.sort();
        v
    }
    pub fn total_bytes(&self) -> u64 {
        self.ids.iter().map(|&i| self.packages[i].size).sum()
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
    let synced = repo_sync::sync_enabled(false)?;
    for r in &synced.repos {
        if let Some(e) = &r.error {
            eprintln!("warning: repo `{}` skipped: {e}", r.id);
        }
    }

    // Expand any `@group` / `@environment` targets into package names (fed to
    // the resolver as explicit installs). Plain package specs pass through.
    let requested: Vec<String> = {
        let has_group = packages.iter().any(|p| p.starts_with('@'));
        if !has_group {
            packages.to_vec()
        } else {
            let comps = super::groups::Comps::load(synced.metas());
            let db = rum_rpm::Rpmdb::open().ok();
            let mut out = Vec::new();
            for spec in packages {
                if let Some(group) = spec.strip_prefix('@') {
                    match comps.expand(group, db.as_ref()) {
                        Some(names) if !names.is_empty() => out.extend(names),
                        Some(_) => eprintln!("warning: group `{spec}` is empty"),
                        None => anyhow::bail!("no group or environment matching `{spec}`"),
                    }
                } else {
                    out.push(spec.clone());
                }
            }
            out
        }
    };
    let packages: &[String] = &requested;

    // Resolve against the repos' zero-copy views (no owned Vec of the whole
    // package set), then materialize only the winning packages. `gid` is a
    // global package index; `offsets[ri]..offsets[ri+1]` is repo ri's range.
    let (packages_out, ids) = {
        let metas = synced.metas();
        let mut offsets = Vec::with_capacity(metas.len() + 1);
        let mut acc = 0usize;
        for m in metas {
            offsets.push(acc);
            acc += m.len();
        }
        offsets.push(acc);

        let rehydrate = |gid: usize| -> Option<AvailablePackage> {
            let ri = offsets.partition_point(|&o| o <= gid).saturating_sub(1);
            metas.get(ri).and_then(|m| m.rehydrate(gid - offsets[ri]))
        };

        let winner_gids: Vec<usize> = if !with_deps {
            let mut gids = Vec::new();
            for spec in packages {
                match best_match_views(spec, metas, &offsets) {
                    Some(g) if !gids.contains(&g) => gids.push(g),
                    Some(_) => {}
                    None => anyhow::bail!("no package found matching `{spec}`"),
                }
            }
            gids
        } else {
            let installed = installed_provides();
            // Extra file provides discovered via the filelists fallback, keyed
            // by pkgid (primary checksum); injected into the source on retry.
            let mut extra: HashMap<String, Vec<String>> = HashMap::new();
            let first = {
                let src = MetasSource {
                    metas,
                    offsets: &offsets,
                    extra: &extra,
                };
                resolve_sat_with(packages, &src, &installed)
            };
            match first {
                Ok(r) => r.to_install,
                Err(e) => {
                    // A file-path requirement may only be satisfiable via
                    // filelists.xml (not primary). Fetch those paths, attach as
                    // extra provides on their owning packages, and retry once.
                    let wanted = unmet_file_requires_views(metas, &installed);
                    if wanted.is_empty() {
                        return Err(anyhow::anyhow!("dependency resolution failed: {e}"));
                    }
                    for (pkgid, files) in repo_sync::load_filelists(&wanted) {
                        extra.entry(pkgid).or_default().extend(files);
                    }
                    let src = MetasSource {
                        metas,
                        offsets: &offsets,
                        extra: &extra,
                    };
                    resolve_sat_with(packages, &src, &installed)
                        .map_err(|e| anyhow::anyhow!("dependency resolution failed: {e}"))?
                        .to_install
                }
            }
        };

        let mut pkgs = Vec::with_capacity(winner_gids.len());
        for g in winner_gids {
            if let Some(p) = rehydrate(g) {
                pkgs.push(p);
            }
        }
        let ids = (0..pkgs.len()).collect::<Vec<_>>();
        (pkgs, ids)
    };

    Ok(Resolution {
        packages: packages_out,
        base_urls: synced.base_urls,
        clients: synced.clients,
        ids,
    })
}

/// A [`CandidateSource`] over the synced repos' zero-copy views. Reads package
/// data straight from the mmap'd metadata; the only owned allocations per
/// candidate are the transient `Dep` vectors, dropped after each visit.
struct MetasSource<'a> {
    metas: &'a [RepoMetadata],
    offsets: &'a [usize],
    extra: &'a HashMap<String, Vec<String>>,
}

impl CandidateSource for MetasSource<'_> {
    fn scan(&self, required: &HashSet<String>, visit: &mut dyn FnMut(CandidateRef<'_>)) {
        for (ri, m) in self.metas.iter().enumerate() {
            let base = self.offsets[ri];
            for (pi, p) in m.views().enumerate() {
                let mut provides = p.provides_with_files_filtered(required);
                if let Some(files) = self.extra.get(p.checksum_hex()) {
                    for f in files {
                        if required.contains(f) {
                            provides.push(Dep::unversioned(f.clone()));
                        }
                    }
                }
                let requires = p.requires();
                let recommends = p.recommends();
                visit(CandidateRef {
                    id: base + pi,
                    name: p.name(),
                    arch: p.arch(),
                    evr: p.evr_cmp(),
                    provides: &provides,
                    requires: &requires,
                    recommends: &recommends,
                });
            }
        }
    }

    fn scan_names(&self, visit: &mut dyn FnMut(NameView<'_>)) {
        for m in self.metas {
            for p in m.views() {
                let provide_names = p.provide_names();
                let require_names = p.require_names();
                let recommend_names = p.recommend_names();
                visit(NameView {
                    name: p.name(),
                    arch: p.arch(),
                    provide_names: &provide_names,
                    require_names: &require_names,
                    recommend_names: &recommend_names,
                });
            }
        }
    }
}

/// Highest-EVR package matching `spec` (name or name.arch) across all repos,
/// returned as a global index.
fn best_match_views(spec: &str, metas: &[RepoMetadata], offsets: &[usize]) -> Option<usize> {
    let mut best: Option<(usize, Evr)> = None;
    for (ri, m) in metas.iter().enumerate() {
        for (pi, p) in m.views().enumerate() {
            if p.name() == spec || p.name_arch() == spec {
                let evr = p.evr_cmp();
                let better = match &best {
                    Some((_, b)) => evr > *b,
                    None => true,
                };
                if better {
                    best = Some((offsets[ri] + pi, evr));
                }
            }
        }
    }
    best.map(|(g, _)| g)
}

/// File-path requirements (`/...`) that nothing provides via primary
/// provides/files or the installed system — candidates for the filelists
/// fallback.
fn unmet_file_requires_views(
    metas: &[RepoMetadata],
    installed: &[(String, Option<Evr>)],
) -> HashSet<String> {
    let mut providable: HashSet<String> = HashSet::new();
    for m in metas {
        for p in m.views() {
            providable.insert(p.name().to_string());
            for n in p.provide_names() {
                providable.insert(n.to_string());
            }
            for n in p.file_names() {
                providable.insert(n.to_string());
            }
        }
    }
    for (n, _) in installed {
        providable.insert(n.clone());
    }
    let mut wanted = HashSet::new();
    for m in metas {
        for p in m.views() {
            for r in p.require_names() {
                if r.starts_with('/') && !providable.contains(r) {
                    wanted.insert(r.to_string());
                }
            }
        }
    }
    wanted
}

/// Download a resolved set to `destdir`, verifying checksums.
pub fn fetch(resolution: &Resolution, destdir: &Path) -> anyhow::Result<Fetched> {
    std::fs::create_dir_all(destdir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", destdir.display()))?;

    let mut jobs: Vec<Job> = Vec::new();
    for &i in &resolution.ids {
        let p = &resolution.packages[i];
        let base = resolution
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
    let failures = download_all(&jobs, &resolution.clients);
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

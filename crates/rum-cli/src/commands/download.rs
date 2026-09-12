//! `rum download [--resolve] [--destdir DIR] <packages...>`
//!
//! Resolves the requested packages (optionally pulling their full dependency
//! closure with --resolve), then downloads the RPMs in parallel, verifying each
//! against its repo checksum. Does not install anything.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::repo_sync;
use rum_repo::{AvailablePackage, Http, RepoMetadata};
use rum_solve::{resolve_sat_with, CandidateRef, CandidateSource, Dep, Evr, NameView, RichExpr};

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
            let mut explicit = Vec::new();
            let mut group_members: Vec<String> = Vec::new();
            for spec in packages {
                if let Some(group) = spec.strip_prefix('@') {
                    match comps.expand(group, db.as_ref()) {
                        Some(names) if !names.is_empty() => group_members.extend(names),
                        Some(_) => eprintln!("warning: group `{spec}` is empty"),
                        None => anyhow::bail!("no group or environment matching `{spec}`"),
                    }
                } else {
                    explicit.push(spec.clone());
                }
            }
            // A group can list members not present in the enabled repos (e.g.
            // AL2023's @development lists `rcs`); dnf silently skips those, so we
            // drop group members that no repo provides (by name or capability)
            // rather than failing the whole group. Explicit targets still error.
            if !group_members.is_empty() {
                let mut available: HashSet<String> = HashSet::new();
                for m in synced.metas() {
                    for p in m.views() {
                        available.insert(p.name().to_string());
                        for pr in p.provide_names() {
                            available.insert(pr.to_string());
                        }
                    }
                }
                let before = group_members.len();
                group_members.retain(|n| {
                    available.contains(n) || db.as_ref().is_some_and(|d| d.is_installed(n))
                });
                let dropped = before - group_members.len();
                if dropped > 0 {
                    eprintln!("note: skipped {dropped} group package(s) not in the enabled repos");
                }
            }
            explicit.extend(group_members);
            explicit
        }
    };
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

        // Installed provides (for the SAT installed-synthetics), computed once.
        let installed = if with_deps {
            installed_provides()
        } else {
            Vec::new()
        };

        // One resolve pass for a target set (multilib-filtered; SAT with the
        // filelists fallback, or best-match for a bare download).
        let resolve_targets = |targets: &[String]| -> anyhow::Result<Vec<usize>> {
            let allow = arches_for(targets);
            if !with_deps {
                let mut gids = Vec::new();
                for spec in targets {
                    match best_match_views(spec, metas, &offsets, &allow) {
                        Some(g) if !gids.contains(&g) => gids.push(g),
                        Some(_) => {}
                        None => anyhow::bail!("no package found matching `{spec}`"),
                    }
                }
                return Ok(gids);
            }
            let mut extra: HashMap<String, Vec<String>> = HashMap::new();
            let first = {
                let src = MetasSource {
                    metas,
                    offsets: &offsets,
                    extra: &extra,
                    allow_arches: &allow,
                };
                resolve_sat_with(targets, &src, &installed)
            };
            match first {
                Ok(r) => Ok(r.to_install),
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
                        allow_arches: &allow,
                    };
                    Ok(resolve_sat_with(targets, &src, &installed)
                        .map_err(|e| anyhow::anyhow!("dependency resolution failed: {e}"))?
                        .to_install)
                }
            }
        };

        // Lockstep-upgrade augmentation: an installed package co-built with one
        // we're upgrading may require it at an EXACT `= version` (e.g.
        // `NetworkManager-tui` needs the old `NetworkManager`); upgrading only
        // part of the set makes rpm reject the transaction. Detect such broken
        // installed requirers via the rpmdb and pull them into the target set,
        // re-resolving to a fixpoint (bounded), so the whole coupled set
        // upgrades together — matching dnf.
        let gids = resolve_targets(&requested)?;
        let mut winners: Vec<AvailablePackage> = gids.into_iter().filter_map(rehydrate).collect();

        // Lockstep-upgrade augmentation. An installed package we're upgrading may
        // have installed co-built siblings pinned to it via `= exact-version`
        // (e.g. NetworkManager-{tui,cloud-setup}); a name target won't upgrade
        // them (the resolver keeps the installed version), so we pull the repo
        // build at the SAME new EVR (co-built subpackages share it) directly
        // into the winner set, to a fixpoint. rpm validates the final set.
        if with_deps {
            if let Ok(db) = rum_rpm::Rpmdb::open() {
                let rd = db.installed_reverse_deps();
                let mut seen: HashSet<String> = winners.iter().map(|w| w.name.clone()).collect();
                let mut queue: Vec<AvailablePackage> = winners.clone();
                while let Some(w) = queue.pop() {
                    let w_evr = Evr::new(Some(w.epoch), w.version.clone(), w.release.clone());
                    // Only upgrades of installed packages can break a pinned sibling.
                    if db.by_name(&w.name).is_empty() {
                        continue;
                    }
                    let Some(requirers) = rd.exact.get(&w.name) else {
                        continue;
                    };
                    for (rqr, reqver) in requirers {
                        // Label comparison (epoch-normalized): the require string
                        // may omit the epoch that `w_evr` carries, so structural
                        // `==` would spuriously differ post the Evr epoch change.
                        if seen.contains(rqr)
                            || Evr::parse(reqver).compare(&w_evr) == std::cmp::Ordering::Equal
                        {
                            continue;
                        }
                        // The co-built sibling build that pins the NEW version
                        // shares its EVR; pull that exact one from the repo.
                        if let Some(pkg) = find_pkg_at(rqr, &w_evr, metas) {
                            seen.insert(rqr.clone());
                            winners.push(pkg.clone());
                            queue.push(pkg);
                        }
                    }
                }

                // Rich reverse-dep augmentation. An installed package may carry a
                // conditional `(X = V if Y)` require: once Y is present (installed
                // or being installed), rpm demands X at V. rum's resolver doesn't
                // model installed packages' rich requires, so satisfy them here by
                // pulling X. Example: installed `systemd` needs
                // `(systemd-rpm-macros = ... if rpm-build)`, and `@development`
                // pulls `rpm-build`, so `systemd-rpm-macros` must come along.
                // Iterate to a fixpoint (bounded by the finite rich-dep set).
                loop {
                    let mut added = false;
                    for (_rqr, expr) in &rd.rich {
                        // Only `(then if cond)` / `(then if cond else _)` where both
                        // sides are plain terms are actionable as a pull.
                        let (then, cond) = match rum_solve::parse_rich(expr) {
                            Some(RichExpr::If(t, c)) | Some(RichExpr::IfElse(t, c, _)) => {
                                match (*t, *c) {
                                    (RichExpr::Term(t), RichExpr::Term(c)) => (t, c),
                                    _ => continue,
                                }
                            }
                            _ => continue,
                        };
                        // Condition active? (being installed, or already installed)
                        let cond_active = seen.contains(&cond.name) || db.is_installed(&cond.name);
                        if !cond_active {
                            continue;
                        }
                        // Already satisfied by a winner or an installed build?
                        if seen.contains(&then.name) {
                            continue;
                        }
                        let want_evr = then.evr.clone();
                        let installed_ok = match &want_evr {
                            Some(v) => db.by_name(&then.name).iter().any(|p| {
                                Evr::new(p.epoch, &p.version, &p.release).compare(v)
                                    == std::cmp::Ordering::Equal
                            }),
                            None => db.is_installed(&then.name),
                        };
                        if installed_ok {
                            continue;
                        }
                        // Pull the provider: the exact build for a versioned `=`,
                        // else the newest available by name.
                        let pulled = match &want_evr {
                            Some(v) => find_pkg_at(&then.name, v, metas),
                            None => best_match_views(&then.name, metas, &offsets, &arches_for(&[]))
                                .and_then(rehydrate),
                        };
                        if let Some(pkg) = pulled {
                            seen.insert(then.name.clone());
                            winners.push(pkg);
                            added = true;
                        }
                    }
                    if !added {
                        break;
                    }
                }

                // Obsoletes replacement (B1). dnf's obsoletes processing: when
                // the transaction touches an installed package that an available
                // package Obsoletes, pull the obsoleter in so it REPLACES the
                // obsoleted one — rpm erases the obsoleted at commit because the
                // obsoleter is in the transaction. Scoped to the transaction's
                // lineage (installed packages that are requested or being
                // upgraded), NOT a global system sweep, matching dnf.
                let affected: HashSet<String> = requested
                    .iter()
                    .cloned()
                    .chain(seen.iter().cloned())
                    .collect();
                let installed: Vec<(String, Evr)> = db
                    .installed()
                    .into_iter()
                    .filter(|p| affected.contains(&p.name))
                    .map(|p| (p.name.clone(), Evr::new(p.epoch, p.version, p.release)))
                    .collect();
                if !installed.is_empty() {
                    for (ri, m) in metas.iter().enumerate() {
                        for (pi, p) in m.views().enumerate() {
                            let obs = p.obsoletes();
                            if obs.is_empty() {
                                continue;
                            }
                            let replaces = installed.iter().any(|(iname, ievr)| {
                                obs.iter().any(|o| o.satisfied_by(iname, Some(ievr)))
                            });
                            if !replaces {
                                continue;
                            }
                            let name = p.name().to_string();
                            // Skip if already a winner or already installed (only
                            // pull a NEW obsoleter that isn't in the plan yet).
                            if seen.contains(&name) || db.is_installed(&name) {
                                continue;
                            }
                            if let Some(pkg) = rehydrate(offsets[ri] + pi) {
                                seen.insert(name);
                                winners.push(pkg);
                            }
                        }
                    }
                }
            }
        }

        let ids = (0..winners.len()).collect::<Vec<_>>();
        (winners, ids)
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
    /// Package arches to consider (host arch + noarch, plus any explicitly
    /// requested). Packages of other arches are skipped (multilib policy).
    allow_arches: &'a HashSet<String>,
}

impl CandidateSource for MetasSource<'_> {
    fn scan(&self, required: &HashSet<String>, visit: &mut dyn FnMut(CandidateRef<'_>)) {
        for (ri, m) in self.metas.iter().enumerate() {
            let base = self.offsets[ri];
            for (pi, p) in m.views().enumerate() {
                // Skip disallowed arches; `pi` still advances so global ids
                // (base + pi) stay aligned with `rehydrate`.
                if !self.allow_arches.contains(p.arch()) {
                    continue;
                }
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
                if !self.allow_arches.contains(p.arch()) {
                    continue;
                }
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

/// Allowed package arches for a target set: host arch + noarch, plus any arch
/// a target explicitly names (`glibc.i686`).
fn arches_for(targets: &[String]) -> HashSet<String> {
    let mut a = HashSet::new();
    a.insert(std::env::consts::ARCH.to_string());
    a.insert("noarch".to_string());
    for spec in targets {
        if let Some(arch) = explicit_arch(spec) {
            a.insert(arch.to_string());
        }
    }
    a
}

/// Find the repo package named `name` at exactly EVR `evr` across all repos,
/// returned as an owned `AvailablePackage` (used to pull a co-built sibling at
/// the same new version as the package it's pinned to).
fn find_pkg_at(name: &str, evr: &Evr, metas: &[RepoMetadata]) -> Option<AvailablePackage> {
    for m in metas {
        for (pi, p) in m.views().enumerate() {
            if p.name() == name && &p.evr_cmp() == evr {
                return m.rehydrate(pi);
            }
        }
    }
    None
}

/// The trailing `.arch` of a spec, if it names a known RPM architecture
/// (so `glibc.i686` re-allows i686, but `python3.11` is not an arch).
fn explicit_arch(spec: &str) -> Option<&str> {
    const ARCHES: &[&str] = &[
        "x86_64", "i686", "i386", "aarch64", "noarch", "armv7hl", "ppc64le", "s390x", "riscv64",
    ];
    let (_, arch) = spec.rsplit_once('.')?;
    ARCHES.contains(&arch).then_some(arch)
}

/// Highest-EVR package matching `spec` (name or name.arch) across all repos,
/// restricted to allowed arches, returned as a global index.
fn best_match_views(
    spec: &str,
    metas: &[RepoMetadata],
    offsets: &[usize],
    allow_arches: &HashSet<String>,
) -> Option<usize> {
    let mut best: Option<(usize, Evr)> = None;
    for (ri, m) in metas.iter().enumerate() {
        for (pi, p) in m.views().enumerate() {
            if !allow_arches.contains(p.arch()) {
                continue;
            }
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
        encode_path(href.trim_start_matches('/'))
    )
}

/// Percent-encode a URL path (RFC 3986): every byte outside the unreserved set
/// (ALPHA / DIGIT / `-._~`) is `%`-escaped, except the `/` separator, `%` (so
/// an already-encoded href is not double-encoded), and `?`/`#`/`&`/`=` so any
/// query string is preserved. Repo `location` hrefs are raw filenames, so a
/// literal `+` (e.g. `gcc-c++-...rpm`) must become `%2B` — S3 treats an
/// unencoded `+` as a different key and returns 403 AccessDenied, which is why
/// `+`-named packages failed to download while every other package worked.
fn encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/'
            | b'%'
            | b'?'
            | b'#'
            | b'&'
            | b'=' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_escapes_plus_keeps_structure() {
        // '+' must become %2B (the gcc-c++ / libstdc++ download bug); path
        // separators, '..', and unreserved chars are preserved.
        assert_eq!(
            encode_path("../../../../blobstore/abc/gcc-c++-11.5.0-5.amzn2023.x86_64.rpm"),
            "../../../../blobstore/abc/gcc-c%2B%2B-11.5.0-5.amzn2023.x86_64.rpm"
        );
        // Spaces encode; already-encoded input is not double-encoded.
        assert_eq!(encode_path("a b"), "a%20b");
        assert_eq!(encode_path("a%2Bb"), "a%2Bb");
        // A query string is preserved verbatim.
        assert_eq!(
            encode_path("blobstore/x/f.rpm?k=v"),
            "blobstore/x/f.rpm?k=v"
        );
    }

    #[test]
    fn join_url_encodes_href() {
        assert_eq!(
            join_url(
                "https://h/core/x86_64/",
                "../../blobstore/z/libstdc++-1.rpm"
            ),
            "https://h/core/x86_64/../../blobstore/z/libstdc%2B%2B-1.rpm"
        );
    }
}

//! SAT-based dependency resolution via the `resolvo` solver.
//!
//! resolvo (like conda's solver) is a name+version CDCL solver with no native
//! concept of RPM "Provides". We bridge that gap:
//!
//!   * Each concrete package is interned once as a solvable under its own name.
//!   * That same solvable is registered as a candidate under *every* capability
//!     it provides (its name, sonames, files, virtual provides). Because it is
//!     the same `SolvableId`, selecting it to satisfy any capability counts as
//!     one install — provides are deduplicated for free.
//!   * The installed system is modeled as preferred "synthetic" solvables (one
//!     per installed capability+version) that carry no dependencies. Sorting
//!     them first makes the solver keep already-installed dependencies instead
//!     of gratuitously upgrading them — matching `dnf install` semantics.
//!
//! Versioned requirements are expressed as `Ranges<Evr>` over our RPM-correct
//! `Evr` ordering, so the solver's version choices use real rpmvercmp. A
//! versioned Provides carries its own advertised version; an *unversioned*
//! Provides is flagged a wildcard and satisfies any require (RPM's rpmdsCompare
//! rule). This yields real backtracking and matches dnf on ordinary closures.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;

use resolvo::utils::Pool;
use resolvo::{
    Candidates, Condition, ConditionId, ConditionalRequirement, Dependencies, DependencyProvider,
    Interner, KnownDependencies, LogicalOperator, NameId, Problem, Requirement, SolvableId, Solver,
    SolverCache, StringId, UnsolvableOrCancelled, VersionSetId, VersionSetUnionId,
};
use version_ranges::Ranges;

use crate::dep::DepFlag;
use crate::resolve::{Candidate, ResolveError, Resolved};
use crate::richdep::{self, RichExpr};
use crate::{Dep, Evr};

/// A borrowed view of one candidate package, valid only for the duration of a
/// single visit. This lets the solver read package data (name, deps) straight
/// from a caller's zero-copy store (e.g. rum-repo's mmap'd metadata) without
/// materializing an owned `Candidate` for every package — essential for large
/// repos on small hosts.
pub struct CandidateRef<'a> {
    /// Caller-defined handle (e.g. a global package index) echoed back in the
    /// resolved set.
    pub id: usize,
    pub name: &'a str,
    pub arch: &'a str,
    pub evr: Evr,
    pub provides: &'a [Dep],
    pub requires: &'a [Dep],
    pub recommends: &'a [Dep],
}

/// A name-only view of one candidate, for the cheap pre-passes (which capability
/// names are provided/required/recommended). Carries borrowed `&str`s, so no
/// `Dep`/`String` is allocated — unlike [`CandidateRef`], whose owned `Dep`
/// vectors are only needed for the final pool build.
pub struct NameView<'a> {
    pub name: &'a str,
    pub arch: &'a str,
    /// Provided capability names (excluding files — see the providable pass).
    pub provide_names: &'a [&'a str],
    pub require_names: &'a [&'a str],
    pub recommend_names: &'a [&'a str],
}

/// A source of candidates that can be scanned repeatedly without holding them
/// all in memory at once. The solver runs one cheap `scan_names` pre-pass
/// (requested/providable/required sets) and one full `scan` to build the pool,
/// so implementations must be cheap to re-iterate.
pub trait CandidateSource {
    /// Full candidates (with owned `Dep`s); used once, to build the pool.
    /// `required` is the set of capability names the pool will actually
    /// register, so implementations may drop provides/files not in it (the
    /// build ignores them anyway) to avoid allocating `Dep`s for the millions
    /// of never-depended-upon files in a distro's metadata.
    fn scan(&self, required: &HashSet<String>, visit: &mut dyn FnMut(CandidateRef<'_>));
    /// Name-only pass (borrowed `&str`, no `Dep` allocation); used for the
    /// providable/required/requested sets.
    fn scan_names(&self, visit: &mut dyn FnMut(NameView<'_>));
}

/// A slice of owned `Candidate`s is a trivial source (used by tests and the
/// greedy path). Its `id` is the candidate's own `id` field.
impl CandidateSource for [Candidate] {
    fn scan(&self, required: &HashSet<String>, visit: &mut dyn FnMut(CandidateRef<'_>)) {
        for c in self {
            let provides: Vec<Dep> = c
                .provides
                .iter()
                .filter(|d| required.contains(&d.name))
                .cloned()
                .collect();
            visit(CandidateRef {
                id: c.id,
                name: &c.name,
                arch: &c.arch,
                evr: c.evr.clone(),
                provides: &provides,
                requires: &c.requires,
                recommends: &c.recommends,
            });
        }
    }
    fn scan_names(&self, visit: &mut dyn FnMut(NameView<'_>)) {
        for c in self {
            let pv: Vec<&str> = c.provides.iter().map(|d| d.name.as_str()).collect();
            let rq: Vec<&str> = c.requires.iter().map(|d| d.name.as_str()).collect();
            let rc: Vec<&str> = c.recommends.iter().map(|d| d.name.as_str()).collect();
            visit(NameView {
                name: &c.name,
                arch: &c.arch,
                provide_names: &pv,
                require_names: &rq,
                recommend_names: &rc,
            });
        }
    }
}

struct RpmProvider {
    pool: Pool<Ranges<Evr>>,
    /// capability NameId -> solvables providing it (repo + installed synthetic)
    providers: HashMap<NameId, Vec<SolvableId>>,
    /// solvable -> its dependency requirements
    deps: HashMap<SolvableId, Vec<ConditionalRequirement>>,
    /// solvables that represent already-installed capabilities (preferred, and
    /// excluded from the install output)
    installed: HashSet<SolvableId>,
    /// solvable -> caller candidate id (only for real repo packages)
    candidate_of: HashMap<SolvableId, usize>,
    /// solvable -> the underlying package's EVR. Candidate ranking must use the
    /// package version, not the solvable's record (which for a capability
    /// provider is the *provided* version and can tie across package builds).
    pkg_evr: HashMap<SolvableId, Evr>,
    /// Solvables created from an *unversioned* Provides (or a file). Per RPM's
    /// `rpmdsCompare`, a versionless provide satisfies ANY versioned require, so
    /// these must pass the version-set filter unconditionally.
    wildcard: HashSet<SolvableId>,
    /// Conditions referenced by conditional requirements (rich/boolean deps),
    /// indexed by `ConditionId`.
    conditions: Vec<Condition>,
    /// The originating Dep for each versioned version-set, so `filter_candidates`
    /// can apply RPM's partial-EVR comparison (e.g. `= 15.0.7` with no release
    /// matches `15.0.7-3.amzn2023.0.4`) instead of an exact Ranges match.
    vsdep: HashMap<VersionSetId, Dep>,
}

/// `rpmlib(...)` feature flags are satisfied by rpm itself, not repo packages.
fn is_ignorable_dep(name: &str) -> bool {
    name.starts_with("rpmlib(")
}

/// Collect every capability name referenced (as a Term) in a rich expression.
fn collect_rich_names(e: &RichExpr, out: &mut HashSet<String>) {
    match e {
        RichExpr::Term(d) => {
            out.insert(d.name.clone());
        }
        RichExpr::And(a, b)
        | RichExpr::Or(a, b)
        | RichExpr::If(a, b)
        | RichExpr::Unless(a, b)
        | RichExpr::With(a, b)
        | RichExpr::Without(a, b) => {
            collect_rich_names(a, out);
            collect_rich_names(b, out);
        }
        RichExpr::IfElse(a, b, c) | RichExpr::UnlessElse(a, b, c) => {
            collect_rich_names(a, out);
            collect_rich_names(b, out);
            collect_rich_names(c, out);
        }
    }
}

/// Mint a new ConditionId for `cond`.
fn mint(conds: &mut Vec<Condition>, cond: Condition) -> ConditionId {
    let id = ConditionId::new(conds.len() as u32);
    conds.push(cond);
    id
}

/// Build a resolvo Condition from a rich expression (used on the condition side
/// of `if`/`unless`). and/or become Binary; anything else falls back to its
/// leftmost term.
fn build_cond(
    pool: &Pool<Ranges<Evr>>,
    conds: &mut Vec<Condition>,
    vsdep: &mut HashMap<VersionSetId, Dep>,
    e: &RichExpr,
) -> ConditionId {
    match e {
        RichExpr::And(a, b) => {
            let ca = build_cond(pool, conds, vsdep, a);
            let cb = build_cond(pool, conds, vsdep, b);
            mint(conds, Condition::Binary(LogicalOperator::And, ca, cb))
        }
        RichExpr::Or(a, b) => {
            let ca = build_cond(pool, conds, vsdep, a);
            let cb = build_cond(pool, conds, vsdep, b);
            mint(conds, Condition::Binary(LogicalOperator::Or, ca, cb))
        }
        RichExpr::Term(d) => {
            let cap = pool.intern_package_name(d.name.clone());
            mint(
                conds,
                Condition::Requirement(version_set(pool, vsdep, cap, d)),
            )
        }
        // Rare: a compound condition; approximate with its leftmost term.
        RichExpr::If(a, _)
        | RichExpr::IfElse(a, _, _)
        | RichExpr::Unless(a, _)
        | RichExpr::UnlessElse(a, _, _)
        | RichExpr::With(a, _)
        | RichExpr::Without(a, _) => build_cond(pool, conds, vsdep, a),
    }
}

/// Collect the version sets of all Term leaves (used to build an OR union).
fn collect_or_vss(
    pool: &Pool<Ranges<Evr>>,
    vsdep: &mut HashMap<VersionSetId, Dep>,
    e: &RichExpr,
    out: &mut Vec<VersionSetId>,
) {
    match e {
        RichExpr::Term(d) => {
            let cap = pool.intern_package_name(d.name.clone());
            out.push(version_set(pool, vsdep, cap, d));
        }
        RichExpr::And(a, b) | RichExpr::Or(a, b) => {
            collect_or_vss(pool, vsdep, a, out);
            collect_or_vss(pool, vsdep, b, out);
        }
        _ => {}
    }
}

/// Is a condition expression already satisfied by the installed system? RPM's
/// `if`/`unless` condition on installed state, so we pre-evaluate against the
/// installed capabilities (by name).
fn cond_installed(e: &RichExpr, installed: &HashSet<String>) -> bool {
    match e {
        RichExpr::Term(d) => installed.contains(&d.name),
        RichExpr::And(a, b) => cond_installed(a, installed) && cond_installed(b, installed),
        RichExpr::Or(a, b) => cond_installed(a, installed) || cond_installed(b, installed),
        RichExpr::If(a, _)
        | RichExpr::IfElse(a, _, _)
        | RichExpr::Unless(a, _)
        | RichExpr::UnlessElse(a, _, _)
        | RichExpr::With(a, _)
        | RichExpr::Without(a, _) => cond_installed(a, installed),
    }
}

/// Map a rich expression to resolvo conditional requirements, appending to `out`.
/// `cond` is an ambient condition (from an enclosing `if`). `installed` is the
/// set of installed capability names, used to pre-evaluate `if`/`unless`.
#[allow(clippy::too_many_arguments)]
fn emit_rich(
    pool: &Pool<Ranges<Evr>>,
    conds: &mut Vec<Condition>,
    vsdep: &mut HashMap<VersionSetId, Dep>,
    installed: &HashSet<String>,
    e: &RichExpr,
    cond: Option<ConditionId>,
    out: &mut Vec<ConditionalRequirement>,
) {
    match e {
        RichExpr::Term(d) => {
            let cap = pool.intern_package_name(d.name.clone());
            out.push(ConditionalRequirement {
                condition: cond,
                requirement: Requirement::Single(version_set(pool, vsdep, cap, d)),
            });
        }
        RichExpr::And(a, b) => {
            emit_rich(pool, conds, vsdep, installed, a, cond, out);
            emit_rich(pool, conds, vsdep, installed, b, cond, out);
        }
        RichExpr::Or(..) => {
            let mut vss = Vec::new();
            collect_or_vss(pool, vsdep, e, &mut vss);
            if let Some((first, rest)) = vss.split_first() {
                let union = pool.intern_version_set_union(*first, rest.iter().copied());
                out.push(ConditionalRequirement {
                    condition: cond,
                    requirement: Requirement::Union(union),
                });
            }
        }
        // `then if cond`: if cond already holds on the system, require `then`
        // unconditionally; otherwise make it conditional on cond entering the
        // transaction (so both dnf senses — installed or co-installed — work).
        RichExpr::If(then, c) => {
            if cond_installed(c, installed) {
                emit_rich(pool, conds, vsdep, installed, then, cond, out);
            } else {
                let cid = build_cond(pool, conds, vsdep, c);
                let combined = match cond {
                    None => cid,
                    Some(o) => mint(conds, Condition::Binary(LogicalOperator::And, o, cid)),
                };
                emit_rich(pool, conds, vsdep, installed, then, Some(combined), out);
            }
        }
        // `then if cond else els`: pick the branch by installed state.
        RichExpr::IfElse(then, c, els) => {
            let branch = if cond_installed(c, installed) {
                then
            } else {
                els
            };
            emit_rich(pool, conds, vsdep, installed, branch, cond, out);
        }
        // `body unless cond`: require body unless cond is present.
        RichExpr::Unless(body, c) => {
            if !cond_installed(c, installed) {
                emit_rich(pool, conds, vsdep, installed, body, cond, out);
            }
        }
        RichExpr::UnlessElse(body, c, els) => {
            let branch = if cond_installed(c, installed) {
                els
            } else {
                body
            };
            emit_rich(pool, conds, vsdep, installed, branch, cond, out);
        }
        // `with`/`without`: no intersection in resolvo; require the primary
        // operand (approximation, documented).
        RichExpr::With(a, _) | RichExpr::Without(a, _) => {
            emit_rich(pool, conds, vsdep, installed, a, cond, out)
        }
    }
}

fn version_set(
    pool: &Pool<Ranges<Evr>>,
    vsdep: &mut HashMap<VersionSetId, Dep>,
    cap: NameId,
    dep: &Dep,
) -> VersionSetId {
    // The Ranges is a coarse approximation kept for display; the authoritative
    // match happens in filter_candidates via the stored Dep (RPM semantics).
    let ranges = match (&dep.evr, dep.flag) {
        (None, _) | (_, DepFlag::Any) => Ranges::full(),
        (Some(e), DepFlag::Eq) => Ranges::singleton(e.clone()),
        (Some(e), DepFlag::Lt) => Ranges::strictly_lower_than(e.clone()),
        (Some(e), DepFlag::Le) => Ranges::lower_than(e.clone()),
        (Some(e), DepFlag::Gt) => Ranges::strictly_higher_than(e.clone()),
        (Some(e), DepFlag::Ge) => Ranges::higher_than(e.clone()),
    };
    let vsid = pool.intern_version_set(cap, ranges);
    if dep.evr.is_some() {
        vsdep.insert(vsid, dep.clone());
    }
    vsid
}

/// Build the resolvo provider.
///
/// resolvo requires every candidate returned for a capability to share one
/// package name, so we intern a *distinct* provider solvable per
/// (capability, package), all under the capability's name, each mapping back to
/// the real package and carrying that package's dependencies. To keep the
/// solvable count bounded we only materialize capabilities that are actually
/// required by something (`required`) — the vast majority of provides/files are
/// never depended upon.
fn build<S: CandidateSource + ?Sized>(
    source: &S,
    installed_provides: &[(String, Option<Evr>)],
    required: &HashSet<String>,
    include_recommends: bool,
    providable: &HashSet<String>,
) -> RpmProvider {
    let pool = Pool::<Ranges<Evr>>::new();
    let mut providers: HashMap<NameId, Vec<SolvableId>> = HashMap::new();
    let mut deps: HashMap<SolvableId, Vec<ConditionalRequirement>> = HashMap::new();
    let mut candidate_of: HashMap<SolvableId, usize> = HashMap::new();
    let mut installed: HashSet<SolvableId> = HashSet::new();
    let mut pkg_evr: HashMap<SolvableId, Evr> = HashMap::new();
    let mut wildcard: HashSet<SolvableId> = HashSet::new();
    let mut conditions: Vec<Condition> = Vec::new();
    let mut vsdep: HashMap<VersionSetId, Dep> = HashMap::new();
    // Installed capability names, for pre-evaluating rich `if`/`unless`.
    let installed_names: HashSet<String> =
        installed_provides.iter().map(|(n, _)| n.clone()).collect();

    source.scan(required, &mut |c| {
        let ci = c.id;
        // Build the package's requirements once; every provider solvable for
        // this package shares them, so selecting the package via *any*
        // capability pulls its dependencies.
        let mut reqs: Vec<ConditionalRequirement> = Vec::new();
        // Hard requires, including rich/boolean deps (parsed into conditional
        // requirements / unions).
        for r in c.requires {
            if is_ignorable_dep(&r.name) {
                continue;
            }
            if richdep::is_rich(&r.name) {
                if let Some(expr) = richdep::parse_rich(&r.name) {
                    emit_rich(
                        &pool,
                        &mut conditions,
                        &mut vsdep,
                        &installed_names,
                        &expr,
                        None,
                        &mut reqs,
                    );
                }
                continue;
            }
            let cap = pool.intern_package_name(r.name.clone());
            reqs.push(ConditionalRequirement::from(version_set(
                &pool, &mut vsdep, cap, r,
            )));
        }
        // Weak deps (Recommends): simple, providable ones, installed as if
        // required (dnf default). Rich recommends are rare and skipped.
        if include_recommends {
            for r in c.recommends {
                if is_ignorable_dep(&r.name) || richdep::is_rich(&r.name) {
                    continue;
                }
                if providable.contains(&r.name) {
                    let cap = pool.intern_package_name(r.name.clone());
                    reqs.push(ConditionalRequirement::from(version_set(
                        &pool, &mut vsdep, cap, r,
                    )));
                }
            }
        }

        // Capabilities this package offers: its own name (versioned, = package
        // EVR) plus every Provides / file. A Provides with no version is
        // unversioned and matches any require (tracked as a wildcard).
        let mut caps: Vec<(&str, Option<&Evr>)> = Vec::with_capacity(c.provides.len() + 1);
        caps.push((c.name, Some(&c.evr)));
        for p in c.provides {
            caps.push((p.name.as_str(), p.evr.as_ref()));
        }

        let mut done: HashSet<&str> = HashSet::new();
        for (capname, prov_evr) in caps {
            if !required.contains(capname) || !done.insert(capname) {
                continue;
            }
            // One provider solvable per (package, capability), interned under
            // the capability name so all providers of a capability share it.
            // The record is the provided version (or the package version as a
            // placeholder for unversioned provides, which are flagged wildcard).
            let cap = pool.intern_package_name(capname.to_string());
            let rec = prov_evr.cloned().unwrap_or_else(|| c.evr.clone());
            let sid = pool.intern_solvable(cap, rec);
            candidate_of.insert(sid, ci);
            pkg_evr.insert(sid, c.evr.clone());
            deps.insert(sid, reqs.clone());
            if prov_evr.is_none() {
                wildcard.insert(sid);
            }
            providers.entry(cap).or_default().push(sid);
        }
    });

    // Installed capabilities as preferred, dependency-free synthetic solvables
    // (only for capabilities that are required, and matching the same-name rule
    // since they are interned under the capability name).
    for (name, evr) in installed_provides {
        if !required.contains(name) {
            continue;
        }
        let cap = pool.intern_package_name(name.clone());
        let rec = evr.clone().unwrap_or_else(|| Evr::new(Some(0), "0", ""));
        let sid = pool.intern_solvable(cap, rec.clone());
        installed.insert(sid);
        pkg_evr.insert(sid, rec);
        deps.insert(sid, Vec::new());
        // An installed capability with no version (e.g. a file) matches any require.
        if evr.is_none() {
            wildcard.insert(sid);
        }
        providers.entry(cap).or_default().push(sid);
    }

    RpmProvider {
        pool,
        providers,
        deps,
        installed,
        candidate_of,
        pkg_evr,
        wildcard,
        conditions,
        vsdep,
    }
}

impl Interner for RpmProvider {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, solvable: SolvableId) -> impl Display + '_ {
        let s = self.pool.resolve_solvable(solvable);
        format!("{}-{}", self.pool.resolve_package_name(s.name), s.record)
    }
    fn display_name(&self, name: NameId) -> impl Display + '_ {
        self.pool.resolve_package_name(name).clone()
    }
    fn display_version_set(&self, version_set: VersionSetId) -> impl Display + '_ {
        format!("{:?}", self.pool.resolve_version_set(version_set))
    }
    fn display_string(&self, string_id: StringId) -> impl Display + '_ {
        self.pool.resolve_string(string_id).to_string()
    }
    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.pool.resolve_version_set_package_name(version_set)
    }
    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.pool.resolve_solvable(solvable).name
    }
    fn version_sets_in_union(
        &self,
        union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        self.pool.resolve_version_set_union(union)
    }
    fn resolve_condition(&self, condition: ConditionId) -> Condition {
        self.conditions[condition.as_u32() as usize].clone()
    }
}

impl DependencyProvider for RpmProvider {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let dep = self.vsdep.get(&version_set);
        candidates
            .iter()
            .copied()
            .filter(|s| {
                // An unversioned provide (wildcard) matches any require (RPM's
                // rpmdsCompare). For versioned requires, use the originating
                // Dep's partial-EVR comparison (so `= 15.0.7` with no release
                // matches `15.0.7-3.amzn2023.0.4`); this mirrors the greedy
                // resolver and RPM exactly. Unversioned requires (no stored
                // Dep) match on name alone, which membership already implies.
                let matches = self.wildcard.contains(s)
                    || match dep {
                        Some(d) => {
                            let rec = &self.pool.resolve_solvable(*s).record;
                            d.satisfied_by(&d.name, Some(rec))
                        }
                        None => true,
                    };
                matches != inverse
            })
            .collect()
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        let list = self.providers.get(&name)?;
        Some(Candidates {
            candidates: list.clone(),
            ..Candidates::default()
        })
    }

    async fn sort_candidates(&self, _solver: &SolverCache<Self>, solvables: &mut [SolvableId]) {
        // Prefer already-installed solvables, then the highest *package* EVR
        // (not the solvable record, which for a capability provider is the
        // provided version and can tie across different package builds).
        let zero = Evr::new(Some(0), "0", "");
        solvables.sort_by(|a, b| {
            let ia = self.installed.contains(a);
            let ib = self.installed.contains(b);
            ib.cmp(&ia).then_with(|| {
                let ra = self.pkg_evr.get(a).unwrap_or(&zero);
                let rb = self.pkg_evr.get(b).unwrap_or(&zero);
                rb.cmp(ra)
            })
        });
    }

    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        Dependencies::Known(KnownDependencies {
            requirements: self.deps.get(&solvable).cloned().unwrap_or_default(),
            constrains: Vec::new(),
        })
    }
}

/// Resolve `requested` package specs against `candidates`, treating
/// `installed_provides` as already satisfied. Uses the resolvo SAT solver.
pub fn resolve_sat(
    requested: &[String],
    candidates: &[Candidate],
    installed_provides: &[(String, Option<Evr>)],
) -> Result<Resolved, ResolveError> {
    resolve_sat_with(requested, candidates, installed_provides)
}

/// Resolve against a [`CandidateSource`] (e.g. rum-repo's zero-copy views), so
/// the full package set need never be materialized as owned `Candidate`s.
pub fn resolve_sat_with<S: CandidateSource + ?Sized>(
    requested: &[String],
    source: &S,
    installed_provides: &[(String, Option<Evr>)],
) -> Result<Resolved, ResolveError> {
    // Single cheap name-only pass gathering everything the pre-solve sets need:
    // requested package names, the providable set, the hard-required capability
    // names (incl. those named in rich exprs), and the raw recommend names.
    // Walking full `Dep`s here (as the pool build does) is what dominated
    // resolve time on large repos, so this pass stays in borrowed `&str`.
    let mut requested_names: Vec<String> = Vec::new();
    let mut found = vec![false; requested.len()];
    let mut providable: HashSet<String> = HashSet::new();
    let mut hard_required: HashSet<String> = HashSet::new();
    let mut recommend_names: HashSet<String> = HashSet::new();
    source.scan_names(&mut |c| {
        for (i, spec) in requested.iter().enumerate() {
            if !found[i] && (c.name == spec || format!("{}.{}", c.name, c.arch) == *spec) {
                found[i] = true;
                if !requested_names.iter().any(|n| n == c.name) {
                    requested_names.push(c.name.to_string());
                }
            }
        }
        // Providable: package names + non-file provides (files never back a
        // weak dep and would balloon this set on RHEL-scale metadata).
        providable.insert(c.name.to_string());
        for p in c.provide_names {
            if !p.starts_with('/') {
                providable.insert(p.to_string());
            }
        }
        for r in c.require_names {
            if is_ignorable_dep(r) {
                continue;
            }
            if richdep::is_rich(r) {
                if let Some(expr) = richdep::parse_rich(r) {
                    collect_rich_names(&expr, &mut hard_required);
                }
                continue;
            }
            hard_required.insert(r.to_string());
        }
        for r in c.recommend_names {
            if !is_ignorable_dep(r) && !richdep::is_rich(r) {
                recommend_names.insert(r.to_string());
            }
        }
    });
    // A requested spec need not be a package name: it may be a capability
    // (e.g. a comps group lists `pkgconfig`, provided by `pkgconf-pkg-config`).
    // If no package name matched but something provides it, use the capability
    // as the root requirement so resolvo picks a provider (like `dnf install
    // <capability>`).
    for (i, spec) in requested.iter().enumerate() {
        if !found[i] && providable.contains(spec.as_str()) {
            found[i] = true;
            if !requested_names.iter().any(|n| n == spec) {
                requested_names.push(spec.clone());
            }
        }
    }
    for (i, spec) in requested.iter().enumerate() {
        if !found[i] {
            return Err(ResolveError::NotFound(spec.clone()));
        }
    }
    for (name, _) in installed_provides {
        providable.insert(name.clone());
    }

    // Try with weak dependencies (dnf's default); if that makes the set
    // unsolvable, retry hard-only so weak deps never cause a failure.
    match attempt(
        requested,
        &requested_names,
        source,
        installed_provides,
        &providable,
        &hard_required,
        &recommend_names,
        true,
    ) {
        Err(ResolveError::Unsatisfied { .. }) => attempt(
            requested,
            &requested_names,
            source,
            installed_provides,
            &providable,
            &hard_required,
            &recommend_names,
            false,
        ),
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
fn attempt<S: CandidateSource + ?Sized>(
    requested: &[String],
    requested_names: &[String],
    source: &S,
    installed_provides: &[(String, Option<Evr>)],
    providable: &HashSet<String>,
    hard_required: &HashSet<String>,
    recommend_names: &HashSet<String>,
    include_recommends: bool,
) -> Result<Resolved, ResolveError> {
    // Capabilities to materialize: everything hard-required, the requested
    // names, and (when on) the satisfiable Recommends. Assembled from the
    // pre-pass sets — no extra scan.
    let mut required: HashSet<String> = hard_required.clone();
    for n in requested_names {
        required.insert(n.clone());
    }
    if include_recommends {
        for r in recommend_names {
            if providable.contains(r) {
                required.insert(r.clone());
            }
        }
    }

    let provider = build(
        source,
        installed_provides,
        &required,
        include_recommends,
        providable,
    );

    // Root requirements: each requested package, at any version.
    let mut root = Vec::new();
    for name in requested_names {
        let cap = provider.pool.intern_package_name(name.clone());
        let vs = provider.pool.intern_version_set(cap, Ranges::full());
        root.push(ConditionalRequirement::from(vs));
    }

    // Retain the maps needed to interpret the solution before moving provider.
    let candidate_of = provider.candidate_of.clone();
    let installed = provider.installed.clone();

    let mut solver = Solver::new(provider);
    let solved = solver
        .solve(Problem::new().requirements(root))
        .map_err(|e| match e {
            UnsolvableOrCancelled::Unsolvable(_) => ResolveError::Unsatisfied {
                package: requested.join(", "),
                requirement: "unsatisfiable dependency set".to_string(),
            },
            UnsolvableOrCancelled::Cancelled(_) => ResolveError::Unsatisfied {
                package: requested.join(", "),
                requirement: "resolution cancelled".to_string(),
            },
        })?;

    // Map chosen solvables back to candidate ids, skipping installed synthetics.
    let mut to_install = Vec::new();
    for s in solved {
        if installed.contains(&s) {
            continue;
        }
        if let Some(&ci) = candidate_of.get(&s) {
            if !to_install.contains(&ci) {
                to_install.push(ci);
            }
        }
    }
    Ok(Resolved { to_install })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{assert_installs, assert_unsolvable, TestRepo};

    fn dep(name: &str) -> Dep {
        Dep::unversioned(name)
    }
    fn cand(id: usize, name: &str, ver: &str, provides: &[&str], requires: &[Dep]) -> Candidate {
        Candidate {
            id,
            name: name.into(),
            arch: "x86_64".into(),
            evr: Evr::new(Some(0), ver, "1"),
            provides: provides.iter().map(|p| Dep::unversioned(*p)).collect(),
            requires: requires.to_vec(),
            recommends: Vec::new(),
        }
    }

    #[test]
    fn resolves_transitive_chain() {
        let mut r = TestRepo::new();
        r.pkg("app-1.0-1.x86_64").requires("libb.so");
        r.pkg("libb-1.0-1.x86_64")
            .provides("libb.so")
            .requires("libc.so");
        r.pkg("libc-1.0-1.x86_64").provides("libc.so");
        assert_installs(
            &r,
            &["app"],
            &["app-1.0-1.x86_64", "libb-1.0-1.x86_64", "libc-1.0-1.x86_64"],
        );
    }

    #[test]
    fn prunes_installed() {
        let mut r = TestRepo::new();
        r.pkg("app-1.0-1.x86_64").requires("libc.so");
        r.pkg("libc-1.0-1.x86_64").provides("libc.so");
        r.installed("libc.so"); // already provided by the system
        assert_installs(&r, &["app"], &["app-1.0-1.x86_64"]);
    }

    #[test]
    fn backtracks_when_newest_provider_is_a_dead_end() {
        // `cap` has two providers: prov-2 (newest) needs `missing` (unsatisfiable),
        // prov-1 (older) is self-contained. A greedy "newest wins" picks prov-2
        // and fails; a backtracking solver must fall back to prov-1.
        let mut r = TestRepo::new();
        r.pkg("app-1.0-1.x86_64").requires("cap");
        r.pkg("prov-1.0-1.x86_64").provides("cap");
        r.pkg("prov-2.0-1.x86_64")
            .provides("cap")
            .requires("missing");
        assert_installs(&r, &["app"], &["app-1.0-1.x86_64", "prov-1.0-1.x86_64"]);
    }

    #[test]
    fn unversioned_provide_satisfies_versioned_require() {
        // RPM rule: a versionless `Provides: webserver` satisfies
        // `Requires: webserver >= 5.0`, even though the package is v3.0.
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").requires("webserver >= 5.0");
        r.pkg("prov-3.0-1.x86_64").provides("webserver");
        assert_installs(&r, &["app"], &["app-1-1.x86_64", "prov-3.0-1.x86_64"]);
    }

    #[test]
    fn unsatisfiable_require_fails() {
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").requires("nonexistent");
        assert_unsolvable(&r, &["app"]);
    }

    #[test]
    fn recommends_pulled_when_satisfiable() {
        // rum installs Recommends by default (dnf install_weak_deps=1).
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").recommends("extra");
        r.pkg("extra-1-1.x86_64").provides("extra");
        assert_installs(&r, &["app"], &["app-1-1.x86_64", "extra-1-1.x86_64"]);
    }

    #[test]
    fn recommends_dropped_when_unsatisfiable() {
        // A missing weak dep must not fail the transaction — app still installs.
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").recommends("absent");
        assert_installs(&r, &["app"], &["app-1-1.x86_64"]);
    }

    // A1 (see [[rum-upstream-research]]): RPM's dependency-overlap rule compares
    // epoch ONLY when both sides carry one. Require `bash >= 2:5.0` vs Provide
    // `bash = 5.2` (no epoch) -> RPM/dnf skip the epoch and 5.2 >= 5.0 satisfies.
    #[test]
    fn epoch_skipped_in_overlap_when_provide_has_none() {
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").requires("bash >= 2:5.0");
        r.pkg("bash-5.2-1.x86_64").provides("bash = 5.2");
        assert_installs(&r, &["app"], &["app-1-1.x86_64", "bash-5.2-1.x86_64"]);
    }

    #[test]
    fn version_locked_arch_qualified_eq_requires() {
        // Reproduces the deep -devel pattern (clang-devel, postgresql*-devel):
        // a package with EQ requires on ARCH-QUALIFIED capabilities that other
        // packages provide versioned, all pinned to one version.
        let evr = |v: &str| Evr::new(Some(0), v, "1");
        let vprov = |name: &str, v: &str| Dep {
            name: name.into(),
            flag: DepFlag::Eq,
            evr: Some(evr(v)),
        };
        // app Requires: lib(x86-64) = 1.0 AND tool(x86-64) = 1.0
        let app = Candidate {
            id: 0,
            name: "app".into(),
            arch: "x86_64".into(),
            evr: evr("1.0"),
            provides: vec![],
            requires: vec![vprov("lib(x86-64)", "1.0"), vprov("tool(x86-64)", "1.0")],
            recommends: vec![],
        };
        // lib and tool each carry a versioned arch-qualified provide.
        let lib = Candidate {
            id: 1,
            name: "lib".into(),
            arch: "x86_64".into(),
            evr: evr("1.0"),
            provides: vec![vprov("lib(x86-64)", "1.0")],
            requires: vec![],
            recommends: vec![],
        };
        let tool = Candidate {
            id: 2,
            name: "tool".into(),
            arch: "x86_64".into(),
            evr: evr("1.0"),
            provides: vec![vprov("tool(x86-64)", "1.0")],
            requires: vec![],
            recommends: vec![],
        };
        let r = resolve_sat(&["app".into()], &[app, lib, tool], &[])
            .unwrap()
            .to_install;
        assert!(
            r.contains(&1) && r.contains(&2),
            "arch-qualified EQ provides must resolve"
        );
    }

    #[test]
    fn rich_if_pulls_dep_when_condition_installed() {
        // app Requires: (extra if trigger). Mirrors mariadb's
        // (mysql-selinux if selinux-policy-targeted).
        let mut app = cand(0, "app", "1.0", &[], &[]);
        app.requires = vec![Dep::unversioned("(extra if trigger)")];
        let extra = cand(1, "extra", "1.0", &["extra"], &[]);

        // trigger installed -> extra should be pulled.
        let installed = vec![("trigger".to_string(), None)];
        let r = resolve_sat(&["app".into()], &[app.clone(), extra.clone()], &installed)
            .unwrap()
            .to_install;
        assert!(r.contains(&1), "extra pulled because trigger is installed");

        // trigger absent -> extra not pulled (condition false).
        let r2 = resolve_sat(&["app".into()], &[app, extra], &[])
            .unwrap()
            .to_install;
        assert!(!r2.contains(&1), "extra not pulled when trigger absent");
    }

    #[test]
    fn rich_or_resolves_via_either_provider() {
        let mut app = cand(0, "app", "1.0", &[], &[]);
        app.requires = vec![Dep::unversioned("(webA or webB)")];
        let weba = cand(1, "webA", "1.0", &["webA"], &[]);
        let r = resolve_sat(&["app".into()], &[app, weba], &[])
            .unwrap()
            .to_install;
        assert!(r.contains(&1), "OR satisfied by the available provider");
    }

    #[test]
    fn eq_require_without_release_matches_any_release() {
        // Reproduces the clang-libs gap: `Requires: cap = 15.0.7` (no release)
        // must match a provider advertising `cap = 15.0.7-3.amzn2023.0.4`.
        let app = Candidate {
            id: 0,
            name: "app".into(),
            arch: "x86_64".into(),
            evr: Evr::new(Some(0), "1.0", "1"),
            provides: vec![],
            requires: vec![Dep {
                name: "cap".into(),
                flag: DepFlag::Eq,
                evr: Some(Evr::new(Some(0), "15.0.7", "")), // version only, no release
            }],
            recommends: vec![],
        };
        let prov = Candidate {
            id: 1,
            name: "prov".into(),
            arch: "x86_64".into(),
            evr: Evr::new(Some(0), "15.0.7", "3.amzn2023.0.4"),
            provides: vec![Dep {
                name: "cap".into(),
                flag: DepFlag::Eq,
                evr: Some(Evr::new(Some(0), "15.0.7", "3.amzn2023.0.4")),
            }],
            requires: vec![],
            recommends: vec![],
        };
        let r = resolve_sat(&["app".into()], &[app, prov], &[])
            .unwrap()
            .to_install;
        assert!(
            r.contains(&1),
            "EQ without release must match any release of that version"
        );
    }

    #[test]
    fn unsatisfiable_errors() {
        let cands = vec![cand(0, "app", "1.0", &[], &[dep("nope")])];
        assert!(resolve_sat(&["app".into()], &cands, &[]).is_err());
    }
}

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
fn build_cond(pool: &Pool<Ranges<Evr>>, conds: &mut Vec<Condition>, e: &RichExpr) -> ConditionId {
    match e {
        RichExpr::And(a, b) => {
            let ca = build_cond(pool, conds, a);
            let cb = build_cond(pool, conds, b);
            mint(conds, Condition::Binary(LogicalOperator::And, ca, cb))
        }
        RichExpr::Or(a, b) => {
            let ca = build_cond(pool, conds, a);
            let cb = build_cond(pool, conds, b);
            mint(conds, Condition::Binary(LogicalOperator::Or, ca, cb))
        }
        RichExpr::Term(d) => {
            let cap = pool.intern_package_name(d.name.clone());
            mint(conds, Condition::Requirement(version_set(pool, cap, d)))
        }
        // Rare: a compound condition; approximate with its leftmost term.
        RichExpr::If(a, _)
        | RichExpr::IfElse(a, _, _)
        | RichExpr::Unless(a, _)
        | RichExpr::UnlessElse(a, _, _)
        | RichExpr::With(a, _)
        | RichExpr::Without(a, _) => build_cond(pool, conds, a),
    }
}

/// Collect the version sets of all Term leaves (used to build an OR union).
fn collect_or_vss(pool: &Pool<Ranges<Evr>>, e: &RichExpr, out: &mut Vec<VersionSetId>) {
    match e {
        RichExpr::Term(d) => {
            let cap = pool.intern_package_name(d.name.clone());
            out.push(version_set(pool, cap, d));
        }
        RichExpr::And(a, b) | RichExpr::Or(a, b) => {
            collect_or_vss(pool, a, out);
            collect_or_vss(pool, b, out);
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
fn emit_rich(
    pool: &Pool<Ranges<Evr>>,
    conds: &mut Vec<Condition>,
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
                requirement: Requirement::Single(version_set(pool, cap, d)),
            });
        }
        RichExpr::And(a, b) => {
            emit_rich(pool, conds, installed, a, cond, out);
            emit_rich(pool, conds, installed, b, cond, out);
        }
        RichExpr::Or(..) => {
            let mut vss = Vec::new();
            collect_or_vss(pool, e, &mut vss);
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
                emit_rich(pool, conds, installed, then, cond, out);
            } else {
                let cid = build_cond(pool, conds, c);
                let combined = match cond {
                    None => cid,
                    Some(o) => mint(conds, Condition::Binary(LogicalOperator::And, o, cid)),
                };
                emit_rich(pool, conds, installed, then, Some(combined), out);
            }
        }
        // `then if cond else els`: pick the branch by installed state.
        RichExpr::IfElse(then, c, els) => {
            let branch = if cond_installed(c, installed) {
                then
            } else {
                els
            };
            emit_rich(pool, conds, installed, branch, cond, out);
        }
        // `body unless cond`: require body unless cond is present.
        RichExpr::Unless(body, c) => {
            if !cond_installed(c, installed) {
                emit_rich(pool, conds, installed, body, cond, out);
            }
        }
        RichExpr::UnlessElse(body, c, els) => {
            let branch = if cond_installed(c, installed) {
                els
            } else {
                body
            };
            emit_rich(pool, conds, installed, branch, cond, out);
        }
        // `with`/`without`: no intersection in resolvo; require the primary
        // operand (approximation, documented).
        RichExpr::With(a, _) | RichExpr::Without(a, _) => {
            emit_rich(pool, conds, installed, a, cond, out)
        }
    }
}

fn version_set(pool: &Pool<Ranges<Evr>>, cap: NameId, dep: &Dep) -> VersionSetId {
    let ranges = match (&dep.evr, dep.flag) {
        (None, _) | (_, DepFlag::Any) => Ranges::full(),
        (Some(e), DepFlag::Eq) => Ranges::singleton(e.clone()),
        (Some(e), DepFlag::Lt) => Ranges::strictly_lower_than(e.clone()),
        (Some(e), DepFlag::Le) => Ranges::lower_than(e.clone()),
        (Some(e), DepFlag::Gt) => Ranges::strictly_higher_than(e.clone()),
        (Some(e), DepFlag::Ge) => Ranges::higher_than(e.clone()),
    };
    pool.intern_version_set(cap, ranges)
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
fn build(
    candidates: &[Candidate],
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
    // Installed capability names, for pre-evaluating rich `if`/`unless`.
    let installed_names: HashSet<String> =
        installed_provides.iter().map(|(n, _)| n.clone()).collect();

    for (ci, c) in candidates.iter().enumerate() {
        // Build the package's requirements once; every provider solvable for
        // this package shares them, so selecting the package via *any*
        // capability pulls its dependencies.
        let mut reqs: Vec<ConditionalRequirement> = Vec::new();
        // Hard requires, including rich/boolean deps (parsed into conditional
        // requirements / unions).
        for r in &c.requires {
            if is_ignorable_dep(&r.name) {
                continue;
            }
            if richdep::is_rich(&r.name) {
                if let Some(expr) = richdep::parse_rich(&r.name) {
                    emit_rich(
                        &pool,
                        &mut conditions,
                        &installed_names,
                        &expr,
                        None,
                        &mut reqs,
                    );
                }
                continue;
            }
            let cap = pool.intern_package_name(r.name.clone());
            reqs.push(ConditionalRequirement::from(version_set(&pool, cap, r)));
        }
        // Weak deps (Recommends): simple, providable ones, installed as if
        // required (dnf default). Rich recommends are rare and skipped.
        if include_recommends {
            for r in &c.recommends {
                if is_ignorable_dep(&r.name) || richdep::is_rich(&r.name) {
                    continue;
                }
                if providable.contains(&r.name) {
                    let cap = pool.intern_package_name(r.name.clone());
                    reqs.push(ConditionalRequirement::from(version_set(&pool, cap, r)));
                }
            }
        }

        // Capabilities this package offers: its own name (versioned, = package
        // EVR) plus every Provides / file. A Provides with no version is
        // unversioned and matches any require (tracked as a wildcard).
        let mut caps: Vec<(&str, Option<&Evr>)> = Vec::with_capacity(c.provides.len() + 1);
        caps.push((c.name.as_str(), Some(&c.evr)));
        for p in &c.provides {
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
    }

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
        let ranges = self.pool.resolve_version_set(version_set);
        candidates
            .iter()
            .copied()
            .filter(|s| {
                // An unversioned provide (wildcard) matches any require, per
                // RPM's rpmdsCompare; otherwise range-check the provided version.
                let matches = self.wildcard.contains(s)
                    || ranges.contains(&self.pool.resolve_solvable(*s).record);
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
    // Resolve requested specs (name or name.arch) to package names up front.
    let mut requested_names = Vec::new();
    for spec in requested {
        let name = candidates
            .iter()
            .find(|c| c.name == *spec || format!("{}.{}", c.name, c.arch) == *spec)
            .map(|c| c.name.clone())
            .ok_or_else(|| ResolveError::NotFound(spec.clone()))?;
        if !requested_names.contains(&name) {
            requested_names.push(name);
        }
    }

    // Every capability that anything can provide (package names + provides +
    // files + installed). A Recommends is only pulled if its capability is in
    // here — otherwise it's silently dropped (matching dnf).
    let mut providable: HashSet<String> = HashSet::new();
    for c in candidates {
        providable.insert(c.name.clone());
        for p in &c.provides {
            providable.insert(p.name.clone());
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
        candidates,
        installed_provides,
        &providable,
        true,
    ) {
        Err(ResolveError::Unsatisfied { .. }) => attempt(
            requested,
            &requested_names,
            candidates,
            installed_provides,
            &providable,
            false,
        ),
        other => other,
    }
}

fn attempt(
    requested: &[String],
    requested_names: &[String],
    candidates: &[Candidate],
    installed_provides: &[(String, Option<Evr>)],
    providable: &HashSet<String>,
    include_recommends: bool,
) -> Result<Resolved, ResolveError> {
    // Capabilities to materialize: everything hard-required, the requested
    // names, and (when on) the satisfiable Recommends.
    let mut required: HashSet<String> = HashSet::new();
    for c in candidates {
        for r in &c.requires {
            if is_ignorable_dep(&r.name) {
                continue;
            }
            if richdep::is_rich(&r.name) {
                // Materialize the capabilities named inside the boolean expr.
                if let Some(expr) = richdep::parse_rich(&r.name) {
                    collect_rich_names(&expr, &mut required);
                }
                continue;
            }
            required.insert(r.name.clone());
        }
        if include_recommends {
            for r in &c.recommends {
                if !is_ignorable_dep(&r.name)
                    && !richdep::is_rich(&r.name)
                    && providable.contains(&r.name)
                {
                    required.insert(r.name.clone());
                }
            }
        }
    }
    for n in requested_names {
        required.insert(n.clone());
    }

    let provider = build(
        candidates,
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
        let cands = vec![
            cand(0, "app", "1.0", &[], &[dep("libb.so")]),
            cand(1, "libb", "1.0", &["libb.so"], &[dep("libc.so")]),
            cand(2, "libc", "1.0", &["libc.so"], &[]),
        ];
        let mut r = resolve_sat(&["app".into()], &cands, &[])
            .unwrap()
            .to_install;
        r.sort();
        assert_eq!(r, vec![0, 1, 2]);
    }

    #[test]
    fn prunes_installed() {
        let cands = vec![
            cand(0, "app", "1.0", &[], &[dep("libc.so")]),
            cand(1, "libc", "1.0", &["libc.so"], &[]),
        ];
        let installed = vec![("libc.so".to_string(), None)];
        let r = resolve_sat(&["app".into()], &cands, &installed)
            .unwrap()
            .to_install;
        assert_eq!(r, vec![0]); // libc.so already provided by system
    }

    #[test]
    fn backtracks_when_newest_provider_is_a_dead_end() {
        // `cap` has two providers: prov-2 (newest) needs `missing` (unsatisfiable),
        // prov-1 (older) is self-contained. A greedy "newest wins" picks prov-2
        // and fails; a backtracking solver must fall back to prov-1.
        let cands = vec![
            cand(0, "app", "1.0", &[], &[dep("cap")]),
            cand(1, "prov", "1.0", &["cap"], &[]),
            cand(2, "prov", "2.0", &["cap"], &[dep("missing")]),
        ];
        let r = resolve_sat(&["app".into()], &cands, &[])
            .unwrap()
            .to_install;
        assert!(r.contains(&0), "app selected");
        assert!(r.contains(&1), "fell back to prov-1.0");
        assert!(!r.contains(&2), "prov-2.0 (dead end) not selected");
    }

    #[test]
    fn unversioned_provide_satisfies_versioned_require() {
        // RPM rule: a versionless `Provides: webserver` satisfies
        // `Requires: webserver >= 5.0`, even though the package is v3.0.
        let app = Candidate {
            id: 0,
            name: "app".into(),
            arch: "x86_64".into(),
            evr: Evr::new(Some(0), "1", "1"),
            provides: vec![],
            requires: vec![Dep {
                name: "webserver".into(),
                flag: DepFlag::Ge,
                evr: Some(Evr::new(Some(0), "5.0", "")),
            }],
            recommends: vec![],
        };
        let prov = Candidate {
            id: 1,
            name: "prov".into(),
            arch: "x86_64".into(),
            evr: Evr::new(Some(0), "3.0", "1"), // package older than the require
            provides: vec![Dep::unversioned("webserver")], // unversioned provide
            requires: vec![],
            recommends: vec![],
        };
        let r = resolve_sat(&["app".into()], &[app, prov], &[])
            .unwrap()
            .to_install;
        assert!(
            r.contains(&1),
            "unversioned provide must satisfy a versioned require"
        );
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
    fn unsatisfiable_errors() {
        let cands = vec![cand(0, "app", "1.0", &[], &[dep("nope")])];
        assert!(resolve_sat(&["app".into()], &cands, &[]).is_err());
    }
}

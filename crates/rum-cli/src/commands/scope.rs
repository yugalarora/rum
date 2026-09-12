//! Scoped-present resolver support: pick the minimal set of INSTALLED packages
//! that a transaction must reason about, so the SAT solver stays small (no
//! whole-rpmdb injection) yet still gets passive conflicts/obsoletes right.
//!
//! A transaction only needs an installed package in the solver if it is:
//!   1. TARGETED — a winner `Conflicts:`/`Obsoletes:` it (matched by the
//!      installed package's name OR any of its `Provides:`, so `Conflicts: MTA`
//!      pulls installed `postfix`), or
//!   2. a DEPENDENT — any installed package that consumes a scoped package's
//!      provides (transitively). We pull in *every* consumer, not only those
//!      for which the scoped package is the sole provider: whether a dependent
//!      can keep its requirement satisfied by an alternative (or must itself be
//!      erased, or makes the transaction unsolvable) is the SAT solver's call,
//!      not ours — omitting it would let the solver erase a package and silently
//!      break its dependents on disk.

use std::collections::{HashMap, HashSet};

use rum_rpm::InstalledPkg;

/// Indices into `installed` of the scoped set for the given target capabilities
/// (the names a transaction's winners Conflict-with or Obsolete). Includes the
/// matched targets plus the transitive closure of their dependents.
pub fn scope_installed(target_caps: &HashSet<String>, installed: &[InstalledPkg]) -> Vec<usize> {
    // capability name -> installed indices that REQUIRE it (for cascade).
    let mut consumers: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, p) in installed.iter().enumerate() {
        for r in &p.requires {
            consumers.entry(r.as_str()).or_default().push(i);
        }
    }

    let mut in_scope = vec![false; installed.len()];
    let mut queue: Vec<usize> = Vec::new();

    // Seed: installed packages a winner conflicts-with / obsoletes, matched by
    // package name OR any provided capability (virtual-provides matching).
    for (i, p) in installed.iter().enumerate() {
        let hit =
            target_caps.contains(&p.name) || p.provides.iter().any(|pv| target_caps.contains(pv));
        if hit && !in_scope[i] {
            in_scope[i] = true;
            queue.push(i);
        }
    }

    // Cascade: any installed consumer of a scoped package's provides (its name
    // included) joins the scope, to a fixpoint.
    let mut head = 0;
    while head < queue.len() {
        let i = queue[head];
        head += 1;
        let caps = std::iter::once(installed[i].name.as_str())
            .chain(installed[i].provides.iter().map(String::as_str));
        for cap in caps {
            if let Some(deps) = consumers.get(cap) {
                for &di in deps {
                    if !in_scope[di] {
                        in_scope[di] = true;
                        queue.push(di);
                    }
                }
            }
        }
    }

    (0..installed.len()).filter(|&i| in_scope[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, provides: &[&str], requires: &[&str]) -> InstalledPkg {
        InstalledPkg {
            name: name.into(),
            evr: "1-1".into(),
            provides: provides.iter().map(|s| s.to_string()).collect(),
            requires: requires.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn scopes_target_by_name() {
        let inst = vec![pkg("A", &[], &[]), pkg("B", &[], &[])];
        let caps: HashSet<String> = ["A".to_string()].into_iter().collect();
        assert_eq!(scope_installed(&caps, &inst), vec![0]);
    }

    #[test]
    fn scopes_target_by_virtual_provide() {
        // Conflicts: MTA -> installed postfix (provides MTA) is scoped.
        let inst = vec![
            pkg("postfix", &["MTA", "smtpd"], &[]),
            pkg("unrelated", &[], &[]),
        ];
        let caps: HashSet<String> = ["MTA".to_string()].into_iter().collect();
        assert_eq!(scope_installed(&caps, &inst), vec![0]);
    }

    #[test]
    fn cascades_to_any_consumer_not_just_sole_provider() {
        // B (target) provides P. C requires P. Even though repo alt E could also
        // provide P, C must enter the scope so the solver handles B's erasure.
        let inst = vec![
            pkg("B", &["P"], &[]),     // target
            pkg("C", &[], &["P"]),     // consumer of P
            pkg("D", &[], &["C"]),     // consumer of C (transitive)
            pkg("Z", &[], &["other"]), // unrelated
        ];
        let caps: HashSet<String> = ["B".to_string()].into_iter().collect();
        assert_eq!(scope_installed(&caps, &inst), vec![0, 1, 2]);
    }

    #[test]
    fn empty_when_no_target() {
        let inst = vec![pkg("A", &[], &[]), pkg("B", &[], &["A"])];
        let caps: HashSet<String> = ["nonexistent".to_string()].into_iter().collect();
        assert!(scope_installed(&caps, &inst).is_empty());
    }
}

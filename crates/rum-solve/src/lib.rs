//! Version and dependency logic for rum.
//!
//! Milestone 1 (this file): RPM version comparison (`rpmvercmp`) and full
//! epoch:version-release (EVR) comparison, plus a helper to pick the newest
//! candidate. These back `check-update`, latest-only listing, and upgrade
//! decisions.
//!
//! SAT dependency resolution over repo `provides`/`requires` is provided by
//! [`resolve_sat`] (backed by the `resolvo` crate); [`resolve`] is the simpler
//! greedy resolver kept as a fallback and for reference.

mod dep;
mod resolve;
mod sat;
mod vercmp;

use std::cmp::Ordering;

pub use dep::{Dep, DepFlag};
pub use resolve::{resolve, Candidate, ResolveError, Resolved};
pub use sat::resolve_sat;
pub use vercmp::rpmvercmp;

/// An epoch:version-release tuple, RPM's unit of "which build is newer".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Evr {
    /// A missing epoch is treated as 0 (RPM/dnf convention).
    pub epoch: u64,
    pub version: String,
    pub release: String,
}

impl Evr {
    pub fn new(epoch: Option<u64>, version: impl Into<String>, release: impl Into<String>) -> Self {
        Evr {
            epoch: epoch.unwrap_or(0),
            version: version.into(),
            release: release.into(),
        }
    }

    /// Parse an EVR string of the form `[epoch:]version[-release]`, as stored
    /// in the rpmdb / repo metadata dependency versions. A missing epoch is 0;
    /// a missing release is empty (which dep comparison then ignores).
    pub fn parse(s: &str) -> Self {
        let (epoch, rest) = match s.split_once(':') {
            Some((e, r)) => (e.parse::<u64>().unwrap_or(0), r),
            None => (0, s),
        };
        let (version, release) = match rest.split_once('-') {
            Some((v, r)) => (v.to_string(), r.to_string()),
            None => (rest.to_string(), String::new()),
        };
        Evr {
            epoch,
            version,
            release,
        }
    }

    /// RPM label comparison: epoch first (numerically), then version, then
    /// release, each with `rpmvercmp`.
    pub fn compare(&self, other: &Evr) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| rpmvercmp(&self.version, &other.version))
            .then_with(|| rpmvercmp(&self.release, &other.release))
    }
}

impl PartialOrd for Evr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Evr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.compare(other)
    }
}

impl std::fmt::Display for Evr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.epoch == 0 {
            write!(f, "{}-{}", self.version, self.release)
        } else {
            write!(f, "{}:{}-{}", self.epoch, self.version, self.release)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_dominates_version() {
        let a = Evr::new(Some(1), "1.0", "1");
        let b = Evr::new(Some(0), "9.9", "9");
        assert!(a > b, "higher epoch wins regardless of version");
    }

    #[test]
    fn missing_epoch_is_zero() {
        let a = Evr::new(None, "1.0", "1");
        let b = Evr::new(Some(0), "1.0", "1");
        assert_eq!(a, b);
    }

    #[test]
    fn version_then_release() {
        let a = Evr::new(Some(0), "1.2.3", "2.amzn2023");
        let b = Evr::new(Some(0), "1.2.3", "10.amzn2023");
        assert!(a < b, "release 10 newer than release 2");

        let c = Evr::new(Some(0), "1.2.4", "1");
        assert!(c > a, "newer version beats older even with lower release");
    }

    #[test]
    fn tilde_prerelease_is_older() {
        let pre = Evr::new(Some(0), "1.0~rc1", "1");
        let rel = Evr::new(Some(0), "1.0", "1");
        assert!(pre < rel);
    }

    #[test]
    fn parse_evr_forms() {
        assert_eq!(Evr::parse("2.34-1"), Evr::new(Some(0), "2.34", "1"));
        assert_eq!(
            Evr::parse("1:2.34-5.amzn2023"),
            Evr::new(Some(1), "2.34", "5.amzn2023")
        );
        assert_eq!(Evr::parse("2.34"), Evr::new(Some(0), "2.34", ""));
        assert_eq!(Evr::parse(""), Evr::new(Some(0), "", ""));
    }
}

//! Version and dependency logic for rum.
//!
//! Milestone 1 (this file): RPM version comparison (`rpmvercmp`) and full
//! epoch:version-release (EVR) comparison, plus a helper to pick the newest
//! candidate. These back `check-update`, latest-only listing, and upgrade
//! decisions.
//!
//! Milestone 2 (future): full SAT dependency resolution over repo
//! `provides`/`requires`, likely built on the `resolvo` crate.

mod vercmp;

use std::cmp::Ordering;

pub use vercmp::rpmvercmp;

/// An epoch:version-release tuple, RPM's unit of "which build is newer".
#[derive(Debug, Clone, PartialEq, Eq)]
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
        Some(self.compare(other))
    }
}

impl Ord for Evr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.compare(other)
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
}

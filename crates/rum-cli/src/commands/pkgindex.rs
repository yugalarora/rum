//! Shared helpers for reasoning about installed vs available versions.

use std::collections::HashMap;

use rum_repo::AvailablePackage;
use rum_rpm::{Package, Rpmdb};
use rum_solve::Evr;

/// EVR of an installed package.
pub fn installed_evr(p: &Package) -> Evr {
    Evr::new(p.epoch, p.version.clone(), p.release.clone())
}

/// EVR of an available package.
pub fn available_evr(p: &AvailablePackage) -> Evr {
    Evr::new(Some(p.epoch), p.version.clone(), p.release.clone())
}

/// Highest installed EVR per `name.arch` (packages can have several installed
/// versions, e.g. kernels; we keep the newest).
pub fn installed_best() -> HashMap<String, Evr> {
    let mut map: HashMap<String, Evr> = HashMap::new();
    if let Ok(db) = Rpmdb::open() {
        for p in db.installed() {
            let evr = installed_evr(&p);
            map.entry(p.name_arch())
                .and_modify(|cur| {
                    if evr > *cur {
                        *cur = evr.clone();
                    }
                })
                .or_insert(evr);
        }
    }
    map
}

/// Highest available package per `name.arch` across all synced repos.
pub fn available_best(packages: Vec<AvailablePackage>) -> HashMap<String, AvailablePackage> {
    let mut map: HashMap<String, AvailablePackage> = HashMap::new();
    for p in packages {
        let key = p.name_arch();
        match map.get(&key) {
            Some(existing) if available_evr(existing) >= available_evr(&p) => {}
            _ => {
                map.insert(key, p);
            }
        }
    }
    map
}

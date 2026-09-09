//! Read-only access to the installed-package database (`/var/lib/rpm`) via the
//! system librpm.
//!
//! This is deliberately read-only: rum never keeps its own notion of "what is
//! installed", it always asks the rpmdb. That is what lets rum coexist with
//! dnf/yum on the same host, both see the same installed set, because there is
//! only one database and librpm abstracts over its backend (sqlite on
//! AL2023/RHEL9, BerkeleyDB on AL2, ndb on SUSE).
//!
//! Writes (installing/erasing) are a separate, later concern and will use the
//! librpm transaction API; they are intentionally not exposed here.

#[cfg(target_os = "linux")]
mod ffi;

#[derive(Debug, thiserror::Error)]
pub enum RpmError {
    #[error("failed to initialize rpm configuration (rpmReadConfigFiles)")]
    ConfigInit,
    #[error("failed to create rpm transaction set")]
    TsCreate,
    #[error("librpm is not available on this platform (built without Linux librpm)")]
    Unsupported,
}

/// One installed package, as read from the rpmdb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    /// Epoch is `None` when the header has no epoch tag (distinct from `Some(0)`).
    pub epoch: Option<u64>,
    pub version: String,
    pub release: String,
    pub arch: String,
    pub summary: String,
    /// Installed size in bytes (RPMTAG_SIZE).
    pub size: u64,
    /// Unix time the package was installed (RPMTAG_INSTALLTIME).
    pub install_time: u64,
}

impl Package {
    /// `epoch:version-release` (epoch omitted when absent), the EVR string.
    pub fn evr(&self) -> String {
        match self.epoch {
            Some(e) => format!("{e}:{}-{}", self.version, self.release),
            None => format!("{}-{}", self.version, self.release),
        }
    }

    /// `name-[epoch:]version-release.arch` — the canonical NEVRA.
    pub fn nevra(&self) -> String {
        format!("{}-{}.{}", self.name, self.evr(), self.arch)
    }

    /// `name.arch` — how dnf labels a package in list output.
    pub fn name_arch(&self) -> String {
        format!("{}.{}", self.name, self.arch)
    }
}

/// A handle to the installed-package database.
pub struct Rpmdb {
    #[cfg(target_os = "linux")]
    ts: ffi::rpmts,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{ffi, Package, RpmError, Rpmdb};
    use std::ffi::{CStr, CString};
    use std::os::raw::c_void;
    use std::ptr;
    use std::sync::Once;

    static INIT: Once = Once::new();
    static mut INIT_OK: bool = false;

    fn ensure_config() -> Result<(), RpmError> {
        // SAFETY: rpmReadConfigFiles mutates process-global rpm state; guard it
        // behind a Once so it runs exactly once per process.
        INIT.call_once(|| unsafe {
            let rc = ffi::rpmReadConfigFiles(ptr::null(), ptr::null());
            // Not using addr_of_mut here keeps MSRV low; access is serialized
            // by Once so there is no data race.
            #[allow(static_mut_refs)]
            {
                INIT_OK = rc == 0;
            }
        });
        // SAFETY: only read after call_once has completed initialization.
        #[allow(static_mut_refs)]
        let ok = unsafe { INIT_OK };
        if ok {
            Ok(())
        } else {
            Err(RpmError::ConfigInit)
        }
    }

    /// Read a borrowed string tag into an owned `String` (empty if absent).
    unsafe fn get_string(h: ffi::Header, tag: ffi::rpmTagVal) -> String {
        let p = ffi::headerGetString(h, tag);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    unsafe fn header_to_package(h: ffi::Header) -> Package {
        let epoch = if ffi::headerIsEntry(h, ffi::RPMTAG_EPOCH) != 0 {
            Some(ffi::headerGetNumber(h, ffi::RPMTAG_EPOCH))
        } else {
            None
        };
        Package {
            name: get_string(h, ffi::RPMTAG_NAME),
            epoch,
            version: get_string(h, ffi::RPMTAG_VERSION),
            release: get_string(h, ffi::RPMTAG_RELEASE),
            arch: get_string(h, ffi::RPMTAG_ARCH),
            summary: get_string(h, ffi::RPMTAG_SUMMARY),
            size: ffi::headerGetNumber(h, ffi::RPMTAG_SIZE),
            install_time: ffi::headerGetNumber(h, ffi::RPMTAG_INSTALLTIME),
        }
    }

    impl Rpmdb {
        /// Open the system rpmdb at root `/`.
        pub fn open() -> Result<Self, RpmError> {
            ensure_config()?;
            // SAFETY: rpmtsCreate returns an owned transaction set or null.
            let ts = unsafe { ffi::rpmtsCreate() };
            if ts.is_null() {
                return Err(RpmError::TsCreate);
            }
            // SAFETY: ts is non-null; setting root "/" is the default but explicit.
            unsafe {
                let root = CString::new("/").unwrap();
                ffi::rpmtsSetRootDir(ts, root.as_ptr());
            }
            Ok(Rpmdb { ts })
        }

        /// All installed packages.
        pub fn installed(&self) -> Vec<Package> {
            self.iter_with(ffi::RPMDBI_PACKAGES, None)
        }

        /// Installed packages with the exact given name (a name may match more
        /// than one row, e.g. multilib i686 + x86_64, or multiple kernels).
        pub fn by_name(&self, name: &str) -> Vec<Package> {
            self.iter_with(ffi::RPMTAG_NAME, Some(name))
        }

        /// Is any package with this exact name installed?
        pub fn is_installed(&self, name: &str) -> bool {
            !self.by_name(name).is_empty()
        }

        /// The version string of a Provides capability, e.g. the version of
        /// `system-release(releasever)`. This is how dnf derives `$releasever`
        /// (its `distroverpkg`), rather than trusting /etc/os-release.
        pub fn provide_version(&self, provide: &str) -> Option<String> {
            let key = CString::new(provide).ok()?;
            // SAFETY: valid ts; iterator keyed by PROVIDENAME returns headers of
            // packages carrying that Provides. We read the first match.
            unsafe {
                let mi = ffi::rpmtsInitIterator(
                    self.ts,
                    ffi::RPMTAG_PROVIDENAME,
                    key.as_ptr() as *const c_void,
                    0,
                );
                if mi.is_null() {
                    return None;
                }
                let h = ffi::rpmdbNextIterator(mi);
                let out = if h.is_null() {
                    None
                } else {
                    provide_version_from_header(h, provide)
                };
                ffi::rpmdbFreeIterator(mi);
                out
            }
        }

        /// dnf-compatible `$releasever` detection.
        pub fn releasever(&self) -> Option<String> {
            self.provide_version("system-release(releasever)")
        }

        fn iter_with(&self, tag: ffi::rpmTagVal, key: Option<&str>) -> Vec<Package> {
            let ckey = key.map(|k| CString::new(k).unwrap());
            let (keyp, keylen) = match &ckey {
                Some(c) => (c.as_ptr() as *const c_void, 0usize),
                None => (ptr::null(), 0usize),
            };

            // SAFETY: self.ts is a valid transaction set for the lifetime of &self.
            let mi = unsafe { ffi::rpmtsInitIterator(self.ts, tag, keyp, keylen) };
            if mi.is_null() {
                return Vec::new();
            }

            let mut out = Vec::new();
            // SAFETY: mi is non-null; NextIterator yields headers owned by the
            // iterator, valid until the next call, so we copy immediately.
            unsafe {
                let hint = ffi::rpmdbGetIteratorCount(mi);
                if hint > 0 {
                    out.reserve(hint as usize);
                }
                loop {
                    let h = ffi::rpmdbNextIterator(mi);
                    if h.is_null() {
                        break;
                    }
                    out.push(header_to_package(h));
                }
                ffi::rpmdbFreeIterator(mi);
            }
            out
        }
    }

    /// Walk the parallel PROVIDENAME / PROVIDEVERSION arrays of a header and
    /// return the version paired with `target`.
    unsafe fn provide_version_from_header(h: ffi::Header, target: &str) -> Option<String> {
        let names = ffi::rpmtdNew();
        let vers = ffi::rpmtdNew();
        let mut out = None;

        if ffi::headerGet(h, ffi::RPMTAG_PROVIDENAME, names, 0) != 0
            && ffi::headerGet(h, ffi::RPMTAG_PROVIDEVERSION, vers, 0) != 0
        {
            ffi::rpmtdInit(names);
            loop {
                let idx = ffi::rpmtdNext(names);
                if idx < 0 {
                    break;
                }
                let np = ffi::rpmtdGetString(names);
                if np.is_null() {
                    continue;
                }
                let name = CStr::from_ptr(np).to_string_lossy();
                if name == target {
                    ffi::rpmtdSetIndex(vers, idx);
                    let vp = ffi::rpmtdGetString(vers);
                    if !vp.is_null() {
                        let v = CStr::from_ptr(vp).to_string_lossy().into_owned();
                        if !v.is_empty() {
                            out = Some(v);
                        }
                    }
                    break;
                }
            }
        }

        ffi::rpmtdFreeData(names);
        ffi::rpmtdFree(names);
        ffi::rpmtdFreeData(vers);
        ffi::rpmtdFree(vers);
        out
    }

    impl Drop for Rpmdb {
        fn drop(&mut self) {
            // SAFETY: ts was created by rpmtsCreate and not freed elsewhere.
            unsafe {
                ffi::rpmtsFree(self.ts);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl Rpmdb {
    pub fn open() -> Result<Self, RpmError> {
        Err(RpmError::Unsupported)
    }
    pub fn installed(&self) -> Vec<Package> {
        Vec::new()
    }
    pub fn by_name(&self, _name: &str) -> Vec<Package> {
        Vec::new()
    }
    pub fn is_installed(&self, _name: &str) -> bool {
        false
    }
    pub fn provide_version(&self, _provide: &str) -> Option<String> {
        None
    }
    pub fn releasever(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evr_and_nevra_formatting() {
        let p = Package {
            name: "bash".into(),
            epoch: None,
            version: "5.2.15".into(),
            release: "1.amzn2023".into(),
            arch: "x86_64".into(),
            summary: "The GNU Bourne Again shell".into(),
            size: 0,
            install_time: 0,
        };
        assert_eq!(p.evr(), "5.2.15-1.amzn2023");
        assert_eq!(p.nevra(), "bash-5.2.15-1.amzn2023.x86_64");
        assert_eq!(p.name_arch(), "bash.x86_64");

        let e = Package {
            epoch: Some(2),
            ..p.clone()
        };
        assert_eq!(e.evr(), "2:5.2.15-1.amzn2023");
        assert_eq!(e.nevra(), "bash-2:5.2.15-1.amzn2023.x86_64");
    }
}

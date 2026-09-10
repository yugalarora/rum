//! Compact, interned representation of repository metadata.
//!
//! RHEL's `primary.xml` holds millions of small strings (file paths, capability
//! names, arches, versions). Storing each as an owned `String` blew up a small
//! host's RAM during parse (a 761MB t2.micro OOM'd on RHEL BaseOS). Here every
//! string is interned once into a `Vec<String>` arena and referenced by a
//! 4-byte symbol; packages hold symbols, not strings. The arena + packages are
//! exactly what we rkyv-serialize, so the mmap'd warm cache is the same compact
//! form and resolving a symbol is a zero-copy slice into the mapped arena.
//!
//! Encapsulation ("the architectural wall"): `lasso` is used *only* as the
//! parse-time interner and its `Spur` never leaves this module — the stored
//! symbol type is a plain `u32`, which (unlike `lasso::Rodeo`) round-trips
//! through rkyv/mmap. Everything outside rum-repo sees only `&str`.

use lasso::{Key, Rodeo};

use crate::checksum::ChecksumKind;
use rum_solve::{Dep, DepFlag, Evr};

/// A symbol: index into [`Store::strings`].
pub type Sym = u32;

/// A dependency capability with interned name and (optional) EVR string.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct IDep {
    pub name: Sym,
    /// Interned EVR string (`[epoch:]version[-release]`); `None` if unversioned.
    pub evr: Option<Sym>,
    pub flag: DepFlag,
}

/// A package with every string field interned to a [`Sym`].
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct IPackage {
    pub name: Sym,
    pub epoch: u64,
    pub version: Sym,
    pub release: Sym,
    pub arch: Sym,
    pub summary: Sym,
    pub size: u64,
    pub location: Sym,
    pub checksum_kind: ChecksumKind,
    pub checksum_hex: Sym,
    pub repo_id: Sym,
    pub provides: Vec<IDep>,
    pub requires: Vec<IDep>,
    pub recommends: Vec<IDep>,
    pub files: Vec<Sym>,
}

/// The whole cache: the string arena plus the interned packages. This is the
/// rkyv root written to `primary.rkyv`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Store {
    /// `strings[sym]` is the text for symbol `sym`.
    pub strings: Vec<String>,
    pub packages: Vec<IPackage>,
}

/// Parse-time interner. Wraps `lasso::Rodeo`; `Spur` is confined here and only
/// `u32` symbols are handed out / stored.
pub struct Interner {
    rodeo: Rodeo,
}

impl Interner {
    pub fn new() -> Self {
        Interner {
            rodeo: Rodeo::default(),
        }
    }

    /// Intern a string, returning its stable symbol.
    #[inline]
    pub fn intern(&mut self, s: &str) -> Sym {
        self.rodeo.get_or_intern(s).into_usize() as Sym
    }

    /// Intern an optional EVR (stored as its display string).
    pub fn intern_evr(&mut self, evr: &Option<Evr>) -> Option<Sym> {
        evr.as_ref().map(|e| self.intern(&e.to_string()))
    }

    /// Finish, materializing the arena in symbol order alongside `packages`.
    pub fn into_store(self, packages: Vec<IPackage>) -> Store {
        let mut strings = vec![String::new(); self.rodeo.len()];
        for (sym, text) in self.rodeo.iter() {
            strings[sym.into_usize()] = text.to_owned();
        }
        Store { strings, packages }
    }
}

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

// --- Zero-copy `&str` views over the archived (mmap'd) store ----------------

impl ArchivedStore {
    #[inline]
    fn sym(&self, sym: Sym) -> &str {
        self.strings[sym as usize].as_str()
    }

    /// Iterate borrowed views over every package.
    pub fn views(&self) -> impl Iterator<Item = PkgView<'_>> {
        self.packages
            .iter()
            .map(move |pkg| PkgView { store: self, pkg })
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }
}

/// A borrowed, `&str`-only view of one archived package. This is the only
/// package shape that leaves rum-repo for the query commands.
pub struct PkgView<'a> {
    store: &'a ArchivedStore,
    pkg: &'a ArchivedIPackage,
}

impl<'a> PkgView<'a> {
    #[inline]
    fn s(&self, sym: Sym) -> &'a str {
        self.store.sym(sym)
    }
    pub fn name(&self) -> &'a str {
        self.s(self.pkg.name.to_native())
    }
    pub fn arch(&self) -> &'a str {
        self.s(self.pkg.arch.to_native())
    }
    pub fn version(&self) -> &'a str {
        self.s(self.pkg.version.to_native())
    }
    pub fn release(&self) -> &'a str {
        self.s(self.pkg.release.to_native())
    }
    pub fn summary(&self) -> &'a str {
        self.s(self.pkg.summary.to_native())
    }
    pub fn repo_id(&self) -> &'a str {
        self.s(self.pkg.repo_id.to_native())
    }
    pub fn epoch(&self) -> u64 {
        self.pkg.epoch.to_native()
    }
    pub fn name_arch(&self) -> String {
        format!("{}.{}", self.name(), self.arch())
    }
    pub fn evr(&self) -> String {
        if self.epoch() == 0 {
            format!("{}-{}", self.version(), self.release())
        } else {
            format!("{}:{}-{}", self.epoch(), self.version(), self.release())
        }
    }
    pub fn evr_cmp(&self) -> Evr {
        Evr::new(Some(self.epoch()), self.version(), self.release())
    }
    pub fn checksum_hex(&self) -> &'a str {
        self.s(self.pkg.checksum_hex.to_native())
    }

    /// Owned `Dep`s for the resolver's hard requires.
    pub fn requires(&self) -> Vec<Dep> {
        self.pkg
            .requires
            .iter()
            .map(|d| self.store.dep(d))
            .collect()
    }
    /// Owned `Dep`s for weak (Recommends) deps.
    pub fn recommends(&self) -> Vec<Dep> {
        self.pkg
            .recommends
            .iter()
            .map(|d| self.store.dep(d))
            .collect()
    }
    /// Provides plus advertised files (as unversioned provides) — the capability
    /// set the resolver registers, matching the download path's candidates.
    pub fn provides_with_files(&self) -> Vec<Dep> {
        let mut v: Vec<Dep> = self
            .pkg
            .provides
            .iter()
            .map(|d| self.store.dep(d))
            .collect();
        for f in self.pkg.files.iter() {
            v.push(Dep::unversioned(self.s(f.to_native())));
        }
        v
    }
}

// --- Materialize owned `AvailablePackage`s (resolve/download path) ----------

impl ArchivedStore {
    /// Resolve every archived package into an owned [`AvailablePackage`].
    pub fn to_owned_packages(&self) -> Vec<crate::AvailablePackage> {
        self.packages.iter().map(|p| self.owned(p)).collect()
    }

    /// Rehydrate a single package by index (used to materialize only the
    /// resolver's winning set, not the whole repo).
    pub fn package_at(&self, idx: usize) -> Option<crate::AvailablePackage> {
        self.packages.get(idx).map(|p| self.owned(p))
    }

    fn owned(&self, p: &ArchivedIPackage) -> crate::AvailablePackage {
        crate::AvailablePackage {
            name: self.sym(p.name.to_native()).to_string(),
            epoch: p.epoch.to_native(),
            version: self.sym(p.version.to_native()).to_string(),
            release: self.sym(p.release.to_native()).to_string(),
            arch: self.sym(p.arch.to_native()).to_string(),
            summary: self.sym(p.summary.to_native()).to_string(),
            size: p.size.to_native(),
            location: self.sym(p.location.to_native()).to_string(),
            checksum: crate::Checksum {
                kind: checksum_kind(&p.checksum_kind),
                hex: self.sym(p.checksum_hex.to_native()).to_string(),
            },
            repo_id: self.sym(p.repo_id.to_native()).to_string(),
            provides: p.provides.iter().map(|d| self.dep(d)).collect(),
            requires: p.requires.iter().map(|d| self.dep(d)).collect(),
            recommends: p.recommends.iter().map(|d| self.dep(d)).collect(),
            files: p
                .files
                .iter()
                .map(|f| self.sym(f.to_native()).to_string())
                .collect(),
        }
    }

    fn dep(&self, d: &ArchivedIDep) -> Dep {
        Dep {
            name: self.sym(d.name.to_native()).to_string(),
            flag: dep_flag(&d.flag),
            evr: d.evr.as_ref().map(|s| Evr::parse(self.sym(s.to_native()))),
        }
    }
}

fn dep_flag(a: &ArchivedDepFlag) -> DepFlag {
    match a {
        ArchivedDepFlag::Any => DepFlag::Any,
        ArchivedDepFlag::Eq => DepFlag::Eq,
        ArchivedDepFlag::Lt => DepFlag::Lt,
        ArchivedDepFlag::Le => DepFlag::Le,
        ArchivedDepFlag::Gt => DepFlag::Gt,
        ArchivedDepFlag::Ge => DepFlag::Ge,
    }
}

fn checksum_kind(a: &ArchivedChecksumKind) -> ChecksumKind {
    match a {
        ArchivedChecksumKind::Sha1 => ChecksumKind::Sha1,
        ArchivedChecksumKind::Sha256 => ChecksumKind::Sha256,
        ArchivedChecksumKind::Sha512 => ChecksumKind::Sha512,
    }
}

// Bring the archived enum names into scope for the match arms above.
use crate::checksum::ArchivedChecksumKind;
use rum_solve::ArchivedDepFlag;

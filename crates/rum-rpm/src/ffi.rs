//! Raw FFI declarations for the subset of librpm we use for read-only rpmdb
//! access. Hand-written (rather than bindgen) because the surface is tiny and
//! stable across the rpm 4.x we target (AL2 through AL2023 / RHEL9).
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_void};

// Opaque handle types. We only ever hold pointers to these.
pub enum rpmts_s {}
pub type rpmts = *mut rpmts_s;

pub enum rpmdbMatchIterator_s {}
pub type rpmdbMatchIterator = *mut rpmdbMatchIterator_s;

pub enum headerToken_s {}
pub type Header = *mut headerToken_s;

// Tag-data container, used to read (possibly array-valued) header tags.
pub enum rpmtd_s {}
pub type rpmtd = *mut rpmtd_s;

// rpmTagVal / rpmDbiTagVal are int32_t in rpm.
pub type rpmTagVal = c_int;

// Header tags we read (from rpmtag.h).
pub const RPMTAG_NAME: rpmTagVal = 1000;
pub const RPMTAG_VERSION: rpmTagVal = 1001;
pub const RPMTAG_RELEASE: rpmTagVal = 1002;
pub const RPMTAG_EPOCH: rpmTagVal = 1003;
pub const RPMTAG_SUMMARY: rpmTagVal = 1004;
pub const RPMTAG_SIZE: rpmTagVal = 1009;
pub const RPMTAG_ARCH: rpmTagVal = 1022;
pub const RPMTAG_INSTALLTIME: rpmTagVal = 1008;
pub const RPMTAG_PROVIDENAME: rpmTagVal = 1047;
pub const RPMTAG_PROVIDEVERSION: rpmTagVal = 1113;
pub const RPMTAG_BASENAMES: rpmTagVal = 1117;
pub const RPMTAG_DIRINDEXES: rpmTagVal = 1116;
pub const RPMTAG_DIRNAMES: rpmTagVal = 1118;

// rpmDbiTag values (from rpmdb.h). RPMDBI_PACKAGES iterates every installed
// header; a plain RPMTAG_* value keys the iterator by that tag.
pub const RPMDBI_PACKAGES: rpmTagVal = 0;

extern "C" {
    /// Read rpm config files / macros. Pass NULL/NULL for defaults. 0 = ok.
    pub fn rpmReadConfigFiles(file: *const c_char, target: *const c_char) -> c_int;

    pub fn rpmtsCreate() -> rpmts;
    pub fn rpmtsFree(ts: rpmts) -> rpmts;
    pub fn rpmtsSetRootDir(ts: rpmts, root_dir: *const c_char) -> c_int;

    /// Create a database iterator. `keyp`/`keylen` may be NULL/0 to match all.
    /// For string keys, keylen 0 means "use strlen".
    pub fn rpmtsInitIterator(
        ts: rpmts,
        rpmtag: rpmTagVal,
        keyp: *const c_void,
        keylen: usize,
    ) -> rpmdbMatchIterator;

    /// Advance the iterator. Returns NULL when exhausted. The returned Header
    /// is owned by the iterator and valid only until the next call.
    pub fn rpmdbNextIterator(mi: rpmdbMatchIterator) -> Header;

    pub fn rpmdbFreeIterator(mi: rpmdbMatchIterator) -> rpmdbMatchIterator;

    /// Number of headers the iterator will yield (for pre-sizing).
    pub fn rpmdbGetIteratorCount(mi: rpmdbMatchIterator) -> c_int;

    /// Whether a tag is present in the header (to distinguish "epoch 0" from
    /// "no epoch", which matters for EVR comparison).
    pub fn headerIsEntry(h: Header, tag: rpmTagVal) -> c_int;

    /// Borrow a string tag. Pointer is valid while the header is alive.
    pub fn headerGetString(h: Header, tag: rpmTagVal) -> *const c_char;

    /// Read a numeric tag. Returns 0 if absent.
    pub fn headerGetNumber(h: Header, tag: rpmTagVal) -> u64;

    // Tag-data container API, for reading array-valued tags (e.g. Provides).
    pub fn rpmtdNew() -> rpmtd;
    pub fn rpmtdFree(td: rpmtd) -> rpmtd;
    pub fn rpmtdFreeData(td: rpmtd);
    /// Fill `td` with the value of `tag`. `flags` 0 = default. Nonzero = ok.
    pub fn headerGet(h: Header, tag: rpmTagVal, td: rpmtd, flags: c_int) -> c_int;
    pub fn rpmtdInit(td: rpmtd) -> c_int;
    /// Advance; returns the current index (>= 0), or -1 when exhausted.
    pub fn rpmtdNext(td: rpmtd) -> c_int;
    pub fn rpmtdSetIndex(td: rpmtd, index: c_int) -> c_int;
    /// Current string element (NULL if not a string / out of range).
    pub fn rpmtdGetString(td: rpmtd) -> *const c_char;
    /// Current numeric element (0 if not numeric / out of range).
    pub fn rpmtdGetNumber(td: rpmtd) -> u64;
}

//! Command implementations.

pub mod check_update;
pub mod info;
pub mod list;
pub mod makecache;
pub mod pkgindex;
pub mod repo_sync;
pub mod repolist;
pub mod search;

use crate::Command;

/// Uniform "recognized but not implemented yet" response for planned verbs.
///
/// We deliberately exit non-zero so scripts don't mistake a stub for success.
pub fn not_yet(cmd: &Command) -> anyhow::Result<()> {
    let verb = cmd.verb();
    anyhow::bail!(
        "`rum {verb}` is a recognized command but is not implemented yet in this build.\n\
         Implemented so far: repolist, makecache, list, info, search, check-update.\n\
         Planned next: provides, download, then install/upgrade (SAT solver)."
    );
}

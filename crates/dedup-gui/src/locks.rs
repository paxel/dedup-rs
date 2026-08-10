//! The session-wide repo lock registry — one consent mechanism for the whole
//! app.
//!
//! Every repository starts **locked** each launch (nothing is persisted —
//! unlocking for deletion is a fresh, conscious choice per session). A lock
//! protects a repo's *existing* files: while locked, nothing may delete or
//! overwrite them, anywhere in the app. **Additions are always allowed** — a
//! locked repo can still gain files (a copy into it, a sync, a back-sync
//! promote), because gaining data is not the loss the lock exists to prevent.
//!
//! Unlocking is done on the repo's chip: every selector chip shows the padlock
//! badge, and clicking it toggles the lock. Once a repo is unlocked the user
//! has declared "I accept loss here" — per-file destructive actions fire with
//! no further questions (batch operations keep their plan summaries, which are
//! workflow, not nags).
//!
//! The registry is shared: one instance, cloned into every tab, so a repo
//! unlocked on one tab is unlocked everywhere. `Arc<Mutex<…>>` because views
//! keep clones; the GUI is single-threaded, so the lock is uncontended.

use crate::settings::TooltipVerbosity;
use crate::util::ExplainExt;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// The shared registry. The inner set holds the names of **unlocked** repos, so
/// an empty set — the state every session starts in — means everything is
/// protected.
#[derive(Clone, Default)]
pub struct RepoLocks(Arc<Mutex<HashSet<String>>>);

impl RepoLocks {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `repo`'s existing files are protected (the default).
    pub fn read_only(&self, repo: &str) -> bool {
        !self.lock().contains(repo)
    }

    /// Flip `repo` between locked and unlocked.
    pub fn toggle(&self, repo: &str) {
        let mut set = self.lock();
        if !set.remove(repo) {
            set.insert(repo.to_string());
        }
    }

    /// Wire a chip's padlock response: attach the standard hover copy and
    /// toggle the lock on click. The one-liner every selector chip uses, so the
    /// padlock behaves identically on every tab. (The Duplicates tab routes the
    /// click through its own action instead — toggling there also clears marks.)
    pub fn handle_badge(
        &self,
        lock: Option<egui::Response>,
        verbosity: TooltipVerbosity,
        name: &str,
    ) {
        if let Some(resp) = lock {
            let (short, long) = Self::hover_copy(self.read_only(name));
            if resp.explain(verbosity, short, long).clicked() {
                self.toggle(name);
            }
        }
    }

    /// The standard hover copy for a lock badge, shared by every tab so the
    /// padlock explains itself with one voice. `(short, verbose)` for the
    /// SHORT / VERBOSE tooltip setting.
    pub fn hover_copy(read_only: bool) -> (&'static str, &'static str) {
        if read_only {
            (
                "Locked: existing files here are protected — click to allow deleting and \
                 overwriting",
                "This repository is locked: none of its existing files can be deleted or \
                 overwritten from anywhere in the app. New files can still be added. Click \
                 to unlock it for this session — unlocking declares you accept losing data \
                 here, so destructive actions will run without further questions.",
            )
        } else {
            (
                "Unlocked: files here may be deleted or overwritten — click to protect them",
                "This repository is unlocked for this session: deleting and overwriting its \
                 files is allowed everywhere in the app, with no further confirmation. Click \
                 to lock it again. Every repository starts locked at launch.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_starts_locked_and_toggles_per_repo() {
        let locks = RepoLocks::new();
        assert!(locks.read_only("photos"), "a fresh session protects all");
        locks.toggle("photos");
        assert!(!locks.read_only("photos"), "unlocked after a toggle");
        assert!(locks.read_only("backup"), "other repos stay protected");
        locks.toggle("photos");
        assert!(locks.read_only("photos"), "a second toggle re-locks");
    }

    #[test]
    fn clones_share_one_registry() {
        let a = RepoLocks::new();
        let b = a.clone();
        b.toggle("photos");
        assert!(
            !a.read_only("photos"),
            "an unlock through one clone shows through every clone — one tab's \
             unlock is the whole app's unlock"
        );
    }
}

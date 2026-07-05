//! Background-worker plumbing: core operations run off the UI thread and report
//! progress through a channel that the UI drains once per frame.
//!
//! Coalescing is the consumer's job (per the core `Progress` contract): the UI
//! keeps only the latest progress event per repo, so a flood of per-file events
//! never forces a repaint or backs up the channel.

use crossbeam_channel::{Receiver, Sender};
use dedup_core::update::{Progress, ProgressEvent, UpdateStats};
use std::collections::HashMap;

/// A message from a worker thread to the UI.
pub enum WorkerMsg {
    Progress {
        repo: String,
        event: ProgressEvent,
    },
    Completed {
        repo: String,
        outcome: Result<UpdateStats, String>,
    },
}

/// [`Progress`] adapter that forwards every event onto the UI channel, tagged
/// with the repo it belongs to. Sends never block; a dropped receiver is fine.
pub struct ChannelProgress {
    repo: String,
    tx: Sender<WorkerMsg>,
}

impl ChannelProgress {
    pub fn new(repo: String, tx: Sender<WorkerMsg>) -> Self {
        Self { repo, tx }
    }
}

impl Progress for ChannelProgress {
    fn on(&self, event: ProgressEvent) {
        let _ = self.tx.send(WorkerMsg::Progress {
            repo: self.repo.clone(),
            event,
        });
    }
}

/// The UI's view of in-flight work: the latest coalesced progress per active
/// repo. A repo is "active" from when its update starts until its completion
/// message arrives.
#[derive(Default)]
pub struct WorkerState {
    active: HashMap<String, ProgressEvent>,
}

impl WorkerState {
    /// Register that an update for `repo` has been dispatched.
    pub fn mark_started(&mut self, repo: &str) {
        self.active.insert(
            repo.to_string(),
            ProgressEvent::Scanning { files: 0, dirs: 0 },
        );
    }

    pub fn is_active(&self, repo: &str) -> bool {
        self.active.contains_key(repo)
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn progress(&self, repo: &str) -> Option<&ProgressEvent> {
        self.active.get(repo)
    }

    /// Drain all pending messages, coalescing progress to the latest event per
    /// repo. Returns one entry per completion for the caller to act on (refresh
    /// stats, drop the cancellation token, …).
    pub fn drain(
        &mut self,
        rx: &Receiver<WorkerMsg>,
    ) -> Vec<(String, Result<UpdateStats, String>)> {
        let mut completed = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerMsg::Progress { repo, event } => {
                    self.active.insert(repo, event);
                }
                WorkerMsg::Completed { repo, outcome } => {
                    self.active.remove(&repo);
                    completed.push((repo, outcome));
                }
            }
        }
        completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_coalesces_progress_to_latest_per_repo() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut state = WorkerState::default();
        state.mark_started("a");

        // A flood of hashing events for the same repo.
        for done in 1..=500u64 {
            tx.send(WorkerMsg::Progress {
                repo: "a".into(),
                event: ProgressEvent::Hashing {
                    done,
                    total: 500,
                    current: format!("f{done}"),
                },
            })
            .unwrap();
        }

        let completed = state.drain(&rx);
        assert!(completed.is_empty(), "no completion yet");
        assert_eq!(
            state.active_count(),
            1,
            "still one active repo, not 500 entries"
        );
        match state.progress("a") {
            Some(ProgressEvent::Hashing { done, .. }) => assert_eq!(*done, 500, "kept latest only"),
            other => panic!("expected latest hashing event, got {other:?}"),
        }
    }

    #[test]
    fn completion_clears_active_and_is_reported() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut state = WorkerState::default();
        state.mark_started("a");
        state.mark_started("b");

        tx.send(WorkerMsg::Completed {
            repo: "a".into(),
            outcome: Ok(UpdateStats::default()),
        })
        .unwrap();

        let completed = state.drain(&rx);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0, "a");
        assert!(!state.is_active("a"), "completed repo is no longer active");
        assert!(state.is_active("b"), "the other repo stays active");
        assert_eq!(state.active_count(), 1);
    }
}

//! Background-worker plumbing: core operations run off the UI thread and report
//! progress through a channel that the UI drains once per frame.
//!
//! Coalescing is the consumer's job (per the core `Progress` contract): the UI
//! keeps only the latest progress event per repo, so a flood of per-file events
//! never forces a repaint or backs up the channel.

use crossbeam_channel::{Receiver, Sender};
use dedup_core::update::{CheckStats, Progress, ProgressEvent, UpdateStats};
use std::collections::HashMap;
use std::time::Instant;

/// What a queued/running job does to a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// Full scan: walk, hash new/changed files, write the index.
    Update,
    /// Dry-run: walk and diff against the index, no hashing or writes.
    Check,
}

/// The result of a finished job, tagged by the kind that produced it.
pub enum JobOutcome {
    Update(Result<UpdateStats, String>),
    Check(Result<CheckStats, String>),
}

/// A message from a worker thread to the UI.
pub enum WorkerMsg {
    Progress { repo: String, event: ProgressEvent },
    Completed { repo: String, outcome: JobOutcome },
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

/// Where a tracked repo sits in the scan pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoStatus {
    /// Waiting in the queue; no worker thread yet.
    Queued,
    /// A worker thread is actively scanning.
    Running,
}

/// The UI's per-repo record: what the job does, pipeline status, timing, and
/// latest coalesced progress event.
pub struct RepoProgress {
    pub kind: JobKind,
    pub status: RepoStatus,
    pub queued_at: Instant,
    pub started_at: Option<Instant>,
    pub event: ProgressEvent,
}

/// The UI's view of in-flight work: one [`RepoProgress`] per repo that is
/// queued or running. A repo is tracked from when it is enqueued until its
/// completion message arrives (or it is removed from the queue).
#[derive(Default)]
pub struct WorkerState {
    tracked: HashMap<String, RepoProgress>,
}

impl WorkerState {
    /// Register that a `kind` job for `repo` has been queued but not yet started.
    pub fn mark_queued(&mut self, repo: &str, kind: JobKind) {
        self.tracked.insert(
            repo.to_string(),
            RepoProgress {
                kind,
                status: RepoStatus::Queued,
                queued_at: Instant::now(),
                started_at: None,
                event: ProgressEvent::Scanning { files: 0, dirs: 0 },
            },
        );
    }

    /// Promote a queued repo to running, stamping its start time.
    pub fn mark_running(&mut self, repo: &str) {
        if let Some(record) = self.tracked.get_mut(repo) {
            record.status = RepoStatus::Running;
            record.started_at = Some(Instant::now());
        }
    }

    /// Whether `repo` is queued or running (guards against double-enqueue).
    pub fn is_tracked(&self, repo: &str) -> bool {
        self.tracked.contains_key(repo)
    }

    /// Number of repos actively scanning; drives the serial pump.
    pub fn running_count(&self) -> usize {
        self.tracked
            .values()
            .filter(|r| r.status == RepoStatus::Running)
            .count()
    }

    /// Total tracked repos (queued + running); drives the busy gate and repaint.
    pub fn active_count(&self) -> usize {
        self.tracked.len()
    }

    pub fn get(&self, repo: &str) -> Option<&RepoProgress> {
        self.tracked.get(repo)
    }

    /// Drop a still-queued repo that was cancelled before it started.
    pub fn remove(&mut self, repo: &str) {
        self.tracked.remove(repo);
    }

    /// Drain all pending messages, coalescing progress to the latest event per
    /// repo. Returns one entry per completion for the caller to act on (refresh
    /// stats, drop the cancellation token, …).
    pub fn drain(&mut self, rx: &Receiver<WorkerMsg>) -> Vec<(String, JobOutcome)> {
        let mut completed = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerMsg::Progress { repo, event } => {
                    if let Some(record) = self.tracked.get_mut(&repo) {
                        record.event = event;
                    }
                }
                WorkerMsg::Completed { repo, outcome } => {
                    self.tracked.remove(&repo);
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
        state.mark_queued("a", JobKind::Update);
        state.mark_running("a");

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
        match state.get("a").map(|r| &r.event) {
            Some(ProgressEvent::Hashing { done, .. }) => assert_eq!(*done, 500, "kept latest only"),
            other => panic!("expected latest hashing event, got {other:?}"),
        }
    }

    #[test]
    fn completion_clears_active_and_is_reported() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut state = WorkerState::default();
        state.mark_queued("a", JobKind::Update);
        state.mark_running("a");
        state.mark_queued("b", JobKind::Update);
        state.mark_running("b");

        tx.send(WorkerMsg::Completed {
            repo: "a".into(),
            outcome: JobOutcome::Update(Ok(UpdateStats::default())),
        })
        .unwrap();

        let completed = state.drain(&rx);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0, "a");
        assert!(
            !state.is_tracked("a"),
            "completed repo is no longer tracked"
        );
        assert!(state.is_tracked("b"), "the other repo stays tracked");
        assert_eq!(state.active_count(), 1);
    }

    #[test]
    fn queued_then_running_transition_stamps_start() {
        let mut state = WorkerState::default();
        state.mark_queued("a", JobKind::Update);
        assert!(state.is_tracked("a"));
        assert_eq!(state.running_count(), 0, "queued repo is not running");
        assert_eq!(state.active_count(), 1);
        match state.get("a") {
            Some(record) => {
                assert_eq!(record.status, RepoStatus::Queued);
                assert!(record.started_at.is_none(), "not started while queued");
            }
            None => panic!("expected a tracked record"),
        }

        state.mark_running("a");
        assert_eq!(state.running_count(), 1);
        match state.get("a") {
            Some(record) => {
                assert_eq!(record.status, RepoStatus::Running);
                assert!(record.started_at.is_some(), "start time stamped on run");
            }
            None => panic!("expected a tracked record"),
        }
    }

    #[test]
    fn remove_drops_a_queued_repo() {
        let mut state = WorkerState::default();
        state.mark_queued("a", JobKind::Update);
        state.mark_queued("b", JobKind::Update);
        assert_eq!(state.active_count(), 2);

        state.remove("a");
        assert!(!state.is_tracked("a"), "removed repo is no longer tracked");
        assert!(state.is_tracked("b"), "the other repo stays queued");
        assert_eq!(state.active_count(), 1);
    }
}

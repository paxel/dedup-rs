//! The SYNC page (ADR 0006): push backup groups from their mains to their
//! backups. Mains are dragged from the pool into the "mains to sync" card;
//! each brings its stored backups with their stored modes, and a backup can
//! be left out of this run. PREVIEW plans every push in a window of its own;
//! GO pushes there.

use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{DiffRun, SyncPlan};
use dedup_core::store::{Store, SyncGroup, SyncMode, SyncSink};
use dedup_core::sync_group::{SinkOutcome, guard_mirror_source, plan_group_sync, run_group_sync};
use egui::RichText;
use std::collections::HashSet;
use std::sync::Arc;

/// What one backup would receive, or why it cannot be planned.
type SinkPlan = (String, SyncMode, Result<SyncPlan, String>);

/// The plan for one group: its name, main, and each backup's plan — or why
/// the whole group is refused.
struct GroupPlan {
    group: String,
    main: String,
    sinks: Result<Vec<SinkPlan>, String>,
}

enum Msg {
    Planned(Vec<GroupPlan>),
}

enum Act {
    /// Put a group's main into the card.
    Choose(String),
    /// Take a group back out of the card.
    Unchoose(String),
    /// Leave a backup out of this run, or take it back in.
    ToggleSink(String, String),
    Preview,
    CancelPreview,
    Go,
}

pub struct SyncPage {
    /// Every backup group, as of the last sync with the store.
    groups: Vec<(String, SyncGroup)>,
    /// The groups in the card, in the order they were dropped.
    chosen: Vec<String>,
    /// `(group, backup)` pairs left out of this run.
    left_out: HashSet<(String, String)>,
    /// The planned pushes: the preview is open while this is set.
    plan: Option<Vec<GroupPlan>>,
    notice: Option<String>,
    error: Option<String>,
    locks: crate::locks::RepoLocks,
    activity: crate::activity::Shared,
    verbosity: TooltipVerbosity,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Draw the preview in the page (the tests have only one window).
    preview_inline: bool,
}

impl SyncPage {
    pub fn new(locks: crate::locks::RepoLocks, activity: crate::activity::Shared) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            groups: Vec::new(),
            chosen: Vec::new(),
            left_out: HashSet::new(),
            plan: None,
            notice: None,
            error: None,
            locks,
            activity,
            verbosity: TooltipVerbosity::default(),
            tx,
            rx,
            preview_inline: cfg!(test),
        }
    }

    /// Re-read the groups; groups that no longer exist leave the card.
    pub fn sync_repos(&mut self, store: &Store) {
        match store.list_sync_groups() {
            Ok(groups) => {
                self.groups = groups;
                let names: HashSet<&String> = self.groups.iter().map(|(n, _)| n).collect();
                self.chosen.retain(|g| names.contains(g));
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Whether the preview is open in its own window, which then shows the
    /// activity window itself.
    pub fn previewing(&self) -> bool {
        self.plan.is_some() && !self.preview_inline
    }

    fn group(&self, name: &str) -> Option<&SyncGroup> {
        self.groups.iter().find(|(n, _)| n == name).map(|(_, g)| g)
    }

    /// The group as this run pushes it: the backups not left out.
    fn run_group(&self, name: &str) -> Option<SyncGroup> {
        self.group(name).map(|g| SyncGroup {
            main: g.main.clone(),
            sinks: g
                .sinks
                .iter()
                .filter(|s| !self.left_out.contains(&(name.to_string(), s.repo.clone())))
                .cloned()
                .collect(),
        })
    }

    /// Unlock every backup of `group` this run deletes in (ADR 0006: the card
    /// unlocks APPLY CHANGES and MIRROR backups; ADD ONLY keeps its lock).
    fn unlock_deleting_sinks(&self, group: &str) {
        if let Some(g) = self.run_group(group) {
            for sink in g.sinks.iter().filter(|s| s.mode.deletes_in_sink()) {
                self.locks.set_read_only(&sink.repo, false);
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, verbosity: TooltipVerbosity) {
        self.verbosity = verbosity;
        while let Ok(Msg::Planned(plan)) = self.rx.try_recv() {
            self.plan = Some(plan);
        }
        self.drop_locked_groups();
        let mut acts = Vec::new();
        if self.preview_inline && self.plan.is_some() {
            self.preview_body(ui, &mut acts);
        } else {
            ui.add_space(6.0);
            ui.label(
                RichText::new("SYNC")
                    .color(theme::lilac())
                    .size(18.0)
                    .strong(),
            );
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.pool_section(ui, &mut acts);
                    self.card_section(ui, &mut acts);
                    let ready = !self.chosen.is_empty()
                        && !crate::activity::lock(&self.activity).is_running();
                    if crate::lcars::action_button(
                        ui,
                        &format!("{} PREVIEW", crate::icon::SEARCH),
                        ready,
                        theme::amber(),
                    )
                    .explain(
                        self.verbosity,
                        "Plan every push and show it",
                        "Work out what each backup in the card would receive and lose, \
                         and open it in its own window. Nothing is copied or deleted \
                         until GO.",
                    )
                    .clicked()
                    {
                        acts.push(Act::Preview);
                    }
                    if let Some(notice) = &self.notice {
                        ui.colored_label(theme::amber(), notice);
                    }
                    if let Some(err) = &self.error {
                        ui.colored_label(theme::red(), err);
                    }
                });
            if self.plan.is_some() {
                let ctx = ui.ctx().clone();
                let closed = crate::process::preview_window(&ctx, "sync_preview", "SYNC", |ui| {
                    let mut here = Vec::new();
                    self.preview_body(ui, &mut here);
                    let ctx = ui.ctx().clone();
                    for act in here {
                        self.apply(&ctx, store, act);
                    }
                    crate::activity::lock(&self.activity).show(ui);
                });
                if closed {
                    acts.push(Act::CancelPreview);
                }
            }
        }
        let ctx = ui.ctx().clone();
        for act in acts {
            self.apply(&ctx, store, act);
        }
    }

    /// ADR 0006: a backup this run deletes in that was locked on another page
    /// no longer fits the card, so its group falls back into the pool.
    fn drop_locked_groups(&mut self) {
        let mut dropped = Vec::new();
        for name in &self.chosen {
            if let Some(g) = self.run_group(name)
                && let Some(sink) = g
                    .sinks
                    .iter()
                    .find(|s| s.mode.deletes_in_sink() && self.locks.read_only(&s.repo))
            {
                dropped.push((name.clone(), sink.repo.clone()));
            }
        }
        for (group, sink) in dropped {
            self.chosen.retain(|g| *g != group);
            self.notice = Some(format!(
                "'{group}' went back to the pool: its backup '{sink}' was locked"
            ));
        }
    }

    /// The pool: every group's main that is not in the card.
    fn pool_section(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let pool: Vec<(String, String)> = self
            .groups
            .iter()
            .filter(|(n, _)| !self.chosen.contains(n))
            .map(|(n, g)| (n.clone(), g.main.clone()))
            .collect();
        let verbosity = self.verbosity;
        crate::lcars::frame_lcars(
            ui,
            "MAINS — DRAG INTO THE CARD BELOW",
            theme::lilac(),
            |ui| {
                let dropped = crate::process::drop_zone(ui, theme::lilac(), |ui| {
                    if pool.is_empty() {
                        ui.label(
                            RichText::new(if self.groups.is_empty() {
                                "No backup groups yet — make one on the repository page."
                            } else {
                                "Every group is in the card."
                            })
                            .color(theme::tan())
                            .size(12.0),
                        );
                    }
                    ui.horizontal_wrapped(|ui| {
                        for (group, main) in &pool {
                            let id = egui::Id::unique(("sync-pool", group));
                            ui.dnd_drag_source(id, group.clone(), |ui| {
                                crate::repo_chip::repo_chip(
                                    ui,
                                    main,
                                    false,
                                    theme::orange(),
                                    true,
                                    None,
                                );
                            })
                            .response
                            .explain(
                                verbosity,
                                "Drag into the card to sync this group",
                                "Drag this main into the card: its backups come along with \
                                 their push modes.",
                            );
                        }
                    });
                });
                if let Some(group) = dropped {
                    acts.push(Act::Unchoose(group));
                }
            },
        );
    }

    /// The card: each chosen main with its backups, their modes, and a switch
    /// to leave one out of this run.
    fn card_section(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let chosen: Vec<(String, SyncGroup)> = self
            .chosen
            .iter()
            .filter_map(|n| self.group(n).map(|g| (n.clone(), g.clone())))
            .collect();
        let verbosity = self.verbosity;
        let ((), header) = crate::lcars::frame_lcars(
            ui,
            "MAINS TO SYNC — BACKUPS IN APPLY CHANGES OR MIRROR LOSE FILES",
            theme::red(),
            |ui| {
                let dropped = crate::process::drop_zone(ui, theme::red(), |ui| {
                    ui.set_min_height(48.0);
                    if chosen.is_empty() {
                        ui.label(
                            RichText::new("Drop mains here")
                                .color(theme::tan())
                                .size(12.0),
                        );
                    }
                    for (name, group) in &chosen {
                        ui.horizontal(|ui| {
                            crate::repo_chip::repo_chip(
                                ui,
                                &group.main,
                                true,
                                theme::orange(),
                                true,
                                None,
                            );
                            if ui
                                .button(RichText::new(crate::icon::X).color(theme::black()))
                                .explain(
                                    verbosity,
                                    "Take this group out of the run",
                                    "Put this main back into the pool; its group is not synced.",
                                )
                                .clicked()
                            {
                                acts.push(Act::Unchoose(name.clone()));
                            }
                        });
                        for sink in &group.sinks {
                            self.sink_row(ui, name, sink, acts);
                        }
                        ui.add_space(4.0);
                    }
                });
                if let Some(group) = dropped {
                    acts.push(Act::Choose(group));
                }
            },
        );
        header.explain(
            verbosity,
            "The groups this run pushes",
            "Every main here is pushed to its backups in their stored modes. Backups in APPLY \
             CHANGES or MIRROR mode can lose files and are unlocked; ADD ONLY backups only \
             gain files and keep their lock.",
        );
    }

    fn sink_row(&self, ui: &mut egui::Ui, group: &str, sink: &SyncSink, acts: &mut Vec<Act>) {
        let out = self
            .left_out
            .contains(&(group.to_string(), sink.repo.clone()));
        ui.horizontal(|ui| {
            ui.add_space(24.0);
            ui.label(RichText::new(format!("→ {}", sink.repo)).color(if out {
                theme::tan()
            } else {
                theme::text()
            }));
            ui.label(
                RichText::new(mode_label(sink.mode))
                    .color(theme::lilac())
                    .size(12.0),
            );
            let (label, hover) = if out {
                ("LEFT OUT", "Take this backup back into the run")
            } else {
                ("IN THIS RUN", "Leave this backup out of this run only")
            };
            if crate::lcars::toggle_button(ui, label, !out, theme::orange())
                .explain(self.verbosity, hover, hover)
                .clicked()
            {
                acts.push(Act::ToggleSink(group.to_string(), sink.repo.clone()));
            }
        });
    }

    fn preview_body(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let busy = crate::activity::lock(&self.activity).is_running();
        egui::Panel::bottom("sync_preview_go")
            .frame(egui::Frame::new().fill(theme::bg()).inner_margin(8.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let work = self.plan.as_ref().is_some_and(|p| has_work(p));
                    if crate::lcars::action_button(ui, "GO — PUSH", !busy && work, theme::red())
                        .explain(
                            self.verbosity,
                            "Push every group as planned",
                            "Copy and delete in every backup as listed. The progress and the \
                             report show in this window.",
                        )
                        .clicked()
                    {
                        acts.push(Act::Go);
                    }
                    if crate::lcars::action_button(ui, "CANCEL", !busy, theme::tan())
                        .explain(
                            self.verbosity,
                            "Close the preview, change nothing",
                            "Close the preview without copying or deleting anything.",
                        )
                        .clicked()
                    {
                        acts.push(Act::CancelPreview);
                    }
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::bg()))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in self.plan.as_deref().map(plan_lines).unwrap_or_default() {
                            ui.label(RichText::new(line).color(theme::text()).size(13.0));
                        }
                    });
            });
    }

    fn apply(&mut self, ctx: &egui::Context, store: &Arc<Store>, act: Act) {
        match act {
            Act::Choose(group) => {
                if !self.chosen.contains(&group) {
                    self.chosen.push(group.clone());
                }
                self.notice = None;
                self.unlock_deleting_sinks(&group);
            }
            Act::Unchoose(group) => self.chosen.retain(|g| *g != group),
            Act::ToggleSink(group, sink) => {
                let key = (group.clone(), sink);
                if !self.left_out.remove(&key) {
                    self.left_out.insert(key);
                }
                self.unlock_deleting_sinks(&group);
            }
            Act::Preview => self.start_plan(ctx, store),
            Act::CancelPreview => self.plan = None,
            Act::Go => self.start_push(ctx, store),
        }
    }

    /// The groups as this run pushes them.
    fn run_groups(&self) -> Vec<(String, SyncGroup)> {
        self.chosen
            .iter()
            .filter_map(|n| self.run_group(n).map(|g| (n.clone(), g)))
            .collect()
    }

    fn start_plan(&mut self, ctx: &egui::Context, store: &Arc<Store>) {
        let groups = self.run_groups();
        let names: Vec<String> = groups.iter().map(|(_, g)| g.main.clone()).collect();
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        let started = crate::activity::lock(&self.activity).start_quiet(
            ctx,
            crate::activity::Spec {
                title: "SYNC: planning".into(),
                repos: names,
            },
            move |progress, cancel| {
                let total = groups.len() as u64;
                let mut plans = Vec::new();
                for (i, (name, group)) in groups.iter().enumerate() {
                    if cancel.is_cancelled() {
                        return Err("cancelled".into());
                    }
                    progress.phase(format!("planning '{name}'"), i as u64, Some(total));
                    plans.push(plan_one(&store, name, group));
                }
                let _ = tx.send(Msg::Planned(plans));
                progress.repaint();
                Ok(())
            },
        );
        if let Err(busy) = started {
            crate::activity::lock(&self.activity)
                .card(ctx, crate::activity::Notification::refused("SYNC", &busy));
        }
    }

    /// GO: push every group, each backup on its own, then plan again so the
    /// preview shows what is left.
    fn start_push(&mut self, ctx: &egui::Context, store: &Arc<Store>) {
        let groups = self.run_groups();
        let names: Vec<String> = groups.iter().map(|(_, g)| g.main.clone()).collect();
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        let started = crate::activity::lock(&self.activity).start(
            ctx,
            crate::activity::Spec {
                title: "SYNC".into(),
                repos: names,
            },
            move |progress, cancel| {
                let mut report = crate::run_result::RunReport::new("SYNC");
                let (mut copied, mut deleted, mut errors) = (0u64, 0u64, 0u64);
                for (name, group) in &groups {
                    // A group whose main looks emptied is refused as a whole.
                    if let Err(e) = guard_mirror_source(&store, group) {
                        report.problem(format!("'{name}': {e}"));
                        continue;
                    }
                    for sink in &group.sinks {
                        let one = SyncGroup {
                            main: group.main.clone(),
                            sinks: vec![sink.clone()],
                        };
                        let run_progress = crate::activity::RunProgress {
                            activity: progress.clone(),
                            repo: sink.repo.clone(),
                        };
                        let run = DiffRun::new(&run_progress, cancel);
                        let outcomes = match run_group_sync(&store, &one, &run) {
                            Ok(o) => o,
                            Err(e) => {
                                report.problem(format!("'{}': {e}", sink.repo));
                                continue;
                            }
                        };
                        for (repo, outcome) in outcomes {
                            match outcome {
                                SinkOutcome::Pushed(stats) => {
                                    copied = copied.saturating_add(stats.copied);
                                    deleted = deleted.saturating_add(stats.deleted);
                                    errors = errors.saturating_add(stats.errors);
                                    progress.row_done(
                                        &repo,
                                        format!(
                                            "{} copied, {} deleted",
                                            stats.copied, stats.deleted
                                        ),
                                    );
                                }
                                SinkOutcome::Failed(e) => report.problem(format!("'{repo}': {e}")),
                                SinkOutcome::Skipped => {
                                    progress.row_done(&repo, "not reached — cancelled")
                                }
                            }
                        }
                    }
                }
                let plans = groups
                    .iter()
                    .map(|(name, group)| plan_one(&store, name, group))
                    .collect();
                let _ = tx.send(Msg::Planned(plans));
                let mut report = report.count("copied", copied).count("deleted", deleted);
                if errors > 0 {
                    report = report.count("errors", errors);
                }
                report.cancelled(cancel.is_cancelled())
            },
        );
        if let Err(busy) = started {
            crate::activity::lock(&self.activity)
                .card(ctx, crate::activity::Notification::refused("SYNC", &busy));
        }
    }

    #[cfg(test)]
    pub(crate) fn chosen(&self) -> &[String] {
        &self.chosen
    }
}

/// Plan one group: every backup's copies and deletes, or why it is refused.
fn plan_one(store: &Store, name: &str, group: &SyncGroup) -> GroupPlan {
    let sinks = plan_group_sync(store, group)
        .map(|plans| {
            plans
                .into_iter()
                .zip(group.sinks.iter())
                .map(|((repo, plan), sink)| (repo, sink.mode, Ok(plan)))
                .collect()
        })
        .map_err(|e| e.to_string());
    GroupPlan {
        group: name.to_string(),
        main: group.main.clone(),
        sinks,
    }
}

/// Whether GO has anything to do.
fn has_work(plan: &[GroupPlan]) -> bool {
    plan.iter().any(|g| {
        g.sinks.as_ref().is_ok_and(|s| {
            s.iter().any(|(_, _, p)| {
                p.as_ref()
                    .is_ok_and(|p| !p.copies.is_empty() || !p.deletes.is_empty())
            })
        })
    })
}

/// The preview's lines: per group and backup what it gains and loses, the
/// paths below.
fn plan_lines(plan: &[GroupPlan]) -> Vec<String> {
    let mut lines = Vec::new();
    for g in plan {
        lines.push(format!("{} (main {})", g.group, g.main));
        match &g.sinks {
            Err(e) => lines.push(format!("    refused — {e}")),
            Ok(sinks) if sinks.is_empty() => lines.push("    no backup in this run".into()),
            Ok(sinks) => {
                for (repo, mode, p) in sinks {
                    match p {
                        Err(e) => lines.push(format!("  → {repo}: cannot be planned — {e}")),
                        Ok(p) => {
                            lines.push(format!(
                                "  → {repo} ({}): {} to copy, {} to delete",
                                mode_label(*mode),
                                p.copies.len(),
                                p.deletes.len()
                            ));
                            lines.extend(p.copies.iter().map(|r| format!("        + {r}")));
                            lines.extend(p.deletes.iter().map(|r| format!("        − {r}")));
                        }
                    }
                }
            }
        }
    }
    if plan.is_empty() {
        lines.push("Nothing to sync.".into());
    }
    lines
}

fn mode_label(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::AddOnly => "ADD ONLY",
        SyncMode::ApplyChanges => "APPLY CHANGES",
        SyncMode::Mirror => "MIRROR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dedup_core::update::{CancellationToken, NoProgress, update_repo};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    /// A store with group `main` → `copy` (`mode`), `main` holding one file.
    fn group_store(mode: SyncMode) -> (tempfile::TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        for name in ["main", "copy"] {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            store.create_repo(name, &dir.to_string_lossy()).unwrap();
        }
        std::fs::write(tmp.path().join("main").join("a.txt"), b"hello").unwrap();
        update_repo(&store, "main", 1, &NoProgress, &CancellationToken::new()).unwrap();
        update_repo(&store, "copy", 1, &NoProgress, &CancellationToken::new()).unwrap();
        store.create_sync_group("main", "main").unwrap();
        store.add_sync_sink("main", "copy", mode).unwrap();
        (tmp, Arc::new(store))
    }

    fn harness(store: Arc<Store>, page: SyncPage) -> Harness<'static, SyncPage> {
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, page: &mut SyncPage| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    page.show(ui, &store, TooltipVerbosity::default());
                    crate::activity::lock(&page.activity).show(ui);
                },
                page,
            );
        h.run();
        h
    }

    fn settle(h: &mut Harness<'static, SyncPage>) {
        for _ in 0..400 {
            h.step();
            if !crate::activity::lock(&h.state().activity).is_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        h.step();
        h.step();
    }

    /// Choosing a group unlocks a backup it deletes in, never an ADD ONLY one.
    #[test]
    fn the_card_unlocks_only_backups_that_lose_files() {
        for (mode, unlocked) in [(SyncMode::Mirror, true), (SyncMode::AddOnly, false)] {
            let (_tmp, store) = group_store(mode);
            let mut page =
                SyncPage::new(crate::locks::RepoLocks::new(), crate::activity::scratch());
            page.sync_repos(&store);
            page.apply(
                &egui::Context::default(),
                &store,
                Act::Choose("main".into()),
            );
            assert_eq!(!page.locks.read_only("copy"), unlocked, "{mode:?}");
        }
    }

    /// A backup locked again on another page sends its group back to the
    /// pool, with a notice.
    #[test]
    fn a_relocked_backup_sends_its_group_back_to_the_pool() {
        let (_tmp, store) = group_store(SyncMode::Mirror);
        let mut page = SyncPage::new(crate::locks::RepoLocks::new(), crate::activity::scratch());
        page.sync_repos(&store);
        page.apply(
            &egui::Context::default(),
            &store,
            Act::Choose("main".into()),
        );
        page.locks.set_read_only("copy", true);
        let h = harness(store, page);
        assert!(h.state().chosen().is_empty(), "back in the pool");
        assert!(h.query_by_label_contains("was locked").is_some());
    }

    /// PREVIEW lists what the backup receives and copies nothing; GO pushes
    /// and the preview then shows nothing left to do. Leaving the backup out
    /// plans nothing for it.
    #[test]
    fn preview_lists_the_push_and_go_carries_it_out() {
        let (tmp, store) = group_store(SyncMode::AddOnly);
        let mut page = SyncPage::new(crate::locks::RepoLocks::new(), crate::activity::scratch());
        page.sync_repos(&store);
        page.apply(
            &egui::Context::default(),
            &store,
            Act::Choose("main".into()),
        );
        let mut h = harness(Arc::clone(&store), page);
        h.get_by_label_contains("PREVIEW").click_accesskit();
        h.step();
        settle(&mut h);
        assert!(
            h.query_by_label_contains("→ copy (ADD ONLY): 1 to copy")
                .is_some()
        );
        assert!(
            !tmp.path().join("copy").join("a.txt").exists(),
            "PREVIEW copies nothing"
        );
        h.get_by_label("GO — PUSH").click_accesskit();
        h.step();
        settle(&mut h);
        assert!(
            tmp.path().join("copy").join("a.txt").exists(),
            "GO pushed the file"
        );
        h.get_by_label("CANCEL").click_accesskit();
        h.run();
        assert!(h.state().plan.is_none(), "CANCEL closes the preview");

        h.state_mut().apply(
            &egui::Context::default(),
            &store,
            Act::ToggleSink("main".into(), "copy".into()),
        );
        let plan = plan_one(&store, "main", &h.state().run_group("main").unwrap());
        assert!(
            plan.sinks.unwrap().is_empty(),
            "a left-out backup is not planned"
        );
    }
}

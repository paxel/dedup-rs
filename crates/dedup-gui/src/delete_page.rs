//! The DELETE page (ADR 0006): delete from several repositories every file
//! whose content a reference repository already holds. Content held only by
//! the repositories being deleted from stays; that is DUPLICATES' job.

use crate::filter_ui::FilterBuilder;
use crate::process::{CardSpec, RoleCards};
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::delete_known::{DeleteItem, plan_delete_known, run_delete_known};
use dedup_core::diff::DiffRun;
use dedup_core::store::Store;
use egui::RichText;
use std::sync::Arc;

const FROM: usize = 0;
const KNOWN: usize = 1;

enum Msg {
    Planned(Result<Vec<DeleteItem>, String>),
}

enum Act {
    Preview,
    CancelPreview,
    Go,
}

pub struct DeletePage {
    cards: RoleCards,
    /// Every repository as `(name, is_main)`, as of the last sync.
    repos: Vec<(String, bool)>,
    /// Content a reference only holds as deleted counts too.
    delete_missing: bool,
    filter: FilterBuilder,
    /// The planned copies: the preview is open while this is set.
    plan: Option<Vec<DeleteItem>>,
    /// References whose index is older than a source's, when the plan was
    /// made: they may not hold what they seem to.
    stale: Vec<String>,
    error: Option<String>,
    locks: crate::locks::RepoLocks,
    activity: crate::activity::Shared,
    verbosity: TooltipVerbosity,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Draw the preview in the page (the tests have only one window).
    preview_inline: bool,
}

impl DeletePage {
    pub fn new(locks: crate::locks::RepoLocks, activity: crate::activity::Shared) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            cards: RoleCards::new(vec![
                CardSpec {
                    title: "DELETE FROM — FILES HERE GET DELETED",
                    hover: "Files in these repositories are deleted when a reference holds \
                            their content. A repository dropped here is unlocked.",
                    accent: theme::red(),
                    lock: Some(false),
                    single: false,
                },
                CardSpec {
                    title: "ALREADY KNOWN IN — SAFE FROM CHANGES",
                    hover: "The references: content any of them holds is deleted from the \
                            other card. Nothing in them changes; a repository dropped here is \
                            locked.",
                    accent: theme::blue(),
                    lock: Some(true),
                    single: false,
                },
            ]),
            repos: Vec::new(),
            delete_missing: false,
            filter: FilterBuilder::new(),
            plan: None,
            stale: Vec::new(),
            error: None,
            locks,
            activity,
            verbosity: TooltipVerbosity::default(),
            tx,
            rx,
            preview_inline: cfg!(test),
        }
    }

    pub fn sync_repos(&mut self, store: &Store) {
        match store.list_repos() {
            Ok(list) => {
                let mains = store.main_repo_names().unwrap_or_default();
                self.repos = list
                    .into_iter()
                    .map(|(name, _, _)| {
                        let is_main = mains.contains(&name);
                        (name, is_main)
                    })
                    .collect();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    pub fn previewing(&self) -> bool {
        self.plan.is_some() && !self.preview_inline
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, verbosity: TooltipVerbosity) {
        self.verbosity = verbosity;
        while let Ok(Msg::Planned(result)) = self.rx.try_recv() {
            match result {
                Ok(plan) => self.plan = Some(plan),
                Err(e) => self.error = Some(e),
            }
        }
        if self.repos.is_empty() {
            self.sync_repos(store);
        }
        let names: Vec<String> = self.repos.iter().map(|(n, _)| n.clone()).collect();
        self.cards.follow(&self.locks, &names);
        let mut acts = Vec::new();
        if self.preview_inline && self.plan.is_some() {
            self.preview_body(ui, &mut acts);
        } else {
            ui.add_space(6.0);
            ui.label(
                RichText::new("COPY")
                    .color(theme::lilac())
                    .size(18.0)
                    .strong(),
            );
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.cards
                        .show(ui, "copy", &self.repos, &self.locks, self.verbosity);
                    self.how_section(ui, store);
                    let ready = !self.cards.members(FROM).is_empty()
                        && !self.cards.members(KNOWN).is_empty()
                        && !crate::activity::lock(&self.activity).is_running();
                    if crate::lcars::action_button(
                        ui,
                        &format!("{} PREVIEW", crate::icon::SEARCH),
                        ready,
                        theme::amber(),
                    )
                    .explain(
                        self.verbosity,
                        "Work out what would be deleted and show it",
                        "List every file that would be deleted, in its own window. Nothing is \
                         deleted until GO.",
                    )
                    .clicked()
                    {
                        acts.push(Act::Preview);
                    }
                    if let Some(err) = &self.error {
                        ui.colored_label(theme::red(), err);
                    }
                });
            if self.plan.is_some() {
                let ctx = ui.ctx().clone();
                let closed =
                    crate::process::preview_window(&ctx, "delete_preview", "DELETE", |ui| {
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

    /// HOW: DELETE MISSING, the warning about stale references, and the
    /// filter.
    fn how_section(&mut self, ui: &mut egui::Ui, store: &Arc<Store>) {
        let verbosity = self.verbosity;
        let stale = stale_references(store, &self.cards.members(FROM), &self.cards.members(KNOWN));
        crate::lcars::frame_lcars(
            ui,
            "HOW — WHAT COUNTS AS ALREADY KNOWN",
            theme::amber(),
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("DELETE MISSING").color(theme::tan()));
                    for (value, label) in [(true, "TRUE"), (false, "FALSE")] {
                        if crate::lcars::toggle_button(
                            ui,
                            label,
                            self.delete_missing == value,
                            theme::red(),
                        )
                        .explain(
                            verbosity,
                            "Does a file deleted in a reference count?",
                            "TRUE: a file whose content a reference only remembers as deleted \
                             (a missing entry) is deleted here too — the deletion is carried \
                             over. FALSE: only content a reference really holds counts.",
                        )
                        .clicked()
                        {
                            self.delete_missing = value;
                        }
                    }
                });
                for name in &stale {
                    ui.colored_label(
                        theme::amber(),
                        format!(
                            "'{name}' was last updated before a repository it is checked against: \
                         UPDATE it first, or its index may claim files it no longer holds"
                        ),
                    );
                }
                let sugg = self.cards.members(FROM).into_iter().next();
                let outcome = self.filter.ui(ui, store, sugg.as_deref(), verbosity);
                if outcome.error.is_some() {
                    self.error = outcome.error;
                }
            },
        );
    }

    fn preview_body(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let busy = crate::activity::lock(&self.activity).is_running();
        let n = self.plan.as_ref().map_or(0, Vec::len);
        egui::Panel::bottom("copy_preview_go")
            .frame(egui::Frame::new().fill(theme::bg()).inner_margin(8.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if crate::lcars::action_button(
                        ui,
                        &format!("GO — DELETE {n} FILE(S)"),
                        !busy && n > 0,
                        theme::red(),
                    )
                    .explain(
                        self.verbosity,
                        "Delete as listed",
                        "Delete every listed file. The progress and the report show in this \
                         window.",
                    )
                    .clicked()
                    {
                        acts.push(Act::Go);
                    }
                    if crate::lcars::action_button(ui, "CANCEL", !busy, theme::tan())
                        .explain(
                            self.verbosity,
                            "Close the preview, delete nothing",
                            "Close the preview without deleting anything.",
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
                let lines = self.plan.as_deref().map(plan_lines).unwrap_or_default();
                for name in &self.stale {
                    ui.colored_label(
                        theme::amber(),
                        format!("'{name}' may be out of date — it was updated before a source"),
                    );
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (line, suffixed) in lines {
                            let color = if suffixed {
                                theme::amber()
                            } else {
                                theme::text()
                            };
                            ui.label(RichText::new(line).color(color).size(13.0));
                        }
                    });
            });
    }

    fn apply(&mut self, ctx: &egui::Context, store: &Arc<Store>, act: Act) {
        match act {
            Act::Preview => self.start(ctx, store, false),
            Act::CancelPreview => self.plan = None,
            Act::Go => self.start(ctx, store, true),
        }
    }

    /// Plan (and with `go`, first copy the open plan): the work behind the
    /// activity window. After GO the plan is made again, so the preview shows
    /// what is left.
    fn start(&mut self, ctx: &egui::Context, store: &Arc<Store>, go: bool) {
        self.error = None;
        let sources = self.cards.members(FROM);
        let references = self.cards.members(KNOWN);
        if sources.is_empty() || references.is_empty() {
            return;
        }
        self.stale = stale_references(store, &sources, &references);
        let delete_missing = self.delete_missing;
        let filter = self.filter.filter_string();
        let plan = if go { self.plan.clone() } else { None };
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        let repos = sources.clone();
        let work = move |progress: &crate::activity::ActivityProgress,
                         cancel: &dedup_core::update::CancellationToken|
              -> crate::run_result::RunReport {
            let mut report = crate::run_result::RunReport::new("DELETE");
            let ref_refs: Vec<&str> = references.iter().map(String::as_str).collect();
            if let Some(plan) = &plan {
                let mut stats = dedup_core::diff::DeleteStats::default();
                for source in &sources {
                    let mine: Vec<DeleteItem> = plan
                        .iter()
                        .filter(|i| i.source == *source)
                        .cloned()
                        .collect();
                    if mine.is_empty() {
                        continue;
                    }
                    let run_progress = crate::activity::RunProgress {
                        activity: progress.clone(),
                        repo: source.clone(),
                    };
                    match run_delete_known(
                        &store,
                        &mine,
                        &ref_refs,
                        &DiffRun::new(&run_progress, cancel),
                    ) {
                        Ok(s) => {
                            stats.deleted = stats.deleted.saturating_add(s.deleted);
                            stats.cancelled |= s.cancelled;
                        }
                        Err(e) => report.problem(format!("'{source}': {e}")),
                    }
                }
                report = report
                    .count("deleted", stats.deleted)
                    .cancelled(stats.cancelled);
            }
            progress.phase("planning", 0, None);
            let source_refs: Vec<&str> = sources.iter().map(String::as_str).collect();
            let planned = plan_delete_known(
                &store,
                &source_refs,
                &ref_refs,
                delete_missing,
                filter.as_deref(),
            )
            .map_err(|e| e.to_string());
            if let Err(e) = &planned {
                report.problem(e.clone());
            }
            let _ = tx.send(Msg::Planned(planned));
            progress.repaint();
            report
        };
        let spec = crate::activity::Spec {
            title: if go {
                "DELETE".into()
            } else {
                "DELETE: planning".into()
            },
            repos,
        };
        let mut activity = crate::activity::lock(&self.activity);
        let started = if go {
            activity.start(ctx, spec, work)
        } else {
            activity.start_quiet(ctx, spec, move |p, c| {
                let report = work(p, c);
                if report.problem_count() > 0 {
                    Err("the delete could not be planned".into())
                } else {
                    Ok(())
                }
            })
        };
        if let Err(busy) = started {
            activity.card(ctx, crate::activity::Notification::refused("DELETE", &busy));
        }
    }
}

/// References last updated before a source: their index may claim content
/// they no longer hold.
fn stale_references(store: &Store, sources: &[String], references: &[String]) -> Vec<String> {
    let scanned = |n: &String| store.get_repo_stats(n).map_or(0, |s| s.last_scan_ms);
    let newest_source = sources.iter().map(scanned).max().unwrap_or(0);
    references
        .iter()
        .filter(|r| scanned(r) < newest_source)
        .cloned()
        .collect()
}

/// The preview's lines, each with whether only a deleted reference copy
/// makes it a candidate.
fn plan_lines(plan: &[DeleteItem]) -> Vec<(String, bool)> {
    if plan.is_empty() {
        return vec![(
            "Nothing to delete: no reference holds any of it.".into(),
            false,
        )];
    }
    plan.iter()
        .map(|i| {
            let note = if i.only_missing {
                "  (deleted in the reference)"
            } else {
                ""
            };
            (format!("{}: {}{note}", i.source, i.rel), i.only_missing)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dedup_core::update::{CancellationToken, NoProgress, update_repo};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn store_with(files: &[(&str, &str, &[u8])]) -> (tempfile::TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        for name in ["src", "ref"] {
            std::fs::create_dir_all(tmp.path().join(name)).unwrap();
            store
                .create_repo(name, &tmp.path().join(name).to_string_lossy())
                .unwrap();
        }
        for (repo, rel, bytes) in files {
            std::fs::write(tmp.path().join(repo).join(rel), bytes).unwrap();
        }
        // `ref` first, then `src` a little later, so `ref` reads as older.
        for name in ["ref", "src"] {
            update_repo(&store, name, 1, &NoProgress, &CancellationToken::new()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (tmp, Arc::new(store))
    }

    fn harness(store: Arc<Store>, page: DeletePage) -> Harness<'static, DeletePage> {
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1000.0, 800.0))
            .build_ui_state(
                move |ui, page: &mut DeletePage| {
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

    fn settle(h: &mut Harness<'static, DeletePage>) {
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

    /// DELETE FROM unlocks, ALREADY KNOWN IN locks; a source locked again
    /// elsewhere falls back to the pool.
    #[test]
    fn the_cards_unlock_sources_and_lock_references() {
        let locks = crate::locks::RepoLocks::new();
        let mut page = DeletePage::new(locks.clone(), crate::activity::scratch());
        locks.set_read_only("ref", false);
        page.cards.place(&locks, "src", Some(FROM));
        page.cards.place(&locks, "ref", Some(KNOWN));
        assert!(
            !locks.read_only("src"),
            "a repository to delete from is unlocked"
        );
        assert!(locks.read_only("ref"), "a reference is locked");
        locks.set_read_only("src", true);
        page.cards.follow(&locks, &["src".into(), "ref".into()]);
        assert!(page.cards.members(FROM).is_empty(), "back in the pool");
    }

    /// PREVIEW lists what the reference holds and deletes nothing; GO deletes
    /// exactly that, and a file only the source holds stays.
    #[test]
    fn preview_then_go_deletes_what_the_reference_holds() {
        let (tmp, store) = store_with(&[
            ("src", "dup.txt", b"same"),
            ("src", "mine.txt", b"only here"),
            ("ref", "orig.txt", b"same"),
        ]);
        let locks = crate::locks::RepoLocks::new();
        let mut page = DeletePage::new(locks.clone(), crate::activity::scratch());
        page.sync_repos(&store);
        page.cards.place(&locks, "src", Some(FROM));
        page.cards.place(&locks, "ref", Some(KNOWN));
        let mut h = harness(store, page);
        h.get_by_label_contains("PREVIEW").click_accesskit();
        h.step();
        settle(&mut h);
        assert!(h.query_by_label("src: dup.txt").is_some());
        assert!(
            tmp.path().join("src").join("dup.txt").exists(),
            "PREVIEW deletes nothing"
        );
        h.get_by_label("GO — DELETE 1 FILE(S)").click_accesskit();
        h.step();
        settle(&mut h);
        assert!(
            !tmp.path().join("src").join("dup.txt").exists(),
            "GO deleted it"
        );
        assert!(
            tmp.path().join("src").join("mine.txt").exists(),
            "unique content stays"
        );
        assert!(
            tmp.path().join("ref").join("orig.txt").exists(),
            "the reference is untouched"
        );
    }

    /// A reference updated before a source is flagged.
    #[test]
    fn a_reference_older_than_a_source_is_flagged() {
        let (_tmp, store) = store_with(&[("src", "a.txt", b"a")]);
        // `src` was scanned after `ref` in `store_with`.
        let stale = stale_references(&store, &["src".into()], &["ref".into()]);
        assert_eq!(stale, vec!["ref".to_string()]);
        assert!(stale_references(&store, &["ref".into()], &["src".into()]).is_empty());
    }
}

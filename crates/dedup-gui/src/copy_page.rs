//! The COPY page (ADR 0006): copy into one repository what it and the
//! reference repositories lack, from several sources, each content once.
//! Nothing is overwritten: an occupied name gets a numeric suffix.

use crate::filter_ui::FilterBuilder;
use crate::process::{CardSpec, RoleCards};
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::copy_into::{CopyItem, CopySpec, plan_copy_into, run_copy_into};
use dedup_core::diff::DiffRun;
use dedup_core::store::Store;
use egui::RichText;
use std::sync::Arc;

const FROM: usize = 0;
const INTO: usize = 1;
const KNOWN: usize = 2;

enum Msg {
    Planned(Result<Vec<CopyItem>, String>),
}

enum Act {
    Preview,
    CancelPreview,
    Go,
}

pub struct CopyPage {
    cards: RoleCards,
    /// Every repository as `(name, is_main)`, as of the last sync.
    repos: Vec<(String, bool)>,
    subdir: String,
    template: String,
    filter: FilterBuilder,
    /// The planned copies: the preview is open while this is set.
    plan: Option<Vec<CopyItem>>,
    /// The target the plan was made for.
    plan_target: String,
    error: Option<String>,
    locks: crate::locks::RepoLocks,
    activity: crate::activity::Shared,
    verbosity: TooltipVerbosity,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Draw the preview in the page (the tests have only one window).
    preview_inline: bool,
}

impl CopyPage {
    pub fn new(locks: crate::locks::RepoLocks, activity: crate::activity::Shared) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            cards: RoleCards::new(vec![
                CardSpec {
                    title: "COPY FROM — SAFE FROM CHANGES",
                    hover: "Files are copied out of these repositories; nothing in them changes. \
                            A repository dropped here is locked.",
                    accent: theme::blue(),
                    lock: Some(true),
                    single: false,
                },
                CardSpec {
                    title: "COPY INTO — ONLY GAINS FILES",
                    hover: "The one repository the files are copied into. Nothing in it is \
                            deleted or overwritten, so its lock stays as it is.",
                    accent: theme::green(),
                    lock: None,
                    single: true,
                },
                CardSpec {
                    title: "ALREADY KNOWN IN — SAFE FROM CHANGES",
                    hover: "Content any of these repositories holds is not copied. Nothing in \
                            them changes; a repository dropped here is locked.",
                    accent: theme::blue(),
                    lock: Some(true),
                    single: false,
                },
            ]),
            repos: Vec::new(),
            subdir: String::new(),
            template: String::new(),
            filter: FilterBuilder::new(),
            plan: None,
            plan_target: String::new(),
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
                        && !self.cards.members(INTO).is_empty()
                        && !crate::activity::lock(&self.activity).is_running();
                    if crate::lcars::action_button(
                        ui,
                        &format!("{} PREVIEW", crate::icon::SEARCH),
                        ready,
                        theme::amber(),
                    )
                    .explain(
                        self.verbosity,
                        "Work out what would be copied and show it",
                        "List every file that would be copied and where it lands, in its own \
                         window. Nothing is copied until GO.",
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
                let closed = crate::process::preview_window(&ctx, "copy_preview", "COPY", |ui| {
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

    /// HOW: the optional subfolder and path template, and the filter.
    fn how_section(&mut self, ui: &mut egui::Ui, store: &Arc<Store>) {
        let verbosity = self.verbosity;
        crate::lcars::frame_lcars(ui, "HOW — WHERE THE COPIES LAND", theme::amber(), |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("SUBFOLDER").color(theme::tan()));
                ui.add(
                    egui::TextEdit::singleline(&mut self.subdir)
                        .desired_width(220.0)
                        .hint_text("none: the target's root"),
                )
                .explain(
                    verbosity,
                    "A folder inside the target the copies land under",
                    "A folder inside the target the copies land under, created when \
                     missing. Empty puts them at the target's root.",
                );
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("TEMPLATE").color(theme::tan()));
                ui.add(
                    egui::TextEdit::singleline(&mut self.template)
                        .desired_width(320.0)
                        .hint_text("none: keep the source path, e.g. {year}/{month}/{o-name}"),
                )
                .explain(
                    verbosity,
                    "A path for each copy, built from its facts",
                    "A path template, as ORGANIZE uses: {year}/{month}/{o-name} sorts the \
                     copies by date. Empty keeps each file's path from its source.",
                );
            });
            let sugg = self.cards.members(FROM).into_iter().next();
            let outcome = self.filter.ui(ui, store, sugg.as_deref(), verbosity);
            if outcome.error.is_some() {
                self.error = outcome.error;
            }
        });
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
                        &format!("GO — COPY {n} FILE(S)"),
                        !busy && n > 0,
                        theme::red(),
                    )
                    .explain(
                        self.verbosity,
                        "Copy as listed",
                        "Copy every listed file. The progress and the report show in this \
                         window.",
                    )
                    .clicked()
                    {
                        acts.push(Act::Go);
                    }
                    if crate::lcars::action_button(ui, "CANCEL", !busy, theme::tan())
                        .explain(
                            self.verbosity,
                            "Close the preview, copy nothing",
                            "Close the preview without copying anything.",
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
                ui.label(
                    RichText::new(format!("into '{}'", self.plan_target))
                        .color(theme::amber())
                        .strong(),
                );
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
        let Some(target) = self.cards.members(INTO).into_iter().next() else {
            return;
        };
        let references = self.cards.members(KNOWN);
        let subdir = self.subdir.trim().to_string();
        let template = self.template.trim().to_string();
        let filter = self.filter.filter_string();
        let plan = if go { self.plan.clone() } else { None };
        self.plan_target = target.clone();
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        let mut repos = sources.clone();
        repos.push(target.clone());
        let work = move |progress: &crate::activity::ActivityProgress,
                         cancel: &dedup_core::update::CancellationToken|
              -> crate::run_result::RunReport {
            let mut report = crate::run_result::RunReport::new("COPY");
            if let Some(plan) = &plan {
                let run_progress = crate::activity::RunProgress {
                    activity: progress.clone(),
                    repo: target.clone(),
                };
                match run_copy_into(&store, &target, plan, &DiffRun::new(&run_progress, cancel)) {
                    Ok(stats) => {
                        report = report
                            .count("copied", stats.copied)
                            .count("under a suffixed name", stats.suffixed)
                            .cancelled(stats.cancelled);
                    }
                    Err(e) => report.problem(e.to_string()),
                }
            }
            progress.phase("planning", 0, None);
            let source_refs: Vec<&str> = sources.iter().map(String::as_str).collect();
            let ref_refs: Vec<&str> = references.iter().map(String::as_str).collect();
            let spec = CopySpec {
                sources: &source_refs,
                target: &target,
                references: &ref_refs,
                subdir: Some(subdir.as_str()).filter(|s| !s.is_empty()),
                template: Some(template.as_str()).filter(|s| !s.is_empty()),
                filter: filter.as_deref(),
            };
            let planned = plan_copy_into(&store, &spec).map_err(|e| e.to_string());
            if let Err(e) = &planned {
                report.problem(e.clone());
            }
            let _ = tx.send(Msg::Planned(planned));
            progress.repaint();
            report
        };
        let spec = crate::activity::Spec {
            title: if go {
                "COPY".into()
            } else {
                "COPY: planning".into()
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
                    Err("the copy could not be planned".into())
                } else {
                    Ok(())
                }
            })
        };
        if let Err(busy) = started {
            activity.card(ctx, crate::activity::Notification::refused("COPY", &busy));
        }
    }
}

/// The preview's lines, each with whether its name was suffixed.
fn plan_lines(plan: &[CopyItem]) -> Vec<(String, bool)> {
    if plan.is_empty() {
        return vec![(
            "Nothing to copy: the target and the references hold it all.".into(),
            false,
        )];
    }
    plan.iter()
        .map(|i| {
            let note = if i.suffixed {
                "  (name taken — suffixed)"
            } else {
                ""
            };
            (
                format!("{}: {} → {}{note}", i.source, i.rel, i.target_rel),
                i.suffixed,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dedup_core::update::{CancellationToken, NoProgress, update_repo};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn store_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        for name in ["src", "dst"] {
            std::fs::create_dir_all(tmp.path().join(name)).unwrap();
            store
                .create_repo(name, &tmp.path().join(name).to_string_lossy())
                .unwrap();
        }
        for (repo, rel) in files {
            std::fs::write(tmp.path().join(repo).join(rel), rel.as_bytes()).unwrap();
        }
        for name in ["src", "dst"] {
            update_repo(&store, name, 1, &NoProgress, &CancellationToken::new()).unwrap();
        }
        (tmp, Arc::new(store))
    }

    fn harness(store: Arc<Store>, page: CopyPage) -> Harness<'static, CopyPage> {
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1000.0, 800.0))
            .build_ui_state(
                move |ui, page: &mut CopyPage| {
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

    fn settle(h: &mut Harness<'static, CopyPage>) {
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

    /// The cards set their locks: sources and references are locked, the
    /// target keeps whatever lock it had.
    #[test]
    fn the_cards_lock_sources_and_references_but_not_the_target() {
        let locks = crate::locks::RepoLocks::new();
        let mut page = CopyPage::new(locks.clone(), crate::activity::scratch());
        locks.set_read_only("dst", false);
        locks.set_read_only("src", false);
        page.cards.place(&locks, "src", Some(FROM));
        page.cards.place(&locks, "dst", Some(INTO));
        assert!(locks.read_only("src"), "a source is locked");
        assert!(!locks.read_only("dst"), "the target keeps its lock");
        // A source unlocked elsewhere no longer fits its card.
        locks.set_read_only("src", false);
        page.cards.follow(&locks, &["src".into(), "dst".into()]);
        assert!(page.cards.members(FROM).is_empty());
        assert!(page.cards.notice().is_some_and(|n| n.contains("'src'")));
    }

    /// PREVIEW lists the copy and copies nothing; GO copies under a suffix
    /// when the name is taken, and the existing file stays.
    #[test]
    fn preview_then_go_copies_without_overwriting() {
        let (tmp, store) = store_with(&[("src", "a.txt"), ("dst", "b.txt")]);
        std::fs::write(tmp.path().join("dst").join("a.txt"), b"mine").unwrap();
        let locks = crate::locks::RepoLocks::new();
        let mut page = CopyPage::new(locks.clone(), crate::activity::scratch());
        page.sync_repos(&store);
        page.cards.place(&locks, "src", Some(FROM));
        page.cards.place(&locks, "dst", Some(INTO));
        let mut h = harness(store, page);
        h.get_by_label_contains("PREVIEW").click_accesskit();
        h.step();
        settle(&mut h);
        assert!(
            h.query_by_label_contains("src: a.txt → a (1).txt")
                .is_some()
        );
        assert!(
            !tmp.path().join("dst").join("a (1).txt").exists(),
            "PREVIEW copies nothing"
        );
        h.get_by_label("GO — COPY 1 FILE(S)").click_accesskit();
        h.step();
        settle(&mut h);
        assert_eq!(
            std::fs::read(tmp.path().join("dst").join("a.txt")).unwrap(),
            b"mine"
        );
        assert!(
            tmp.path().join("dst").join("a (1).txt").is_file(),
            "GO copied it"
        );
        assert!(locks.read_only("dst"), "a locked target still gains files");
    }
}

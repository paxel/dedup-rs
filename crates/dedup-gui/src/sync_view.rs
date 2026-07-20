//! The Sync Groups tab: keep one **main** repository backed up to one or more
//! remote **sinks**, instead of the manual duplicate → relocate → rescan dance.
//!
//! A group names a main repo, its sinks, and how it is pushed:
//! **ADD ONLY** copies what a sink lacks and never deletes; **MIRROR** also
//! removes sink content the main no longer has, so the sink converges on
//! exactly the main's content.
//!
//! The tab follows the same REVIEW → confirm → RUN shape as the Transfer tab:
//! REVIEW plans every sink and renders the result in the shared review board,
//! RUN asks before touching anything and then pushes on a worker thread.

use crate::review;
use crate::review::PREVIEW_CAP;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{DiffEvent, DiffProgress, DiffRun};
use dedup_core::store::{Store, SyncGroup, SyncMode};
use dedup_core::sync_group::{SinkOutcome, plan_group_sync, run_group_sync};
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::sync::{Arc, Mutex};

/// Messages from a worker thread.
enum Msg {
    /// A finished push, as the shared report — or a group-level refusal that
    /// meant nothing ran at all.
    Done(Result<crate::run_result::RunReport, String>),
    /// A finished plan. `confirm` asks the UI to raise the RUN confirmation
    /// once the plan is in hand — the deferred half of a RUN SYNC click.
    Preview {
        result: Result<PreviewOutcome, String>,
        confirm: bool,
    },
}

/// The result of planning a group push, built off the UI thread. The board rows
/// come back unsorted; the (cheap) sort happens when they are applied.
struct PreviewOutcome {
    /// The group this plan is for. Carried through so the confirmation and the
    /// push both act on the group that was *planned*, not whatever is selected
    /// when the async result lands — the user may have clicked another group
    /// while the scan ran.
    group_name: String,
    group: SyncGroup,
    rows: Vec<review::ReviewRow>,
    added: usize,
    removed: usize,
    sink_count: usize,
    main_header: String,
    /// Sinks the plan would empty of their current contents, `(sink, live)`.
    wholesale_sinks: Vec<(String, u64)>,
}

/// Plan every sink and turn it into review-board rows: a copy shows as *added*
/// on the sink side, a mirror deletion as *removed*. Runs the full index scan,
/// so callers keep it off the UI thread.
fn build_preview(
    store: &Store,
    group_name: &str,
    group: &SyncGroup,
) -> Result<PreviewOutcome, String> {
    let plans = plan_group_sync(store, group).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let (mut added, mut removed) = (0usize, 0usize);
    let mut wholesale_sinks = Vec::new();
    for (sink, plan) in &plans {
        // A plan that deletes everything the sink holds today is a wholesale
        // replacement, not an incremental sync — worth naming before proceeding.
        let live = store
            .get_repo_stats(sink)
            .map(|s| s.file_count)
            .unwrap_or(0);
        if live > 0 && plan.deletes.len() as u64 >= live {
            wholesale_sinks.push((sink.clone(), live));
        }
        // Counts cover the whole plan; the board rows are a capped sample, like
        // the Transfer tab's — an initial whole-disk push would otherwise build
        // one row per file.
        for rel in &plan.copies {
            added += 1;
            if rows.len() < PREVIEW_CAP {
                rows.push(review::ReviewRow {
                    source: review::SideStatus::Unchanged,
                    target: review::SideStatus::Added,
                    source_path: rel.clone(),
                    target_path: format!("{sink}: {rel}"),
                });
            }
        }
        for rel in &plan.deletes {
            removed += 1;
            if rows.len() < PREVIEW_CAP {
                rows.push(review::ReviewRow {
                    source: review::SideStatus::Absent,
                    target: review::SideStatus::Removed,
                    source_path: String::new(),
                    target_path: format!("{sink}: {rel}"),
                });
            }
        }
    }
    Ok(PreviewOutcome {
        group_name: group_name.to_string(),
        group: group.clone(),
        rows,
        added,
        removed,
        sink_count: plans.len(),
        main_header: group.main.clone(),
        wholesale_sinks,
    })
}

/// Collects the per-file failures of a push so the result panel can list them.
///
/// Live per-file progress is still dropped (a group push reports per sink), but
/// the errors are not: "12 error(s)" without saying which files is exactly the
/// report a beta user cannot act on.
struct CollectProblems {
    problems: Mutex<Vec<String>>,
}

impl CollectProblems {
    fn new() -> Self {
        Self {
            problems: Mutex::new(Vec::new()),
        }
    }

    fn take(&self) -> Vec<String> {
        match self.problems.lock() {
            Ok(mut held) => std::mem::take(&mut held),
            Err(_) => Vec::new(),
        }
    }
}

impl DiffProgress for CollectProblems {
    fn on(&self, event: DiffEvent) {
        if let DiffEvent::Error { path, message } = event
            && let Ok(mut held) = self.problems.lock()
            && held.len() < crate::run_result::MAX_PROBLEMS
        {
            held.push(format!("{path}: {message}"));
        }
    }
}

pub struct SyncView {
    loaded: bool,
    /// Every registered repo, for the pickers.
    repos: Vec<String>,
    /// Every group, by name.
    groups: Vec<(String, SyncGroup)>,
    /// The group whose details and preview are shown.
    selected: Option<String>,
    /// NEW GROUP form.
    new_name: String,
    new_main: Option<String>,
    /// Preview of the next push, in the shared review board.
    preview: Vec<review::ReviewRow>,
    preview_totals: [usize; 3],
    preview_main_header: String,
    preview_sink_header: String,
    /// Sinks whose entire current contents the last plan would delete, as
    /// `(sink, files it holds now)`. The main's content is copied in to replace
    /// them, so the sink does not end up empty — but nothing it holds today
    /// survives, which the confirm dialog spells out.
    wholesale_sinks: Vec<(String, u64)>,
    review_state: review::ReviewState,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<String>,
    /// The group a raised confirmation is about, captured when the plan landed.
    /// PROCEED pushes *this* group, not whatever is selected when the button is
    /// clicked — the two can differ if the confirmation was built from an async
    /// plan. Set with `confirm`, cleared when it closes.
    pending_push: Option<(String, SyncGroup)>,
    /// The report of the last finished push.
    result: crate::run_result::ResultModal,
    running: bool,
    /// Set while a plan is being built on a worker thread, so the UI shows it
    /// is busy without offering CANCEL — planning takes no cancel token, only a
    /// push does. (Same split as the Transfer tab.)
    previewing: bool,
    cancel: CancellationToken,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    verbosity: TooltipVerbosity,
}

enum Act {
    Select(String),
    SetNewMain(String),
    CreateGroup,
    DeleteGroup(String),
    AddSink(String, String),
    RemoveSink(String, String),
    MakeMain(String, String),
    SetMode(String, SyncMode),
    Preview,
    Ask,
    Confirm,
    CancelConfirm,
    CancelRun,
}

impl Default for SyncView {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            loaded: false,
            repos: Vec::new(),
            groups: Vec::new(),
            selected: None,
            new_name: String::new(),
            new_main: None,
            preview: Vec::new(),
            preview_totals: [0; 3],
            preview_main_header: String::new(),
            preview_sink_header: String::new(),
            wholesale_sinks: Vec::new(),
            review_state: review::ReviewState::default(),
            status: None,
            error: None,
            confirm: None,
            pending_push: None,
            result: crate::run_result::ResultModal::default(),
            running: false,
            previewing: false,
            cancel: CancellationToken::new(),
            tx,
            rx,
            verbosity: TooltipVerbosity::default(),
        }
    }

    /// Reload repos and groups from the store. Called on first show and
    /// whenever the tab is re-shown, so there is no refresh button.
    pub fn sync_repos(&mut self, store: &Store) {
        match (store.list_repos(), store.list_sync_groups()) {
            (Ok(repos), Ok(groups)) => {
                self.repos = repos.into_iter().map(|(n, _, _)| n).collect();
                self.groups = groups;
                if let Some(selected) = &self.selected
                    && !self.groups.iter().any(|(n, _)| n == selected)
                {
                    self.selected = None;
                    self.clear_preview();
                }
                self.loaded = true;
                self.error = None;
            }
            (Err(e), _) | (_, Err(e)) => self.error = Some(e.to_string()),
        }
    }

    /// Repos that belong to no group — the only ones a group can take in.
    fn ungrouped(&self) -> Vec<String> {
        self.repos
            .iter()
            .filter(|r| !self.groups.iter().any(|(_, g)| g.has_member(r)))
            .cloned()
            .collect()
    }

    fn selected_group(&self) -> Option<(String, SyncGroup)> {
        let name = self.selected.as_ref()?;
        self.groups
            .iter()
            .find(|(n, _)| n == name)
            .map(|(n, g)| (n.clone(), g.clone()))
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, verbosity: TooltipVerbosity) {
        self.verbosity = verbosity;
        self.drain(ui);
        if !self.loaded {
            self.sync_repos(store);
        }
        let mut acts: Vec<Act> = Vec::new();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("SYNC GROUPS")
                        .color(theme::GREEN)
                        .size(18.0)
                        .strong(),
                );
                crate::util::shortcut_bar(ui, "P review · R run sync");

                self.groups_section(ui, &mut acts);
                self.new_group_section(ui, &mut acts);
                if let Some((name, group)) = self.selected_group() {
                    self.members_section(ui, &name, &group, &mut acts);
                    self.action_section(ui, &mut acts);
                }

                if let Some(err) = &self.error {
                    ui.colored_label(theme::RED, err);
                }
                if let Some(status) = &self.status {
                    ui.label(RichText::new(status).color(theme::TAN).size(13.0));
                }
                ui.separator();
                self.preview_panel(ui);
            });

        if self.confirm.is_none()
            && !self.result.is_open()
            && !self.running
            && !self.previewing
            && !ui.ctx().egui_wants_keyboard_input()
        {
            ui.input(|i| {
                if i.key_pressed(egui::Key::P) {
                    acts.push(Act::Preview);
                }
                if i.key_pressed(egui::Key::R) {
                    acts.push(Act::Ask);
                }
            });
        }
        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }
        self.result.show(ui);
        for act in acts {
            self.apply(store, act);
        }
    }

    /// The groups themselves: one chip per group, the selected one filled.
    fn groups_section(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "GROUPS — PICK ONE TO MANAGE", theme::GREEN, |ui| {
            if self.groups.is_empty() {
                ui.label(
                    RichText::new(
                        "No sync groups yet. Create one below to keep a repository backed \
                         up to another.",
                    )
                    .color(theme::TEXT)
                    .size(12.0),
                );
                return;
            }
            let names: Vec<String> = self.groups.iter().map(|(n, _)| n.clone()).collect();
            ui.horizontal_wrapped(|ui| {
                for name in &names {
                    let selected = self.selected.as_deref() == Some(name.as_str());
                    if crate::lcars::toggle_button(ui, name, selected, theme::GREEN)
                        .explain(
                            self.verbosity,
                            "Manage this group",
                            "Show this group's main repository, its sinks and its mode, \
                             and preview or run its next push.",
                        )
                        .clicked()
                    {
                        acts.push(Act::Select(name.clone()));
                    }
                }
            });
        });
    }

    /// Create a group: a name plus the repo that is its main.
    fn new_group_section(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "NEW GROUP — NAME IT & PICK THE MAIN",
            theme::LILAC,
            |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_name)
                            .hint_text("group name")
                            .desired_width(180.0),
                    );
                    let ready = !self.new_name.trim().is_empty() && self.new_main.is_some();
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new("CREATE").color(theme::BLACK))
                                .fill(theme::LILAC),
                        )
                        .explain(
                            self.verbosity,
                            "Create the group",
                            "Create a group around the picked repository. Add the repos it \
                         should be backed up to as sinks afterwards.",
                        )
                        .clicked()
                    {
                        acts.push(Act::CreateGroup);
                    }
                });
                let candidates = self.ungrouped();
                if candidates.is_empty() {
                    ui.label(
                        RichText::new("Every repository already belongs to a group.")
                            .color(theme::TAN)
                            .size(11.0),
                    );
                    return;
                }
                crate::repo_chip::chip_row(
                    ui,
                    "sync_new_main",
                    "MAIN",
                    candidates.len(),
                    |ui, i| {
                        let name = &candidates[i];
                        let sel = self.new_main.as_deref() == Some(name.as_str());
                        let chip = crate::repo_chip::repo_chip(ui, name, sel, theme::ORANGE, None);
                        if chip
                            .name
                            .explain(
                                self.verbosity,
                                "Use as the group's main repo",
                                "The main repository is the one that gets pushed out — its content \
                         is copied to every sink in the group.",
                            )
                            .clicked()
                        {
                            acts.push(Act::SetNewMain(name.clone()));
                        }
                        chip.outer
                    },
                );
            },
        );
    }

    /// The selected group: its main, its sinks (each removable / promotable),
    /// the repos it can still take in, and its mode.
    fn members_section(
        &mut self,
        ui: &mut egui::Ui,
        name: &str,
        group: &SyncGroup,
        acts: &mut Vec<Act>,
    ) {
        crate::lcars::section_lcars(
            ui,
            &format!("{name} — MAIN, SINKS & MODE"),
            theme::BLUE,
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("MAIN").color(theme::TEXT).size(12.0));
                    let chip =
                        crate::repo_chip::repo_chip(ui, &group.main, true, theme::ORANGE, None);
                    chip.name.on_hover_text("The repository that is pushed out");
                });

                // Sinks: each can be promoted to main or taken out.
                for sink in &group.sinks {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("SINK").color(theme::TEXT).size(12.0));
                        let chip = crate::repo_chip::repo_chip(ui, sink, false, theme::BLUE, None);
                        chip.name
                            .on_hover_text("A repository the main is pushed to");
                        if crate::lcars::action_button(ui, "MAKE MAIN", true, theme::ORANGE)
                            .explain(
                                self.verbosity,
                                "Push from this one instead",
                                "Make this repository the group's main; the current main \
                                 becomes a sink, so nothing leaves the group.",
                            )
                            .clicked()
                        {
                            acts.push(Act::MakeMain(name.to_string(), sink.clone()));
                        }
                        if crate::lcars::action_button(ui, "TAKE OUT", true, theme::RED)
                            .explain(
                                self.verbosity,
                                "Remove this sink from the group",
                                "Take this repository out of the group. Its files are left \
                                 exactly as they are — only the grouping changes.",
                            )
                            .clicked()
                        {
                            acts.push(Act::RemoveSink(name.to_string(), sink.clone()));
                        }
                    });
                }

                // Repos that are still free can be taken in as sinks.
                let candidates = self.ungrouped();
                if !candidates.is_empty() {
                    crate::repo_chip::chip_row(
                        ui,
                        "sync_add_sink",
                        "ADD SINK",
                        candidates.len(),
                        |ui, i| {
                            let repo = &candidates[i];
                            let chip =
                                crate::repo_chip::repo_chip(ui, repo, false, theme::BLUE, None);
                            if chip
                                .name
                                .explain(
                                    self.verbosity,
                                    "Add as a sink",
                                    "Add this repository to the group as a sink: the main's \
                                     content is pushed to it on every sync.",
                                )
                                .clicked()
                            {
                                acts.push(Act::AddSink(name.to_string(), repo.clone()));
                            }
                            chip.outer
                        },
                    );
                }

                ui.horizontal(|ui| {
                    for (mode, label, short, verbose) in [
                        (
                            SyncMode::AddOnly,
                            "ADD ONLY",
                            "Only copy, never delete",
                            "Copy content the sink lacks and never delete anything from it. \
                             A sink may keep files the main no longer has.",
                        ),
                        (
                            SyncMode::Mirror,
                            "MIRROR",
                            "Make the sinks match the main exactly",
                            "Copy content the sink lacks AND delete sink content the main \
                             does not have, so each sink ends up holding exactly the main's \
                             content. Deletions cannot be undone.",
                        ),
                    ] {
                        let selected = group.mode == mode;
                        let accent = if mode == SyncMode::Mirror {
                            theme::RED
                        } else {
                            theme::GREEN
                        };
                        if crate::lcars::toggle_button(ui, label, selected, accent)
                            .explain(self.verbosity, short, verbose)
                            .clicked()
                        {
                            acts.push(Act::SetMode(name.to_string(), mode));
                        }
                    }
                    if crate::lcars::action_button(ui, "DELETE GROUP", true, theme::RED)
                        .explain(
                            self.verbosity,
                            "Delete this group",
                            "Delete the group. Every repository in it stays exactly as it \
                             is — only the grouping is forgotten.",
                        )
                        .clicked()
                    {
                        acts.push(Act::DeleteGroup(name.to_string()));
                    }
                });
                if group.sinks.is_empty() {
                    ui.label(
                        RichText::new(
                            "Add at least one sink — a group with no sink has nothing to push to.",
                        )
                        .color(theme::TAN)
                        .size(11.0),
                    );
                }
            },
        );
    }

    fn action_section(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "ACTION — REVIEW & RUN SYNC", theme::AMBER, |ui| {
            ui.horizontal(|ui| {
                let ready = !self.running
                    && !self.previewing
                    && self
                        .selected_group()
                        .is_some_and(|(_, g)| !g.sinks.is_empty());
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(RichText::new("REVIEW").color(theme::BLACK)),
                    )
                    .explain(
                        self.verbosity,
                        "Review the next push",
                        "Show what a push would copy into each sink (and, in MIRROR mode, \
                         what it would delete there) without touching disk.",
                    )
                    .clicked()
                {
                    acts.push(Act::Preview);
                }
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(RichText::new("RUN SYNC").color(theme::BLACK))
                            .fill(theme::AMBER),
                    )
                    .explain(
                        self.verbosity,
                        "Push the main to every sink",
                        "Push the main repository to every sink in the group, after a \
                         confirmation. Each sink is handled independently — one failing \
                         sink does not stop the others.",
                    )
                    .clicked()
                {
                    acts.push(Act::Ask);
                }
                if self.previewing {
                    // Planning takes no cancel token, so no CANCEL — a button
                    // that does nothing is worse than none.
                    ui.add(egui::Spinner::new().color(theme::AMBER));
                }
                if self.running {
                    ui.add(egui::Spinner::new().color(theme::AMBER));
                    if ui
                        .add(
                            egui::Button::new(RichText::new("CANCEL").color(theme::BLACK))
                                .fill(theme::RED),
                        )
                        .explain(
                            self.verbosity,
                            "Stop the push",
                            "Stop after the sink currently being pushed. Files already \
                             copied stay as they are; this does not roll back.",
                        )
                        .clicked()
                    {
                        acts.push(Act::CancelRun);
                    }
                }
            });
        });
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        if self.preview.is_empty() {
            ui.add_space(6.0);
            ui.colored_label(
                theme::TEXT,
                "Pick a group and press REVIEW to see what its next push would do.",
            );
            return;
        }
        review::table(
            ui,
            &mut self.review_state,
            &mut self.preview,
            self.preview_totals,
            &self.preview_main_header,
            // A group push is always two-sided (main → sinks).
            Some(&self.preview_sink_header),
            review::RowControls::ReadOnly,
        );
    }

    fn confirm_modal(&mut self, ui: &mut egui::Ui, prompt: &str, acts: &mut Vec<Act>) {
        egui::Modal::new(Id::new("sync-confirm")).show(&ui.ctx().clone(), |ui| {
            ui.set_width(420.0);
            ui.label(
                RichText::new("CONFIRM SYNC")
                    .color(theme::AMBER)
                    .size(16.0)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.colored_label(theme::TEXT, prompt);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let mirror = self
                    .selected_group()
                    .is_some_and(|(_, g)| g.mode == SyncMode::Mirror);
                let fill = if mirror { theme::RED } else { theme::AMBER };
                if ui
                    .add(egui::Button::new(RichText::new("PROCEED").color(theme::BLACK)).fill(fill))
                    .clicked()
                {
                    acts.push(Act::Confirm);
                }
                if ui
                    .button(RichText::new("CANCEL").color(theme::BLACK))
                    .clicked()
                {
                    acts.push(Act::CancelConfirm);
                }
            });
        });
    }

    fn apply(&mut self, store: &Arc<Store>, act: Act) {
        // Only membership changes invalidate the cached repo/group lists.
        // Reloading after every act would also wipe an error a preview or a
        // refused push just reported, since `sync_repos` clears it on success.
        let reload = matches!(
            act,
            Act::CreateGroup
                | Act::DeleteGroup(_)
                | Act::AddSink(..)
                | Act::RemoveSink(..)
                | Act::MakeMain(..)
                | Act::SetMode(..)
        );
        let result = match act {
            Act::Select(name) => {
                self.selected = Some(name);
                self.clear_preview();
                Ok(())
            }
            Act::SetNewMain(repo) => {
                self.new_main = Some(repo);
                Ok(())
            }
            Act::CreateGroup => {
                let name = self.new_name.trim().to_string();
                match self.new_main.clone() {
                    Some(main) => store
                        .create_sync_group(&name, &main, SyncMode::AddOnly)
                        .map(|()| {
                            self.new_name.clear();
                            self.new_main = None;
                            self.selected = Some(name);
                        }),
                    None => Ok(()),
                }
            }
            Act::DeleteGroup(name) => store.delete_sync_group(&name).map(|()| {
                self.selected = None;
                self.clear_preview();
            }),
            Act::AddSink(group, repo) => store.add_sync_sink(&group, &repo).map(|()| {
                self.clear_preview();
            }),
            Act::RemoveSink(group, repo) => store.remove_sync_sink(&group, &repo).map(|()| {
                self.clear_preview();
            }),
            Act::MakeMain(group, repo) => store.set_sync_main(&group, &repo).map(|()| {
                self.clear_preview();
            }),
            Act::SetMode(group, mode) => store.set_sync_mode(&group, mode).map(|()| {
                self.clear_preview();
            }),
            Act::Preview => {
                self.spawn_preview(store, false);
                Ok(())
            }
            Act::Ask => {
                // Plan on a worker thread, then raise the confirmation when it
                // lands (the `confirm` flag rides through). A refused or failing
                // plan — an empty MIRROR main, say — surfaces as an error and
                // never reaches the dialog.
                self.spawn_preview(store, true);
                Ok(())
            }
            Act::CancelConfirm => {
                self.confirm = None;
                self.pending_push = None;
                Ok(())
            }
            Act::Confirm => {
                self.confirm = None;
                self.start(store);
                Ok(())
            }
            Act::CancelRun => {
                self.cancel.cancel();
                Ok(())
            }
        };
        match result {
            // Membership changed under us? Re-read, so the UI always shows
            // what the registry actually holds.
            Ok(()) if reload => self.sync_repos(store),
            Ok(()) => {}
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn clear_preview(&mut self) {
        self.pending_push = None;
        self.preview.clear();
        self.preview_totals = [0; 3];
        self.preview_main_header.clear();
        self.preview_sink_header.clear();
        self.wholesale_sinks.clear();
        self.review_state.rejected.clear();
    }

    /// Plan the selected group on a worker thread. `confirm` carries through to
    /// the result: when set, the RUN confirmation is raised once the plan lands.
    /// The full index scan runs off the UI thread so a large group does not
    /// freeze the window.
    fn spawn_preview(&mut self, store: &Arc<Store>, confirm: bool) {
        let Some((name, group)) = self.selected_group() else {
            return;
        };
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.previewing = true;
        self.status = Some("planning…".to_string());
        std::thread::spawn(move || {
            let result = build_preview(&store, &name, &group);
            let _ = tx.send(Msg::Preview { result, confirm });
        });
    }

    /// Fold a finished plan into the board and, if this plan was for a RUN
    /// click, raise the confirmation now that the real counts are known.
    fn apply_preview(&mut self, result: Result<PreviewOutcome, String>, confirm: bool) {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.preview_totals = [outcome.added, outcome.removed, 0];
        self.preview_main_header = outcome.main_header;
        self.preview_sink_header = "SINKS".to_string();
        self.wholesale_sinks = outcome.wholesale_sinks;
        let mut rows = outcome.rows;
        review::sort(&mut rows, &self.review_state);
        self.preview = rows;
        self.status = Some(format!(
            "{} file(s) to copy, {} to delete across {} sink(s).",
            outcome.added, outcome.removed, outcome.sink_count
        ));
        self.error = None;
        if confirm {
            // Confirm and push the group that was *planned*, captured here — the
            // selection may have changed while the scan ran.
            self.raise_confirm(&outcome.group_name, &outcome.group);
            self.pending_push = Some((outcome.group_name, outcome.group));
        }
    }

    /// Build the RUN confirmation from the plan just applied. A confirmation
    /// that cannot say how much it deletes is not one the user can weigh.
    fn raise_confirm(&mut self, name: &str, group: &SyncGroup) {
        let [copies, deletes, _] = self.preview_totals;
        let mut prompt = format!(
            "Push '{}' to {} sink(s) of group '{name}': copy {copies} file(s)",
            group.main,
            group.sinks.len()
        );
        if group.mode == SyncMode::Mirror {
            prompt.push_str(&format!(
                " and DELETE {deletes} file(s) from the sink(s), which cannot be undone"
            ));
        }
        prompt.push_str(". The main is never changed.");
        if !self.wholesale_sinks.is_empty() {
            // Say what actually happens: nothing the sink holds today survives,
            // and the main's content takes its place. It is not left empty —
            // claiming that would be false, and a confirmation nobody trusts is
            // worse than none.
            let listed: Vec<String> = self
                .wholesale_sinks
                .iter()
                .map(|(sink, live)| format!("{sink} (all {live} of its files)"))
                .collect();
            prompt.push_str(&format!(
                "\n\nWARNING: this replaces the entire current contents of {} with the main's \
                 content — nothing they hold today survives. If that is not what you expect, \
                 check the main is complete first.",
                listed.join(", ")
            ));
        }
        self.confirm = Some(prompt);
    }

    fn start(&mut self, store: &Arc<Store>) {
        // Push the group the confirmation was built for, not the current
        // selection — see `pending_push`.
        let Some((name, group)) = self.pending_push.take() else {
            return;
        };
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        self.running = true;
        self.status = Some(format!("syncing '{name}'…"));
        // The previous run's report is about to be superseded.
        self.result.close();
        self.clear_preview();

        std::thread::spawn(move || {
            let progress = CollectProblems::new();
            let run = DiffRun::new(&progress, &cancel);
            let results = match run_group_sync(&store, &group, &run) {
                Ok(results) => results,
                // The group was refused outright (e.g. an empty MIRROR main):
                // nothing was attempted, so say so plainly.
                Err(e) => {
                    let _ = tx.send(Msg::Done(Err(e.to_string())));
                    return;
                }
            };
            let (mut copied, mut deleted, mut file_errors) = (0u64, 0u64, 0u64);
            let mut failures = Vec::new();
            let mut skipped = Vec::new();
            let mut cancelled = false;
            for (sink, outcome) in results {
                match outcome {
                    SinkOutcome::Pushed(stats) => {
                        copied += stats.copied;
                        deleted += stats.deleted;
                        file_errors += stats.errors;
                        cancelled |= stats.cancelled;
                    }
                    SinkOutcome::Failed(e) => failures.push(format!("{sink}: {e}")),
                    SinkOutcome::Skipped => {
                        cancelled = true;
                        skipped.push(sink);
                    }
                }
            }
            // Every way a push can fall short of "done" has to reach the user —
            // a backup that silently did nothing is worse than one that failed
            // loudly. The individual per-file failures come from the progress
            // sink; the whole-sink ones from the outcomes.
            let mut report = crate::run_result::RunReport::new(format!("Sync group '{name}'"))
                .count("copied", copied)
                .count("deleted", deleted)
                .cancelled(cancelled)
                .problems(progress.take())
                .problems(failures);
            if !skipped.is_empty() {
                report = report.note(format!(
                    "{} sink(s) were never pushed and are now stale: {}",
                    skipped.len(),
                    skipped.join(", ")
                ));
            }
            // `stats.errors` counts failures the progress sink may have capped;
            // trust the count, and let the panel say how many it is showing.
            if file_errors > report.problem_count() {
                report = report.count("files that failed to copy", file_errors);
            }
            let _ = tx.send(Msg::Done(Ok(report)));
        });
    }

    fn drain(&mut self, ui: &egui::Ui) {
        let mut got = false;
        while let Ok(msg) = self.rx.try_recv() {
            got = true;
            match msg {
                Msg::Preview { result, confirm } => {
                    self.previewing = false;
                    self.apply_preview(result, confirm);
                }
                Msg::Done(Ok(report)) => {
                    self.running = false;
                    let headline = report.headline();
                    if report.complete() {
                        log::info!("sync group push: {headline}");
                    } else {
                        log::warn!("sync group push: {headline}");
                        for problem in report.problem_lines() {
                            log::warn!("  {problem}");
                        }
                    }
                    self.status = Some(headline);
                    self.error = None;
                    // The panel carries the detail the status line cannot.
                    self.result.open(report);
                }
                Msg::Done(Err(e)) => {
                    self.running = false;
                    log::error!("sync group push: {e}");
                    self.error = Some(e);
                }
            }
        }
        if got || self.running || self.previewing {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    /// A store with MAIN, SINK1 and SINK2 registered over real (empty) dirs.
    fn sample_store() -> (tempfile::TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::open_at(tmp.path().join("cfg")).expect("store");
        for name in ["PRIMARY", "BACKUP1", "BACKUP2"] {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).expect("repo dir");
            store
                .create_repo(name, &dir.to_string_lossy())
                .expect("create repo");
        }
        (tmp, Arc::new(store))
    }

    fn harness(store: Arc<Store>) -> Harness<'static, SyncView> {
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 900.0))
            .build_ui_state(
                move |ui, view: &mut SyncView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                SyncView::new(),
            );
        harness.run();
        harness
    }

    /// Pump frames until the current worker thread (preview or push) has
    /// delivered its result and the view is idle again. Preview and RUN plan
    /// off the UI thread now, so a click's own `run()` returns before the
    /// result arrives; the tiny test repos finish near-instantly.
    fn settle(h: &mut Harness<'static, SyncView>) {
        for _ in 0..100 {
            h.run();
            if !h.state().running && !h.state().previewing {
                // One more frame so the result (confirm dialog, board) renders.
                h.run();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("worker thread did not settle");
    }

    /// With no groups yet the tab explains itself and offers the NEW GROUP
    /// form; creating a group selects it and shows its members and mode.
    #[test]
    fn creating_a_group_shows_its_main_sinks_and_mode() {
        let (_tmp, store) = sample_store();
        let mut h = harness(Arc::clone(&store));
        assert!(
            h.query_by_label_contains("No sync groups yet").is_some(),
            "an empty tab says so"
        );

        h.get_by_label("PRIMARY").click();
        h.run();
        h.state_mut().new_name = "offsite".to_string();
        h.run();
        h.get_by_label("CREATE").click();
        h.run();

        assert_eq!(
            store.list_sync_groups().expect("list").len(),
            1,
            "the group is in the registry"
        );
        assert_eq!(
            h.state().selected.as_deref(),
            Some("offsite"),
            "the new group is selected"
        );
        assert!(
            h.query_by_label("ADD ONLY").is_some() && h.query_by_label("MIRROR").is_some(),
            "the mode toggles show"
        );
        assert!(
            h.query_by_label_contains("Add at least one sink").is_some(),
            "a group with no sink says what it needs"
        );
    }

    /// Adding a sink, promoting it, and switching the mode all land in the
    /// registry — the tab is a view onto the store, not its own state.
    #[test]
    fn members_and_mode_changes_are_stored() {
        let (_tmp, store) = sample_store();
        store
            .create_sync_group("offsite", "PRIMARY", SyncMode::AddOnly)
            .expect("create group");
        let mut h = harness(Arc::clone(&store));
        h.get_by_label("offsite").click();
        h.run();

        // An ungrouped repo is offered twice — as a candidate main for a NEW
        // GROUP and as a sink for this one. The ADD SINK row renders second.
        match h.get_all_by_label("BACKUP1").last() {
            Some(chip) => chip.click(),
            None => panic!("BACKUP1 is not offered as a sink"),
        }
        h.run();
        assert_eq!(
            store.get_sync_group("offsite").expect("group").sinks,
            ["BACKUP1"]
        );

        h.get_by_label("MIRROR").click();
        h.run();
        assert_eq!(
            store.get_sync_group("offsite").expect("group").mode,
            SyncMode::Mirror,
            "the mode is per group and persisted"
        );

        h.get_by_label("MAKE MAIN").click();
        h.run();
        let group = store.get_sync_group("offsite").expect("group");
        assert_eq!(group.main, "BACKUP1");
        assert_eq!(group.sinks, ["PRIMARY"], "the old main stays as a sink");

        h.get_by_label("TAKE OUT").click();
        h.run();
        assert!(
            store
                .get_sync_group("offsite")
                .expect("group")
                .sinks
                .is_empty(),
            "the sink left the group"
        );
    }

    /// REVIEW plans every sink and renders the result in the shared review
    /// board — copies on the sink side, mirror deletions marked removed.
    #[test]
    fn preview_shows_what_the_push_would_do() {
        let (tmp, store) = sample_store();
        std::fs::write(tmp.path().join("PRIMARY/a.txt"), b"alpha").expect("write");
        std::fs::write(tmp.path().join("BACKUP1/gone.txt"), b"only in sink").expect("write");
        for repo in ["PRIMARY", "BACKUP1"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        store
            .create_sync_group("offsite", "PRIMARY", SyncMode::Mirror)
            .expect("create group");
        store.add_sync_sink("offsite", "BACKUP1").expect("add sink");

        let mut h = harness(Arc::clone(&store));
        h.get_by_label("offsite").click();
        h.run();
        h.get_by_label("REVIEW").click();
        settle(&mut h);

        assert_eq!(
            h.state().preview_totals,
            [1, 1, 0],
            "one copy into the sink, one mirror deletion"
        );
        assert!(
            h.query_by_label_contains("BACKUP1: a.txt").is_some(),
            "the copy names the sink it goes to"
        );
        assert!(
            h.query_by_label_contains("BACKUP1: gone.txt").is_some(),
            "so does the deletion"
        );
    }

    /// The sync preview is read-only: a push is all-or-nothing, so the board
    /// must not offer per-row reject toggles it cannot honour — promising to
    /// skip a MIRROR deletion and then making it anyway would lose a file.
    #[test]
    fn the_sync_preview_offers_no_per_row_controls() {
        let (tmp, store) = sample_store();
        std::fs::write(tmp.path().join("PRIMARY/a.txt"), b"alpha").expect("write");
        std::fs::write(tmp.path().join("BACKUP1/gone.txt"), b"only in sink").expect("write");
        for repo in ["PRIMARY", "BACKUP1"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        store
            .create_sync_group("offsite", "PRIMARY", SyncMode::Mirror)
            .expect("create group");
        store.add_sync_sink("offsite", "BACKUP1").expect("add sink");

        let mut h = harness(Arc::clone(&store));
        h.get_by_label("offsite").click();
        h.run();
        h.get_by_label("REVIEW").click();
        settle(&mut h);

        assert!(
            h.query_by_label_contains("BACKUP1: gone.txt").is_some(),
            "the deletion is listed"
        );
        assert_eq!(
            h.query_all_by_label("ACTIONS").count(),
            0,
            "no per-row action column"
        );
        assert_eq!(
            h.query_all_by_label(crate::icon::ARROW_RIGHT).count(),
            0,
            "no single-row APPLY on a board whose run is all-or-nothing"
        );
    }

    /// RUN SYNC always confirms first, and the prompt carries the real counts —
    /// "deletions cannot be undone" is not something a user can weigh without
    /// knowing how many.
    #[test]
    fn run_sync_asks_before_pushing_and_says_how_much() {
        let (tmp, store) = sample_store();
        std::fs::write(tmp.path().join("PRIMARY/a.txt"), b"alpha").expect("write");
        std::fs::write(tmp.path().join("BACKUP1/gone.txt"), b"only in sink").expect("write");
        for repo in ["PRIMARY", "BACKUP1"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        store
            .create_sync_group("offsite", "PRIMARY", SyncMode::Mirror)
            .expect("create group");
        store.add_sync_sink("offsite", "BACKUP1").expect("add sink");
        let mut h = harness(Arc::clone(&store));
        h.get_by_label("offsite").click();
        h.run();

        h.get_by_label("RUN SYNC").click();
        settle(&mut h);
        assert!(
            h.query_by_label_contains("CONFIRM SYNC").is_some(),
            "a push is always confirmed"
        );
        assert!(
            h.query_by_label_contains("copy 1 file(s)").is_some(),
            "the prompt counts the copies"
        );
        assert!(
            h.query_by_label_contains("DELETE 1 file(s)").is_some(),
            "and the deletions, so MIRROR can be weighed"
        );
        // BACKUP1's only file goes, so this is a wholesale replacement — but the
        // main's content is copied in, so the sink is NOT left empty. The
        // warning must say the former and never claim the latter.
        assert!(
            h.query_by_label_contains("replaces the entire current contents")
                .is_some(),
            "a push that drops everything the sink holds says so"
        );
        assert!(
            h.query_by_label_contains("empties").is_none(),
            "and never claims the sink ends up empty — the main's content replaces it"
        );
        h.get_by_label("CANCEL").click();
        h.run();
        assert!(h.state().confirm.is_none(), "cancelling closes the dialog");
        assert!(!h.state().running, "and nothing was pushed");
    }

    /// The plan is built off-thread, so the selection can change before it
    /// lands. The confirmation and the push must describe the group that was
    /// *planned*, never whatever is selected when the result arrives —
    /// otherwise a MIRROR confirm could name one group while another is pushed.
    #[test]
    fn confirm_targets_the_planned_group_not_the_current_selection() {
        let mut view = SyncView::new();
        let planned = SyncGroup {
            main: "A_MAIN".to_string(),
            sinks: vec!["A_SINK".to_string()],
            mode: SyncMode::AddOnly,
        };
        let outcome = PreviewOutcome {
            group_name: "groupA".to_string(),
            group: planned,
            rows: Vec::new(),
            added: 3,
            removed: 0,
            sink_count: 1,
            main_header: "A_MAIN".to_string(),
            wholesale_sinks: Vec::new(),
        };
        // The user has since clicked another group; its plan is what lands.
        view.selected = Some("groupB".to_string());
        view.apply_preview(Ok(outcome), true);

        assert!(
            view.confirm
                .as_deref()
                .unwrap_or_default()
                .contains("A_MAIN"),
            "the confirmation names the planned group's main, not the selection"
        );
        let (name, group) = view.pending_push.as_ref().expect("a push is pending");
        assert_eq!(name, "groupA", "PROCEED will push the planned group");
        assert_eq!(group.main, "A_MAIN");
    }

    /// A MIRROR whose main holds nothing would delete every file in the sink.
    /// RUN SYNC must refuse outright rather than offer a confirmation — the
    /// main being empty is virtually always an unscanned or unmounted drive.
    #[test]
    fn run_sync_refuses_to_mirror_from_an_empty_main() {
        let (tmp, store) = sample_store();
        std::fs::write(tmp.path().join("BACKUP1/precious.txt"), b"the only copy").expect("write");
        // Only the sink is scanned: PRIMARY's index stays empty.
        dedup_core::update::update_repo(
            &store,
            "BACKUP1",
            1,
            &dedup_core::update::NoProgress,
            &CancellationToken::new(),
        )
        .expect("scan");
        store
            .create_sync_group("offsite", "PRIMARY", SyncMode::Mirror)
            .expect("create group");
        store.add_sync_sink("offsite", "BACKUP1").expect("add sink");
        let mut h = harness(Arc::clone(&store));
        h.get_by_label("offsite").click();
        h.run();

        h.get_by_label("RUN SYNC").click();
        settle(&mut h);
        assert!(
            h.state().confirm.is_none(),
            "no confirmation is offered for a push that would wipe the sink"
        );
        assert!(!h.state().running, "and nothing is pushed");
        assert!(
            h.query_by_label_contains("has no indexed files").is_some(),
            "the refusal explains itself"
        );
        assert!(
            tmp.path().join("BACKUP1/precious.txt").exists(),
            "the sink still holds its content"
        );
    }

    /// Render snapshot of the tab: `cargo test -p dedup-gui render_sync_groups
    /// -- --ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_sync_groups() {
        let (_tmp, store) = sample_store();
        store
            .create_sync_group("offsite", "PRIMARY", SyncMode::Mirror)
            .expect("create group");
        store.add_sync_sink("offsite", "BACKUP1").expect("add sink");
        let mut h = Harness::builder()
            .with_size(egui::vec2(1120.0, 900.0))
            .wgpu()
            .build_ui_state(
                {
                    let store = Arc::clone(&store);
                    let mut init = false;
                    move |ui, view: &mut SyncView| {
                        if !init {
                            crate::icon::install(ui.ctx());
                            crate::theme::apply(ui.ctx());
                            init = true;
                        }
                        view.show(ui, &store, TooltipVerbosity::default());
                    }
                },
                SyncView::new(),
            );
        h.run();
        h.get_by_label("offsite").click();
        h.run();
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/sync_groups.png");
        let img = h.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}

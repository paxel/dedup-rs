//! The File Management tab: pick a source repo and a target repo, choose a
//! command (copy / move / delete), narrow with a filter, preview the first
//! `from → to` transfers, then run it on a background thread with confirmation.
//!
//! Semantics reuse the core diff operations (content compared by size + hash):
//! - **Copy/Move** transfer source files whose content the target lacks into the
//!   target repo's directory (move also marks the source entries missing).
//! - **Delete** removes source files whose content the target already has.

use crate::icon;
use crate::theme;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{
    CopyDest, DiffAction, DiffEvent, DiffItem, DiffProgress, DiffRun, diff_copy, diff_delete,
    diff_print,
};
use dedup_core::filter::{FileFilter, count_matches};
use dedup_core::store::Store;
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long after the last edit the NAME match count is (re)computed.
const COUNT_DEBOUNCE: Duration = Duration::from_millis(350);

const PREVIEW_LIMIT: usize = 30;
/// How many recent actions the running panel keeps in its scrolling log.
const RUN_LOG_LIMIT: usize = 10;

#[derive(PartialEq, Clone, Copy)]
enum Command {
    Copy,
    Move,
    Delete,
}

impl Command {
    fn label(self) -> &'static str {
        match self {
            Command::Copy => "COPY",
            Command::Move => "MOVE",
            Command::Delete => "DELETE",
        }
    }
    fn destructive(self) -> bool {
        !matches!(self, Command::Copy)
    }
}

struct PreviewRow {
    from: String,
    to: String,
}

/// The kind of a single filter condition. Maps one-to-one to the `mime:` /
/// `name:` / `size:` prefixes understood by `dedup_core::filter::FileFilter`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterKind {
    Mime,
    Name,
    Size,
}

impl FilterKind {
    fn label(self) -> &'static str {
        match self {
            FilterKind::Mime => "MIME",
            FilterKind::Name => "NAME",
            FilterKind::Size => "SIZE",
        }
    }

    /// The lowercase prefix used in the generated filter expression. Also used
    /// as the stable serialized tag for a saved condition.
    fn prefix(self) -> &'static str {
        match self {
            FilterKind::Mime => "mime",
            FilterKind::Name => "name",
            FilterKind::Size => "size",
        }
    }

    /// Parse a kind back from its serialized [`prefix`](Self::prefix) tag.
    fn from_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "mime" => Some(FilterKind::Mime),
            "name" => Some(FilterKind::Name),
            "size" => Some(FilterKind::Size),
            _ => None,
        }
    }

    fn hint(self) -> &'static str {
        match self {
            FilterKind::Mime => "image/",
            FilterKind::Name => "substring",
            FilterKind::Size => ">=1000",
        }
    }
}

/// A single active filter condition in the builder. Exactly one condition may
/// be `editing` at a time, which reveals its inline editor panel.
struct FilterCond {
    kind: FilterKind,
    value: String,
    editing: bool,
}

/// How many recent values per kind are remembered and offered as quick-picks.
const HISTORY_LIMIT: usize = 12;

/// File name of the persisted filter history inside the store's config dir.
const HISTORY_FILE: &str = "filter_history.json";

/// A single saved condition inside a named preset. `kind` is the stable
/// prefix tag (`mime` / `name` / `size`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SavedCond {
    kind: String,
    value: String,
}

/// A named filter preset: a whole condition set the user saved for reuse.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct FilterPreset {
    name: String,
    conds: Vec<SavedCond>,
}

/// Remembered values the user has entered plus named presets, offered as
/// one-click quick options and persisted as JSON in the config dir.
#[derive(Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct FilterHistory {
    mime: Vec<String>,
    name: Vec<String>,
    size: Vec<String>,
    presets: Vec<FilterPreset>,
}

impl FilterHistory {
    fn for_kind(&self, kind: FilterKind) -> &Vec<String> {
        match kind {
            FilterKind::Mime => &self.mime,
            FilterKind::Name => &self.name,
            FilterKind::Size => &self.size,
        }
    }

    fn for_kind_mut(&mut self, kind: FilterKind) -> &mut Vec<String> {
        match kind {
            FilterKind::Mime => &mut self.mime,
            FilterKind::Name => &mut self.name,
            FilterKind::Size => &mut self.size,
        }
    }

    /// Record a value at the front of its kind's list, de-duplicating and
    /// capping the list length. Blank values are ignored. Returns whether the
    /// list actually changed.
    fn record_value(&mut self, kind: FilterKind, value: &str) -> bool {
        let value = value.trim();
        if value.is_empty() {
            return false;
        }
        let list = self.for_kind_mut(kind);
        if list.first().map(String::as_str) == Some(value) {
            return false;
        }
        list.retain(|v| v != value);
        list.insert(0, value.to_string());
        list.truncate(HISTORY_LIMIT);
        true
    }

    /// Load the history from `path`, falling back to an empty history when the
    /// file is absent or unreadable (a corrupt history must never break the UI).
    fn load(path: &std::path::Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())
    }

    /// Merge an imported history into this one: imported values are recorded
    /// (keeping their order at the front) and imported presets replace
    /// same-named local presets or are appended.
    fn merge(&mut self, other: FilterHistory) {
        for kind in [FilterKind::Mime, FilterKind::Name, FilterKind::Size] {
            for value in other.for_kind(kind).iter().rev() {
                self.record_value(kind, value);
            }
        }
        for preset in other.presets {
            match self.presets.iter_mut().find(|p| p.name == preset.name) {
                Some(existing) => *existing = preset,
                None => self.presets.push(preset),
            }
        }
    }
}

/// Result of a background export/import file dialog, reported to the UI thread.
enum HistoryIo {
    Imported(FilterHistory),
    Exported(PathBuf),
    Failed(String),
}

enum OpResult {
    Copied {
        copied: u64,
        cancelled: bool,
        moved: bool,
    },
    Deleted {
        deleted: u64,
        cancelled: bool,
    },
    Error(String),
}

/// Messages flowing from the worker thread to the UI thread: live per-file
/// progress events plus the single terminal result.
enum Msg {
    Progress(DiffEvent),
    Done(OpResult),
}

/// [`DiffProgress`] adapter that forwards every diff event onto the FilesView
/// channel. Sends never block; a dropped receiver is fine.
struct ChannelDiffProgress {
    tx: Sender<Msg>,
}

impl DiffProgress for ChannelDiffProgress {
    fn on(&self, event: DiffEvent) {
        let _ = self.tx.send(Msg::Progress(event));
    }
}

pub struct FilesView {
    repos: Vec<String>,
    loaded: bool,
    source: Option<String>,
    target: Option<String>,
    /// Extra reference repos beyond the target: a file counts as "new" only
    /// when neither the target nor any of these already has its content.
    extra_refs: Vec<String>,
    command: Command,
    subdir: String,
    subdir_tx: Sender<Result<String, String>>,
    subdir_rx: Receiver<Result<String, String>>,
    filters: Vec<FilterCond>,
    /// Whether the `+` type picker is currently expanded.
    adding: bool,
    /// Remembered values offered as quick-picks in the condition editors.
    /// Loaded from / saved to `HISTORY_FILE` in the store's config dir.
    history: FilterHistory,
    /// Name typed for the preset about to be saved.
    preset_name: String,
    // Results of the background export/import file dialogs.
    io_tx: Sender<HistoryIo>,
    io_rx: Receiver<HistoryIo>,
    // Debounced background NAME match count against the source repo index.
    // A `None` count means the count failed (unparsable filter or unreadable
    // index).
    count_tx: Sender<(u64, Option<usize>)>,
    count_rx: Receiver<(u64, Option<usize>)>,
    /// Generation token so stale background counts are discarded.
    count_token: u64,
    /// When set, a count is (re)launched once this deadline passes.
    count_deadline: Option<Instant>,
    /// Whether a launched count has not reported back yet.
    count_in_flight: bool,
    /// The latest match count, if one has been computed.
    count_result: Option<usize>,
    /// The source repo's MIME stats, cached for the MIME editor's suggestions;
    /// refreshed when the source changes (fetching opens the repo database, so
    /// it must not happen every frame).
    mime_stats: Vec<(String, u64)>,
    preview: Vec<PreviewRow>,
    preview_total: usize,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<String>,
    running: bool,
    cancel: CancellationToken,
    // Live run progress: the last N actions, the running counters and the
    // file currently being handled.
    run_log: VecDeque<String>,
    run_done: u64,
    run_total: u64,
    run_current: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

enum Act {
    PickSource(String),
    PickTarget(String),
    ToggleExtraRef(String),
    MarkSourceDone,
    SetCommand(Command),
    SubdirChanged,
    FilterChanged,
    AddCond(FilterKind),
    RemoveCond(usize),
    EditCond(usize),
    CommitCond,
    ClearConds,
    SavePreset,
    ApplyPreset(usize),
    RemovePreset(usize),
    ExportHistory,
    ImportHistory,
    BrowseSubdir,
    Reload,
    Preview,
    Ask,
    Confirm,
    CancelConfirm,
    CancelRun,
}

impl FilesView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (subdir_tx, subdir_rx) = crossbeam_channel::unbounded();
        let (count_tx, count_rx) = crossbeam_channel::unbounded();
        let (io_tx, io_rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
            loaded: false,
            source: None,
            target: None,
            extra_refs: Vec::new(),
            command: Command::Copy,
            subdir: String::new(),
            subdir_tx,
            subdir_rx,
            filters: Vec::new(),
            adding: false,
            history: FilterHistory::default(),
            preset_name: String::new(),
            io_tx,
            io_rx,
            count_tx,
            count_rx,
            count_token: 0,
            count_deadline: None,
            count_in_flight: false,
            count_result: None,
            mime_stats: Vec::new(),
            preview: Vec::new(),
            preview_total: 0,
            status: None,
            error: None,
            confirm: None,
            running: false,
            cancel: CancellationToken::new(),
            run_log: VecDeque::new(),
            run_done: 0,
            run_total: 0,
            run_current: String::new(),
            tx,
            rx,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>) {
        self.drain(ui, store);
        if !self.loaded {
            self.reload(store);
        }
        self.maybe_launch_count(store, &ui.ctx().clone());

        let mut acts: Vec<Act> = Vec::new();
        ui.add_space(6.0);
        ui.label(
            RichText::new("FILE MANAGEMENT")
                .color(theme::BLUE)
                .size(18.0)
                .strong(),
        );

        self.repo_rows(ui, &mut acts);
        self.command_bar(ui, &mut acts);
        self.subdir_bar(ui, &mut acts);
        self.filter_bar(ui, &mut acts);
        self.action_bar(ui, &mut acts);

        if let Some(err) = &self.error {
            ui.colored_label(theme::RED, err);
        }
        if let Some(status) = &self.status {
            ui.label(RichText::new(status).color(theme::TAN).size(13.0));
        }
        ui.separator();
        // RUN and PREVIEW are mutually exclusive: while a run is active or has
        // left a log, show the live run panel; otherwise show the preview.
        if self.running || !self.run_log.is_empty() {
            self.run_panel(ui);
        } else {
            self.preview_panel(ui);
        }

        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }

        let ctx = ui.ctx().clone();
        for act in acts {
            self.apply(store, &ctx, act);
        }
    }

    fn reload(&mut self, store: &Store) {
        // The persisted history is read once, on the first load; afterwards the
        // in-memory copy is authoritative and written back on every change.
        if !self.loaded {
            self.history = FilterHistory::load(&store.config_dir().join(HISTORY_FILE));
        }
        match store.list_repos() {
            Ok(list) => {
                self.repos = list.into_iter().map(|(n, _, _)| n).collect();
                if let Some(s) = &self.source
                    && !self.repos.contains(s)
                {
                    self.source = None;
                }
                if let Some(t) = &self.target
                    && !self.repos.contains(t)
                {
                    self.target = None;
                }
                self.loaded = true;
                self.error = None;
                self.refresh_mime_stats(store);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// (Re)fetch the cached MIME stats for the current source repo, or clear
    /// them when none is selected. Failures just leave the suggestions empty.
    fn refresh_mime_stats(&mut self, store: &Store) {
        self.mime_stats = match &self.source {
            Some(source) => store.get_mime_stats(source).unwrap_or_default(),
            None => Vec::new(),
        };
    }

    fn repo_rows(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::LILAC).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("SOURCE").color(theme::TEXT).size(12.0));
                for name in &self.repos {
                    let sel = self.source.as_deref() == Some(name.as_str());
                    let fill = if sel { theme::ORANGE } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::TEXT };
                    if ui
                        .add(egui::Button::new(RichText::new(name).color(col)).fill(fill))
                        .clicked()
                    {
                        acts.push(Act::PickSource(name.clone()));
                    }
                }
                if ui
                    .button(RichText::new(icon::REFRESH).color(theme::BLACK))
                    .clicked()
                {
                    acts.push(Act::Reload);
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("TARGET").color(theme::TEXT).size(12.0));
                for name in &self.repos {
                    // The target is chosen from the repos that are not the source.
                    if self.source.as_deref() == Some(name.as_str()) {
                        continue;
                    }
                    let sel = self.target.as_deref() == Some(name.as_str());
                    let fill = if sel { theme::BLUE } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::BLUE };
                    if ui
                        .add(egui::Button::new(RichText::new(name).color(col)).fill(fill))
                        .clicked()
                    {
                        acts.push(Act::PickTarget(name.clone()));
                    }
                }
            });
            // Optional extra reference repos: content present in any of them is
            // treated as "already known" (so it is not copied / is deletable).
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("ALSO REF").color(theme::TEXT).size(12.0));
                for name in &self.repos {
                    // Extra refs exclude the source and target (target is always
                    // a reference already).
                    if self.source.as_deref() == Some(name.as_str())
                        || self.target.as_deref() == Some(name.as_str())
                    {
                        continue;
                    }
                    let sel = self.extra_refs.iter().any(|r| r == name);
                    let fill = if sel { theme::LILAC } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::LILAC };
                    if ui
                        .add(egui::Button::new(RichText::new(name).color(col)).fill(fill))
                        .clicked()
                    {
                        acts.push(Act::ToggleExtraRef(name.clone()));
                    }
                }
            });
        });
    }

    /// The reference list for the diff ops: the target (primary) plus any extra
    /// references, skipping ones that are no longer valid repos.
    fn references(&self, target: &str) -> Vec<String> {
        let mut refs = vec![target.to_string()];
        for r in &self.extra_refs {
            if r != target && self.source.as_deref() != Some(r.as_str()) {
                refs.push(r.clone());
            }
        }
        refs
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::ORANGE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("COMMAND").color(theme::TEXT).size(12.0));
                for cmd in [Command::Copy, Command::Move, Command::Delete] {
                    let sel = self.command == cmd;
                    let accent = if cmd.destructive() {
                        theme::RED
                    } else {
                        theme::AMBER
                    };
                    let fill = if sel { accent } else { theme::PANEL };
                    // Unselected pills sit on the dark panel — black text would
                    // vanish there, so they carry their accent color instead.
                    let col = if sel { theme::BLACK } else { accent };
                    if ui
                        .add(egui::Button::new(RichText::new(cmd.label()).color(col)).fill(fill))
                        .clicked()
                    {
                        acts.push(Act::SetCommand(cmd));
                    }
                }
            });
            self.hint(ui);
        });
    }

    fn subdir_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        // The relative target subdirectory only applies to copy/move; delete
        // never writes into the target, so the group is hidden there.
        if self.command == Command::Delete {
            return;
        }
        theme::section(theme::BLUE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("INTO").color(theme::TEXT).size(12.0));
                let changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.subdir)
                            .desired_width(220.0)
                            .hint_text("relative/subdir (optional)"),
                    )
                    .changed();
                if changed {
                    acts.push(Act::SubdirChanged);
                }
                let can_browse = self.target.is_some();
                if ui
                    .add_enabled(
                        can_browse,
                        egui::Button::new(
                            RichText::new(format!("{} BROWSE", icon::FOLDER_OPEN))
                                .color(theme::BLACK),
                        ),
                    )
                    .clicked()
                {
                    acts.push(Act::BrowseSubdir);
                }
            });
            ui.label(
                RichText::new(
                    "Files keep their source-relative path under this folder inside the target.",
                )
                .color(theme::LILAC)
                .size(11.0),
            );
        });
    }

    fn hint(&self, ui: &mut egui::Ui) {
        let text = match self.command {
            Command::Copy => "Copy source files the target does not have into the target repo.",
            Command::Move => {
                "Move source files the target does not have into the target repo \
                 (they are removed from the source directory)."
            }
            Command::Delete => "Delete source files whose content the target already has.",
        };
        ui.label(RichText::new(text).color(theme::LILAC).size(11.0));
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        // Editing any condition invalidates the current preview, exactly like
        // changing the target subdir does.
        let mut changed = false;
        theme::section(theme::LILAC).show(ui, |ui| {
            let mut editing_idx = None;
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("FILTER").color(theme::TEXT).size(12.0));

                // One pill per active condition: a clickable label opening its
                // editor, followed by a small remove button.
                for (i, cond) in self.filters.iter().enumerate() {
                    if cond.editing {
                        editing_idx = Some(i);
                    }
                    let shown = cond.value.trim();
                    let text = if shown.is_empty() {
                        format!("{}: …", cond.kind.label())
                    } else {
                        format!("{}: {}", cond.kind.label(), shown)
                    };
                    let fill = if cond.editing {
                        theme::ORANGE
                    } else {
                        theme::PANEL
                    };
                    let col = if cond.editing {
                        theme::BLACK
                    } else {
                        theme::TEXT
                    };
                    if ui
                        .add(egui::Button::new(RichText::new(text).color(col)).fill(fill))
                        .clicked()
                    {
                        acts.push(Act::EditCond(i));
                    }
                    if ui
                        .add(egui::Button::new(RichText::new("×").color(theme::RED)))
                        .clicked()
                    {
                        acts.push(Act::RemoveCond(i));
                    }
                }

                // The trailing `+` pill toggles the type picker.
                if ui
                    .add(
                        egui::Button::new(RichText::new("+").color(theme::BLACK))
                            .fill(theme::AMBER),
                    )
                    .clicked()
                {
                    self.adding = !self.adding;
                }
                if self.adding {
                    for kind in [FilterKind::Mime, FilterKind::Name, FilterKind::Size] {
                        if ui
                            .add(
                                egui::Button::new(RichText::new(kind.label()).color(theme::BLUE))
                                    .fill(theme::PANEL),
                            )
                            .clicked()
                        {
                            acts.push(Act::AddCond(kind));
                        }
                    }
                }

                if !self.filters.is_empty()
                    && ui
                        .add(
                            egui::Button::new(RichText::new("CLEAR").color(theme::BLACK))
                                .fill(theme::RED),
                        )
                        .clicked()
                {
                    acts.push(Act::ClearConds);
                }
            });

            // Inline editor panel for the condition currently being edited.
            if let Some(idx) = editing_idx {
                changed |= self.cond_editor(ui, idx, acts);
            }

            self.preset_row(ui, acts);
        });
        if changed {
            acts.push(Act::FilterChanged);
        }
    }

    /// The saved-preset row: one pill per preset (click to apply, `×` to
    /// forget), a name field + SAVE for the current condition set, and
    /// EXPORT / IMPORT of the whole history as a JSON file. Hidden while the
    /// filter section is in its collapsed `FILTER +` state.
    fn preset_row(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        if !self.adding && self.filters.is_empty() && self.history.presets.is_empty() {
            return;
        }
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("PRESETS").color(theme::TEXT).size(12.0));
            for (i, preset) in self.history.presets.iter().enumerate() {
                if ui
                    .add(
                        egui::Button::new(RichText::new(&preset.name).color(theme::TAN))
                            .fill(theme::PANEL),
                    )
                    .clicked()
                {
                    acts.push(Act::ApplyPreset(i));
                }
                if ui
                    .add(egui::Button::new(RichText::new("×").color(theme::RED)))
                    .clicked()
                {
                    acts.push(Act::RemovePreset(i));
                }
            }
            // Saving needs at least one non-blank condition and a name.
            let has_conds = self.filters.iter().any(|c| !c.value.trim().is_empty());
            if has_conds {
                ui.add(
                    egui::TextEdit::singleline(&mut self.preset_name)
                        .desired_width(120.0)
                        .hint_text("preset name"),
                );
                let can_save = !self.preset_name.trim().is_empty();
                if ui
                    .add_enabled(
                        can_save,
                        egui::Button::new(RichText::new("SAVE").color(theme::BLACK))
                            .fill(theme::AMBER),
                    )
                    .clicked()
                {
                    acts.push(Act::SavePreset);
                }
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("EXPORT").color(theme::BLUE))
                        .fill(theme::PANEL),
                )
                .clicked()
            {
                acts.push(Act::ExportHistory);
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("IMPORT").color(theme::BLUE))
                        .fill(theme::PANEL),
                )
                .clicked()
            {
                acts.push(Act::ImportHistory);
            }
        });
    }

    /// The inline editor for the condition at `idx`: a text field plus
    /// data-driven assistance (cached MIME suggestions from the source repo,
    /// and remembered-value quick-picks). Returns whether the value changed.
    fn cond_editor(&mut self, ui: &mut egui::Ui, idx: usize, acts: &mut Vec<Act>) -> bool {
        let mut changed = false;
        let Some(kind) = self.filters.get(idx).map(|c| c.kind) else {
            return false;
        };
        let current = self
            .filters
            .get(idx)
            .map(|c| c.value.clone())
            .unwrap_or_default();

        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!("{}:", kind.label()))
                    .color(theme::LILAC)
                    .size(11.0),
            );
            if let Some(cond) = self.filters.get_mut(idx) {
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut cond.value)
                            .desired_width(200.0)
                            .hint_text(kind.hint()),
                    )
                    .changed();
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("DONE").color(theme::BLACK)).fill(theme::AMBER),
                )
                .clicked()
            {
                acts.push(Act::CommitCond);
            }

            // Live, debounced match count for NAME conditions (source-gated).
            // Shows nothing when the last count failed (unparsable filter or
            // unreadable index) rather than a forever-stuck "counting…".
            if kind == FilterKind::Name && self.source.is_some() {
                let counting = self.count_deadline.is_some() || self.count_in_flight;
                let text = match self.count_result {
                    Some(n) => Some(format!("{n} files match")),
                    None if counting => Some("counting…".to_string()),
                    None => None,
                };
                if let Some(text) = text {
                    ui.label(RichText::new(text).color(theme::AMBER).size(11.0));
                }
            }
        });

        // MIME suggestions: the source repo's actual MIME types (with counts),
        // cached per source repo and filtered by the typed substring. Empty
        // (and hidden) when no source is selected.
        if kind == FilterKind::Mime && !self.mime_stats.is_empty() {
            let stats = self.mime_stats.clone();
            let query = current.trim().to_lowercase();
            ui.horizontal_wrapped(|ui| {
                let mut shown = 0;
                for (mime, count) in &stats {
                    if !query.is_empty() && !mime.to_lowercase().contains(&query) {
                        continue;
                    }
                    if shown >= HISTORY_LIMIT {
                        break;
                    }
                    shown += 1;
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(format!("{mime} ({count})")).color(theme::BLUE),
                            )
                            .fill(theme::PANEL),
                        )
                        .clicked()
                        && let Some(cond) = self.filters.get_mut(idx)
                    {
                        cond.value = mime.clone();
                        changed = true;
                    }
                }
            });
        }

        // Remembered-value quick-picks for this kind.
        let recent = self.history.for_kind(kind).clone();
        if !recent.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("recent:").color(theme::LILAC).size(11.0));
                for value in &recent {
                    if ui
                        .add(
                            egui::Button::new(RichText::new(value).color(theme::TAN))
                                .fill(theme::PANEL),
                        )
                        .clicked()
                        && let Some(cond) = self.filters.get_mut(idx)
                    {
                        cond.value = value.clone();
                        changed = true;
                    }
                }
            });
        }
        changed
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::AMBER).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("ACTION").color(theme::TEXT).size(12.0));
                let ready = self.source.is_some() && self.target.is_some() && !self.running;
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(RichText::new("PREVIEW").color(theme::BLACK)),
                    )
                    .clicked()
                {
                    acts.push(Act::Preview);
                }
                let run =
                    egui::Button::new(RichText::new("RUN").color(theme::BLACK)).fill(theme::AMBER);
                if ui.add_enabled(ready, run).clicked() {
                    acts.push(Act::Ask);
                }
                if self.running {
                    ui.add(egui::Spinner::new().color(theme::AMBER));
                    if ui
                        .add(
                            egui::Button::new(RichText::new("CANCEL").color(theme::BLACK))
                                .fill(theme::RED),
                        )
                        .clicked()
                    {
                        acts.push(Act::CancelRun);
                    }
                }
                // After sanitizing a disk, mark the source repo triage-done.
                if self.source.is_some() && !self.running {
                    ui.separator();
                    if ui
                        .add(
                            egui::Button::new(RichText::new("MARK SOURCE DONE").color(theme::BLUE))
                                .fill(theme::PANEL),
                        )
                        .on_hover_text("Flag the source repo as triaged (its uniques copied out)")
                        .clicked()
                    {
                        acts.push(Act::MarkSourceDone);
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
                "Pick a source, a target and a command, then press PREVIEW.",
            );
            return;
        }
        ui.label(
            RichText::new(format!(
                "{} file(s) match · showing first {}",
                self.preview_total,
                self.preview.len()
            ))
            .color(theme::AMBER)
            .strong(),
        );
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("preview_grid")
                    .num_columns(3)
                    .striped(true)
                    .spacing(egui::vec2(12.0, 4.0))
                    .show(ui, |ui| {
                        for row in &self.preview {
                            ui.label(RichText::new(&row.from).color(theme::TEXT).size(12.0));
                            ui.label(RichText::new(icon::ARROW_RIGHT).color(theme::ORANGE));
                            ui.label(RichText::new(&row.to).color(theme::BLUE).size(12.0));
                            ui.end_row();
                        }
                    });
            });
    }

    /// The live run panel: a spinner, the file currently being handled, a
    /// scrolling list of the last N actions and a running summary line. Styled
    /// like the repo scan progress in `app.rs`.
    fn run_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.running {
                ui.add(egui::Spinner::new().color(theme::AMBER));
            }
            let current = if self.run_current.is_empty() {
                "preparing…".to_string()
            } else {
                self.run_current.clone()
            };
            ui.label(RichText::new(current).color(theme::AMBER).strong());
        });

        let summary = if self.run_total > 0 {
            format!("{} / {}", self.run_done, self.run_total)
        } else {
            self.run_done.to_string()
        };
        ui.label(
            RichText::new(format!("Processed {summary}"))
                .color(theme::TAN)
                .size(12.0),
        );

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(180.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.run_log {
                    ui.label(RichText::new(line).color(theme::TEXT).size(12.0));
                }
            });
    }

    fn confirm_modal(&mut self, ui: &mut egui::Ui, prompt: &str, acts: &mut Vec<Act>) {
        egui::Modal::new(Id::new("files-confirm")).show(&ui.ctx().clone(), |ui| {
            ui.set_width(380.0);
            ui.label(
                RichText::new(format!("CONFIRM {}", self.command.label()))
                    .color(theme::AMBER)
                    .size(16.0)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.colored_label(theme::TEXT, prompt);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let fill = if self.command.destructive() {
                    theme::RED
                } else {
                    theme::AMBER
                };
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

    fn apply(&mut self, store: &Arc<Store>, ctx: &egui::Context, act: Act) {
        match act {
            Act::PickSource(name) => {
                if self.target.as_deref() == Some(name.as_str()) {
                    self.target = None;
                }
                self.extra_refs.retain(|r| r != &name);
                self.source = Some(name);
                self.clear_preview();
                self.schedule_count();
                self.refresh_mime_stats(store);
            }
            Act::PickTarget(name) => {
                self.extra_refs.retain(|r| r != &name);
                self.target = Some(name);
                self.clear_preview();
            }
            Act::ToggleExtraRef(name) => {
                if let Some(pos) = self.extra_refs.iter().position(|r| r == &name) {
                    self.extra_refs.remove(pos);
                } else {
                    self.extra_refs.push(name);
                }
                self.clear_preview();
            }
            Act::MarkSourceDone => {
                if let Some(source) = self.source.clone() {
                    match store.set_triage_done(&source, true) {
                        Ok(()) => {
                            self.status = Some(format!("Marked '{source}' triage-done."));
                            self.error = None;
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
            }
            Act::SetCommand(cmd) => {
                self.command = cmd;
                self.clear_preview();
            }
            Act::SubdirChanged => self.clear_preview(),
            Act::FilterChanged => {
                self.clear_preview();
                self.schedule_count();
            }
            Act::AddCond(kind) => {
                for c in &mut self.filters {
                    c.editing = false;
                }
                self.filters.push(FilterCond {
                    kind,
                    value: String::new(),
                    editing: true,
                });
                self.adding = false;
                self.clear_preview();
                self.schedule_count();
            }
            Act::RemoveCond(i) => {
                if i < self.filters.len() {
                    self.filters.remove(i);
                }
                self.clear_preview();
                self.schedule_count();
            }
            Act::EditCond(i) => {
                let was_editing = self.filters.get(i).map(|c| c.editing).unwrap_or(false);
                for c in &mut self.filters {
                    c.editing = false;
                }
                if let Some(c) = self.filters.get_mut(i) {
                    c.editing = !was_editing;
                }
                self.schedule_count();
            }
            Act::CommitCond => {
                // Remember every non-blank condition value for quick-picks.
                let entries: Vec<(FilterKind, String)> = self
                    .filters
                    .iter()
                    .map(|c| (c.kind, c.value.clone()))
                    .collect();
                let mut dirty = false;
                for (kind, value) in entries {
                    dirty |= self.history.record_value(kind, &value);
                }
                if dirty {
                    self.save_history(store);
                }
                for c in &mut self.filters {
                    c.editing = false;
                }
            }
            Act::ClearConds => {
                self.filters.clear();
                self.adding = false;
                self.clear_preview();
                self.schedule_count();
            }
            Act::SavePreset => self.save_preset(store),
            Act::ApplyPreset(i) => {
                if let Some(preset) = self.history.presets.get(i) {
                    self.filters = preset
                        .conds
                        .iter()
                        .filter_map(|c| {
                            FilterKind::from_prefix(&c.kind).map(|kind| FilterCond {
                                kind,
                                value: c.value.clone(),
                                editing: false,
                            })
                        })
                        .collect();
                    self.adding = false;
                    self.clear_preview();
                    self.schedule_count();
                }
            }
            Act::RemovePreset(i) => {
                if i < self.history.presets.len() {
                    self.history.presets.remove(i);
                    self.save_history(store);
                }
            }
            Act::ExportHistory => self.export_history(ctx),
            Act::ImportHistory => self.import_history(ctx),
            Act::BrowseSubdir => self.browse_subdir(store, ctx),
            Act::Reload => self.reload(store),
            Act::Preview => self.run_preview(store),
            Act::Ask => {
                if let Some(prompt) = self.build_prompt(store) {
                    self.confirm = Some(prompt);
                }
            }
            Act::CancelConfirm => self.confirm = None,
            Act::Confirm => {
                self.confirm = None;
                self.start(store);
            }
            Act::CancelRun => self.cancel.cancel(),
        }
    }

    fn clear_preview(&mut self) {
        self.preview.clear();
        self.preview_total = 0;
    }

    /// Arm the debounced NAME match count. Only counts when a source repo is
    /// selected and at least one NAME condition exists; otherwise it clears any
    /// pending count and result.
    fn schedule_count(&mut self) {
        let has_name = self.filters.iter().any(|c| c.kind == FilterKind::Name);
        if self.source.is_some() && has_name {
            self.count_deadline = Some(Instant::now() + COUNT_DEBOUNCE);
            self.count_result = None;
        } else {
            self.count_deadline = None;
            self.count_result = None;
        }
    }

    /// Launch the background count once the debounce deadline has passed. The
    /// count runs off the UI thread and reports back on `count_rx` tagged with
    /// the current generation token so stale results can be discarded.
    fn maybe_launch_count(&mut self, store: &Arc<Store>, ctx: &egui::Context) {
        let Some(deadline) = self.count_deadline else {
            return;
        };
        let now = Instant::now();
        if now < deadline {
            ctx.request_repaint_after(deadline - now);
            return;
        }
        self.count_deadline = None;
        let Some(source) = self.source.clone() else {
            return;
        };
        self.count_token += 1;
        let token = self.count_token;
        self.count_in_flight = true;
        let filter_str = self.filter_string();
        let store = Arc::clone(store);
        let tx = self.count_tx.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            // A failed count (unparsable filter, unreadable index) must still
            // report back, or the UI would show "counting…" forever.
            let count = match FileFilter::parse(filter_str.as_deref()) {
                Ok(filter) => store
                    .open_repo_db(&source)
                    .ok()
                    .and_then(|db| count_matches(&db, &filter).ok()),
                Err(_) => None,
            };
            let _ = tx.send((token, count));
            repaint.request_repaint();
        });
    }

    /// Write the in-memory history back to its JSON file in the config dir.
    fn save_history(&mut self, store: &Store) {
        let path = store.config_dir().join(HISTORY_FILE);
        if let Err(e) = self.history.save(&path) {
            self.error = Some(format!("Could not save filter history: {e}"));
        }
    }

    /// Save the current non-blank conditions as a named preset (replacing a
    /// same-named one) and remember their values for quick-picks.
    fn save_preset(&mut self, store: &Store) {
        let name = self.preset_name.trim().to_string();
        let conds: Vec<SavedCond> = self
            .filters
            .iter()
            .filter(|c| !c.value.trim().is_empty())
            .map(|c| SavedCond {
                kind: c.kind.prefix().to_string(),
                value: c.value.trim().to_string(),
            })
            .collect();
        if name.is_empty() || conds.is_empty() {
            return;
        }
        for cond in &conds {
            if let Some(kind) = FilterKind::from_prefix(&cond.kind) {
                self.history.record_value(kind, &cond.value);
            }
        }
        let preset = FilterPreset {
            name: name.clone(),
            conds,
        };
        match self.history.presets.iter_mut().find(|p| p.name == name) {
            Some(existing) => *existing = preset,
            None => self.history.presets.push(preset),
        }
        self.preset_name.clear();
        self.save_history(store);
        self.status = Some(format!("Saved filter preset '{name}'."));
    }

    /// Open a native save dialog and write the whole history (recent values +
    /// presets) as JSON to the chosen file. Runs off the UI thread.
    fn export_history(&mut self, ctx: &egui::Context) {
        let json = match serde_json::to_vec_pretty(&self.history) {
            Ok(json) => json,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        let tx = self.io_tx.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Export filter history")
                .set_file_name("dedup-filters.json")
                .add_filter("JSON", &["json"])
                .save_file()
            {
                let msg = match std::fs::write(&path, &json) {
                    Ok(()) => HistoryIo::Exported(path),
                    Err(e) => HistoryIo::Failed(e.to_string()),
                };
                let _ = tx.send(msg);
                repaint.request_repaint();
            }
        });
    }

    /// Open a native file dialog and merge the picked JSON history into the
    /// current one. Runs off the UI thread; the merge happens in `drain`.
    fn import_history(&mut self, ctx: &egui::Context) {
        let tx = self.io_tx.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Import filter history")
                .add_filter("JSON", &["json"])
                .pick_file()
            {
                let msg = match std::fs::read(&path) {
                    Ok(bytes) => match serde_json::from_slice::<FilterHistory>(&bytes) {
                        Ok(history) => HistoryIo::Imported(history),
                        Err(e) => HistoryIo::Failed(format!("Not a filter history file: {e}")),
                    },
                    Err(e) => HistoryIo::Failed(e.to_string()),
                };
                let _ = tx.send(msg);
                repaint.request_repaint();
            }
        });
    }

    /// The subdir trimmed of surrounding whitespace and slashes; empty means
    /// "place files at the target root".
    fn normalized_subdir(&self) -> String {
        self.subdir.trim().trim_matches('/').to_string()
    }

    /// Open the native folder dialog rooted at the target repo and, on a pick,
    /// store the chosen folder as a path relative to the target root.
    fn browse_subdir(&mut self, store: &Arc<Store>, ctx: &egui::Context) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let target_root = match store.get_repo(&target) {
            Ok(meta) => PathBuf::from(meta.abs_path),
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        let tx = self.subdir_tx.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            if let Some(dir) = rfd::FileDialog::new()
                .set_title("Choose a subdirectory inside the target")
                .set_directory(&target_root)
                .pick_folder()
            {
                let msg = match dir.strip_prefix(&target_root) {
                    Ok(rel) => Ok(rel.to_string_lossy().replace('\\', "/")),
                    Err(_) => Err("The chosen folder is outside the target repo.".to_string()),
                };
                let _ = tx.send(msg);
                repaint.request_repaint();
            }
        });
    }

    fn filter_string(&self) -> Option<String> {
        let mut parts = Vec::new();
        for cond in &self.filters {
            let value = cond.value.trim();
            if !value.is_empty() {
                parts.push(format!("{}:{}", cond.kind.prefix(), value));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    }

    fn run_preview(&mut self, store: &Store) {
        let (Some(source), Some(target)) = (self.source.clone(), self.target.clone()) else {
            return;
        };
        // PREVIEW and RUN are mutually exclusive: previewing drops any run log.
        self.reset_run();
        // Remember the values actually used for a preview.
        let entries: Vec<(FilterKind, String)> = self
            .filters
            .iter()
            .map(|c| (c.kind, c.value.clone()))
            .collect();
        let mut dirty = false;
        for (kind, value) in entries {
            dirty |= self.history.record_value(kind, &value);
        }
        if dirty {
            self.save_history(store);
        }
        let filter = self.filter_string();
        let references = self.references(&target);
        let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
        match diff_print(store, &source, &ref_slice, filter.as_deref()) {
            Ok(items) => {
                let deleting = self.command == Command::Delete;
                let matched: Vec<&DiffItem> = items
                    .iter()
                    .filter(|item| match item {
                        // Copy/Move act on content the target lacks (New).
                        DiffItem::New { .. } => !deleting,
                        // Delete acts on content the target already knows.
                        DiffItem::Equal { .. } | DiffItem::DeletedInReference { .. } => deleting,
                    })
                    .collect();
                self.preview_total = matched.len();
                self.preview = matched
                    .into_iter()
                    .take(PREVIEW_LIMIT)
                    .map(|item| self.preview_row(&source, &target, item))
                    .collect();
                self.status = Some(format!(
                    "{} match the {}.",
                    self.preview_total,
                    self.command.label().to_lowercase()
                ));
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn preview_row(&self, source: &str, target: &str, item: &DiffItem) -> PreviewRow {
        let subdir = self.normalized_subdir();
        match item {
            DiffItem::New { rel_path } => {
                let to = if subdir.is_empty() {
                    format!("{target}/{rel_path}")
                } else {
                    format!("{target}/{subdir}/{rel_path}")
                };
                PreviewRow {
                    from: format!("{source}/{rel_path}"),
                    to,
                }
            }
            DiffItem::Equal { rel_path, .. } | DiffItem::DeletedInReference { rel_path } => {
                PreviewRow {
                    from: format!("{source}/{rel_path}"),
                    to: "✗ delete (already in target)".to_string(),
                }
            }
        }
    }

    fn build_prompt(&mut self, store: &Store) -> Option<String> {
        // Refresh the count so the confirmation reflects the current filter.
        self.run_preview(store);
        let (source, target) = (self.source.as_ref()?, self.target.as_ref()?);
        let subdir = self.normalized_subdir();
        let dest = if subdir.is_empty() {
            target.to_string()
        } else {
            format!("{target}/{subdir}")
        };
        Some(match self.command {
            Command::Copy => format!(
                "Copy {} file(s) from '{source}' into '{dest}'?",
                self.preview_total
            ),
            Command::Move => format!(
                "Move {} file(s) from '{source}' into '{dest}'? They are removed from the source directory.",
                self.preview_total
            ),
            Command::Delete => format!(
                "Delete {} file(s) from '{source}' that already exist in '{target}'? This cannot be undone.",
                self.preview_total
            ),
        })
    }

    fn start(&mut self, store: &Arc<Store>) {
        let (Some(source), Some(target)) = (self.source.clone(), self.target.clone()) else {
            return;
        };
        let references = self.references(&target);
        let filter = self.filter_string();
        let subdir = self.normalized_subdir();
        let command = self.command;
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        self.running = true;
        self.status = Some(format!("{}…", command.label().to_lowercase()));
        // RUN and PREVIEW are mutually exclusive: starting a run drops the
        // stale preview and resets the live run log/counters.
        self.clear_preview();
        self.reset_run();

        std::thread::spawn(move || {
            let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
            let progress = ChannelDiffProgress { tx: tx.clone() };
            let run = DiffRun::new(&progress, &cancel);
            let result = match command {
                Command::Copy | Command::Move => {
                    let move_files = command == Command::Move;
                    match store.get_repo(&target) {
                        Ok(meta) => {
                            let target_dir = std::path::PathBuf::from(meta.abs_path);
                            let subdir = if subdir.is_empty() {
                                None
                            } else {
                                Some(subdir.as_str())
                            };
                            match diff_copy(
                                &store,
                                &source,
                                &ref_slice,
                                CopyDest {
                                    dir: &target_dir,
                                    subdir,
                                },
                                move_files,
                                filter.as_deref(),
                                &run,
                            ) {
                                Ok(s) => OpResult::Copied {
                                    copied: s.copied,
                                    cancelled: s.cancelled,
                                    moved: move_files,
                                },
                                Err(e) => OpResult::Error(e.to_string()),
                            }
                        }
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
                Command::Delete => {
                    match diff_delete(&store, &source, &ref_slice, filter.as_deref(), &run) {
                        Ok(s) => OpResult::Deleted {
                            deleted: s.deleted,
                            cancelled: s.cancelled,
                        },
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
            };
            let _ = tx.send(Msg::Done(result));
        });
    }

    /// Clear the live run log and counters (used when a run starts or a
    /// preview replaces it).
    fn reset_run(&mut self) {
        self.run_log.clear();
        self.run_done = 0;
        self.run_total = 0;
        self.run_current.clear();
    }

    /// Fold one live progress event into the running counters, current line
    /// and last-N action log.
    fn apply_progress(&mut self, event: DiffEvent) {
        match event {
            DiffEvent::Progress {
                action,
                done,
                total,
                rel_path,
            } => {
                let verb = match action {
                    DiffAction::Copy => "Copied",
                    DiffAction::Move => "Moved",
                    DiffAction::Delete => "Deleted",
                };
                self.run_done = done;
                self.run_total = total;
                self.run_current = rel_path.clone();
                self.run_log.push_back(format!("{verb} {rel_path}"));
                while self.run_log.len() > RUN_LOG_LIMIT {
                    self.run_log.pop_front();
                }
            }
            DiffEvent::Error { path, message } => {
                self.run_log.push_back(format!("✗ {path}: {message}"));
                while self.run_log.len() > RUN_LOG_LIMIT {
                    self.run_log.pop_front();
                }
            }
        }
    }

    fn drain(&mut self, ui: &egui::Ui, store: &Store) {
        let mut got = false;
        // Apply export/import results from the native file dialog threads.
        while let Ok(msg) = self.io_rx.try_recv() {
            got = true;
            match msg {
                HistoryIo::Imported(history) => {
                    self.history.merge(history);
                    self.save_history(store);
                    self.status = Some("Imported filter history.".to_string());
                    self.error = None;
                }
                HistoryIo::Exported(path) => {
                    self.status = Some(format!("Exported filter history to {}.", path.display()));
                    self.error = None;
                }
                HistoryIo::Failed(e) => self.error = Some(e),
            }
        }
        // Apply any folder picked by the native subdir dialog thread.
        while let Ok(picked) = self.subdir_rx.try_recv() {
            got = true;
            match picked {
                Ok(rel) => {
                    self.subdir = rel;
                    self.error = None;
                    self.clear_preview();
                }
                Err(e) => self.error = Some(e),
            }
        }
        // Apply background NAME match counts, discarding stale generations.
        while let Ok((token, count)) = self.count_rx.try_recv() {
            got = true;
            if token == self.count_token {
                self.count_in_flight = false;
                self.count_result = count;
            }
        }
        while let Ok(msg) = self.rx.try_recv() {
            got = true;
            match msg {
                Msg::Progress(event) => self.apply_progress(event),
                Msg::Done(result) => {
                    self.running = false;
                    match result {
                        OpResult::Copied {
                            copied,
                            cancelled,
                            moved,
                        } => {
                            let verb = if moved { "Moved" } else { "Copied" };
                            self.status = Some(format!(
                                "{verb} {copied} file(s){}.",
                                if cancelled { " (cancelled)" } else { "" }
                            ));
                            self.error = None;
                        }
                        OpResult::Deleted { deleted, cancelled } => {
                            self.status = Some(format!(
                                "Deleted {deleted} file(s){}.",
                                if cancelled { " (cancelled)" } else { "" }
                            ));
                            self.error = None;
                        }
                        OpResult::Error(e) => self.error = Some(e),
                    }
                }
            }
        }
        if got || self.running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(kind: FilterKind, value: &str) -> FilterCond {
        FilterCond {
            kind,
            value: value.to_string(),
            editing: false,
        }
    }

    #[test]
    fn conditions_map_to_expected_expression() {
        let mut view = FilesView::new();
        assert_eq!(view.filter_string(), None);

        view.filters = vec![
            cond(FilterKind::Mime, "image/"),
            cond(FilterKind::Name, "foo"),
            cond(FilterKind::Size, ">=100"),
        ];
        assert_eq!(
            view.filter_string().as_deref(),
            Some("mime:image/ name:foo size:>=100")
        );
    }

    #[test]
    fn blank_conditions_are_skipped() {
        let mut view = FilesView::new();
        view.filters = vec![cond(FilterKind::Mime, "  "), cond(FilterKind::Name, "foo")];
        assert_eq!(view.filter_string().as_deref(), Some("name:foo"));
    }

    #[test]
    fn history_round_trips_through_json_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(HISTORY_FILE);

        let mut history = FilterHistory::default();
        history.record_value(FilterKind::Mime, "image/png");
        history.record_value(FilterKind::Mime, "image/jpeg");
        history.record_value(FilterKind::Size, ">=1000");
        history.presets.push(FilterPreset {
            name: "big images".to_string(),
            conds: vec![SavedCond {
                kind: "mime".to_string(),
                value: "image/".to_string(),
            }],
        });

        history.save(&path).map_err(std::io::Error::other)?;
        assert_eq!(FilterHistory::load(&path), history);
        // A missing or corrupt file falls back to an empty history.
        assert_eq!(
            FilterHistory::load(&dir.path().join("absent.json")),
            FilterHistory::default()
        );
        std::fs::write(&path, b"not json")?;
        assert_eq!(FilterHistory::load(&path), FilterHistory::default());
        Ok(())
    }

    #[test]
    fn merge_keeps_order_and_replaces_same_named_presets() {
        let preset = |name: &str, value: &str| FilterPreset {
            name: name.to_string(),
            conds: vec![SavedCond {
                kind: "name".to_string(),
                value: value.to_string(),
            }],
        };
        let mut local = FilterHistory::default();
        local.record_value(FilterKind::Name, "old");
        local.presets.push(preset("mine", "local"));

        let mut imported = FilterHistory::default();
        imported.record_value(FilterKind::Name, "second");
        imported.record_value(FilterKind::Name, "first");
        imported.presets.push(preset("mine", "imported"));
        imported.presets.push(preset("theirs", "extra"));

        local.merge(imported);
        assert_eq!(local.name, vec!["first", "second", "old"]);
        assert_eq!(
            local.presets,
            vec![preset("mine", "imported"), preset("theirs", "extra")]
        );
    }

    #[test]
    fn save_and_apply_preset() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = Arc::new(Store::open_at(dir.path().to_path_buf())?);
        let ctx = egui::Context::default();

        let mut view = FilesView::new();
        view.filters = vec![
            cond(FilterKind::Mime, "image/"),
            cond(FilterKind::Size, ">=100"),
        ];
        view.preset_name = "big images".to_string();
        view.apply(&store, &ctx, Act::SavePreset);

        assert_eq!(view.history.presets.len(), 1);
        assert!(view.preset_name.is_empty());
        // The preset (and the recorded values) hit the disk immediately.
        let loaded = FilterHistory::load(&store.config_dir().join(HISTORY_FILE));
        assert_eq!(loaded.presets, view.history.presets);
        assert_eq!(loaded.mime, vec!["image/"]);

        // Applying the preset replaces the active conditions.
        view.filters.clear();
        view.apply(&store, &ctx, Act::ApplyPreset(0));
        assert_eq!(
            view.filter_string().as_deref(),
            Some("mime:image/ size:>=100")
        );
        Ok(())
    }
}

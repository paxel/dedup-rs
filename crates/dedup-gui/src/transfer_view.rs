//! The Transfer tab: pick a source repo and a target repo, choose a command
//! (copy / move), narrow with a filter, preview the first `from → to` transfers,
//! then run it on a background thread with confirmation.
//!
//! Semantics reuse the core diff operations (content compared by size + hash):
//! - **Copy/Move** transfer source files whose content the target lacks into the
//!   target repo's directory (move also marks the source entries missing).

use crate::icon;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{
    CopyDest, DiffAction, DiffEvent, DiffItem, DiffProgress, DiffRun, FolderMode, diff_copy,
    diff_print, export_to_folder, plan_folder_export,
};
use dedup_core::filter::{FileFilter, count_matches};
use dedup_core::store::Store;
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
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
}

impl Command {
    fn label(self) -> &'static str {
        match self {
            Command::Copy => "COPY",
            Command::Move => "MOVE",
        }
    }
    fn destructive(self) -> bool {
        !matches!(self, Command::Copy)
    }
    /// (short, verbose) tooltip text for this command's selector button.
    fn tooltip(self) -> (&'static str, &'static str) {
        match self {
            Command::Copy => (
                "Copy files the target doesn't have",
                "Copy source files whose content the target (and any ALSO REF repos) \
                 doesn't already have into the target repo's directory. Source files are \
                 left in place.",
            ),
            Command::Move => (
                "Move files the target doesn't have",
                "Move source files whose content the target (and any ALSO REF repos) \
                 doesn't already have into the target repo's directory, marking the \
                 source entries missing.",
            ),
        }
    }
}

/// Where a COPY/MOVE lands: into another repo, or into a plain folder.
#[derive(PartialEq, Clone, Copy)]
enum Destination {
    /// Into the target repo (content compared against target + references).
    Repo,
    /// Into a user-picked folder (a deduplicated selection of the source).
    Folder,
}

/// How a folder export groups the source to pick which copies to keep.
#[derive(PartialEq, Clone, Copy)]
enum SelectMode {
    /// Exact-content duplicate groups.
    Exact,
    /// Perceptual-similarity groups (at the app's similarity threshold).
    Similar,
}

impl SelectMode {
    fn label(self) -> &'static str {
        match self {
            SelectMode::Exact => "EXACT",
            SelectMode::Similar => "SIMILAR",
        }
    }
}

struct PreviewRow {
    from: String,
    to: String,
}

/// A snapshot of the destination captured when a run starts, so the worker
/// thread owns everything it needs without borrowing the view.
enum StartDest {
    Repo {
        references: Vec<String>,
        target: String,
        subdir: String,
    },
    Folder {
        references: Vec<String>,
        dir: PathBuf,
        mode: FolderMode,
        invert: bool,
    },
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
    Error(String),
}

/// Messages flowing from the worker thread to the UI thread: live per-file
/// progress events plus the single terminal result.
enum Msg {
    Progress(DiffEvent),
    Done(OpResult),
}

/// [`DiffProgress`] adapter that forwards every diff event onto the TransferView
/// channel. Sends never block; a dropped receiver is fine.
struct ChannelDiffProgress {
    tx: Sender<Msg>,
}

impl DiffProgress for ChannelDiffProgress {
    fn on(&self, event: DiffEvent) {
        let _ = self.tx.send(Msg::Progress(event));
    }
}

pub struct TransferView {
    repos: Vec<String>,
    loaded: bool,
    source: Option<String>,
    target: Option<String>,
    /// Extra reference repos beyond the target: a file counts as "new" only
    /// when neither the target nor any of these already has its content.
    extra_refs: Vec<String>,
    command: Command,
    /// Whether COPY/MOVE goes into a repo or a picked folder.
    destination: Destination,
    /// Absolute path of the export folder (Destination::Folder).
    folder: String,
    /// Grouping basis for a folder export.
    select_mode: SelectMode,
    /// Export the redundant copies instead of the unique files.
    invert: bool,
    /// The app-wide similarity threshold, refreshed each frame from `show`;
    /// used when a folder export groups by SIMILAR.
    similar_threshold: f64,
    subdir: String,
    subdir_tx: Sender<Result<String, String>>,
    subdir_rx: Receiver<Result<String, String>>,
    /// Absolute export folder picked by the native folder dialog thread.
    folder_tx: Sender<Result<String, String>>,
    folder_rx: Receiver<Result<String, String>>,
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
    /// Tooltip wording for this frame, set at the top of [`Self::show`] from
    /// the app-wide setting (not persisted here; `app.rs` owns that).
    verbosity: TooltipVerbosity,
}

enum Act {
    PickSource(String),
    PickTarget(String),
    ToggleExtraRef(String),
    MarkSourceDone,
    SetCommand(Command),
    SetDestination(Destination),
    SetMode(SelectMode),
    ToggleInvert,
    FolderChanged,
    BrowseFolder,
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

impl TransferView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (subdir_tx, subdir_rx) = crossbeam_channel::unbounded();
        let (folder_tx, folder_rx) = crossbeam_channel::unbounded();
        let (count_tx, count_rx) = crossbeam_channel::unbounded();
        let (io_tx, io_rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
            loaded: false,
            source: None,
            target: None,
            extra_refs: Vec::new(),
            command: Command::Copy,
            destination: Destination::Repo,
            folder: String::new(),
            select_mode: SelectMode::Exact,
            invert: false,
            similar_threshold: 90.0,
            subdir: String::new(),
            subdir_tx,
            subdir_rx,
            folder_tx,
            folder_rx,
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
            verbosity: TooltipVerbosity::default(),
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        store: &Arc<Store>,
        verbosity: TooltipVerbosity,
        similar_threshold: f64,
    ) {
        self.verbosity = verbosity;
        self.similar_threshold = similar_threshold;
        self.drain(ui, store);
        if !self.loaded {
            self.reload(store);
        }
        self.maybe_launch_count(store, &ui.ctx().clone());

        let mut acts: Vec<Act> = Vec::new();
        ui.add_space(6.0);
        ui.label(
            RichText::new("TRANSFER")
                .color(theme::BLUE)
                .size(18.0)
                .strong(),
        );

        self.repo_rows(ui, &mut acts);
        self.command_bar(ui, &mut acts);
        self.dest_bar(ui, &mut acts);
        match self.destination {
            Destination::Repo => self.subdir_bar(ui, &mut acts),
            Destination::Folder => {
                self.folder_bar(ui, &mut acts);
                self.mode_bar(ui, &mut acts);
            }
        }
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
                        .explain(
                            self.verbosity,
                            "Pick as the source repo",
                            "Use this repository as the source: its files are compared \
                             against the target (and any DupePool repos) to decide what's \
                             new or already known.",
                        )
                        .clicked()
                    {
                        acts.push(Act::PickSource(name.clone()));
                    }
                }
                if ui
                    .button(RichText::new(icon::REFRESH).color(theme::BLACK))
                    .explain(
                        self.verbosity,
                        "Reload the repository list",
                        "Reload the list of registered repositories, e.g. after adding one \
                         in the Repositories tab.",
                    )
                    .clicked()
                {
                    acts.push(Act::Reload);
                }
            });
            // The target repo is only chosen when copying/moving into a repo; a
            // folder export has no target (the folder is the destination).
            if self.destination == Destination::Repo {
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
                            .explain(
                                self.verbosity,
                                "Pick as the target repo",
                                "Use this repository as the target: it's where COPY/MOVE files \
                                 land, and it always counts as a reference for deciding what's \
                                 new.",
                            )
                            .clicked()
                        {
                            acts.push(Act::PickTarget(name.clone()));
                        }
                    }
                });
            }
            // Reference repos: content any of them already holds is treated as
            // "already known" and never re-copied. In REPO mode the target is
            // always a reference and is shown as a locked chip.
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("DUPEPOOL").color(theme::TEXT).size(12.0));
                if self.destination == Destination::Repo
                    && let Some(target) = self.target.clone()
                {
                    // A locked, non-toggleable chip: the target is always a
                    // reference. Rendered filled (not disabled) so it reads
                    // as "on"; clicks are intentionally ignored.
                    ui.add(
                        egui::Button::new(
                            RichText::new(format!("{} {target}", icon::LOCK)).color(theme::BLACK),
                        )
                        .fill(theme::LILAC),
                    )
                    .explain(
                        self.verbosity,
                        "Always in the pool (it's the target)",
                        "The target repo is always in the dupe pool — COPY/MOVE never \
                         re-copies content the target already has — so it can't be \
                         toggled off.",
                    );
                }
                for name in &self.repos {
                    // Never a reference to itself; in REPO mode the target is
                    // shown locked above, so skip it here.
                    if self.source.as_deref() == Some(name.as_str()) {
                        continue;
                    }
                    if self.destination == Destination::Repo
                        && self.target.as_deref() == Some(name.as_str())
                    {
                        continue;
                    }
                    let sel = self.extra_refs.iter().any(|r| r == name);
                    let fill = if sel { theme::LILAC } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::LILAC };
                    if ui
                        .add(egui::Button::new(RichText::new(name).color(col)).fill(fill))
                        .explain(
                            self.verbosity,
                            "Add to the dupe pool",
                            "Also check for dupes vs these repos in addition to the target repo.",
                        )
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

    /// The DupePool repos to subtract from a folder export: the extra references
    /// (there is no target in folder mode), minus the source.
    fn folder_references(&self) -> Vec<String> {
        self.extra_refs
            .iter()
            .filter(|r| self.source.as_deref() != Some(r.as_str()))
            .cloned()
            .collect()
    }

    /// The [`FolderMode`] currently selected for a folder export.
    fn folder_mode(&self) -> FolderMode {
        match self.select_mode {
            SelectMode::Exact => FolderMode::Exact,
            SelectMode::Similar => FolderMode::Similar {
                threshold: self.similar_threshold,
            },
        }
    }

    /// Whether PREVIEW/RUN can act: a source is picked, the destination is
    /// resolved (a target repo, or a non-blank export folder), and nothing is
    /// already running.
    fn ready(&self) -> bool {
        if self.running || self.source.is_none() {
            return false;
        }
        match self.destination {
            Destination::Repo => self.target.is_some(),
            Destination::Folder => !self.folder.trim().is_empty(),
        }
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::ORANGE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("COMMAND").color(theme::TEXT).size(12.0));
                for cmd in [Command::Copy, Command::Move] {
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
                    let (short, verbose) = cmd.tooltip();
                    if ui
                        .add(egui::Button::new(RichText::new(cmd.label()).color(col)).fill(fill))
                        .explain(self.verbosity, short, verbose)
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
        theme::section(theme::BLUE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("INTO").color(theme::TEXT).size(12.0));
                let changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.subdir)
                            .desired_width(220.0)
                            .hint_text("relative/subdir (optional)"),
                    )
                    .explain(
                        self.verbosity,
                        "Relative subfolder inside the target",
                        "Place transferred files under this relative subfolder inside the \
                         target repo, preserving each file's source-relative path. Leave \
                         blank to place them at the target root. Paths escaping the target \
                         (absolute or containing `..`) are rejected.",
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
                    .explain(
                        self.verbosity,
                        "Pick or create a subfolder",
                        "Open a native folder picker rooted at the target repo to pick (or \
                         create) the subfolder transferred files go into.",
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

    /// Selector for where COPY/MOVE lands: into a repo or into a picked folder.
    fn dest_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::BLUE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("DEST").color(theme::TEXT).size(12.0));
                for (dest, label, short, verbose) in [
                    (
                        Destination::Repo,
                        "REPO",
                        "Transfer into the target repo",
                        "COPY/MOVE the source files the target (and DupePool) don't have \
                         into the target repository.",
                    ),
                    (
                        Destination::Folder,
                        "FOLDER",
                        "Export into a picked folder",
                        "COPY/MOVE a deduplicated selection of the source into a plain \
                         folder you pick, keeping each file's source-relative path.",
                    ),
                ] {
                    let sel = self.destination == dest;
                    let fill = if sel { theme::BLUE } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::BLUE };
                    if ui
                        .add(egui::Button::new(RichText::new(label).color(col)).fill(fill))
                        .explain(self.verbosity, short, verbose)
                        .clicked()
                    {
                        acts.push(Act::SetDestination(dest));
                    }
                }
            });
        });
    }

    /// The export-folder path input and its native folder picker (FOLDER mode).
    fn folder_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::BLUE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("FOLDER").color(theme::TEXT).size(12.0));
                let changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.folder)
                            .desired_width(320.0)
                            .hint_text("/absolute/export/folder"),
                    )
                    .explain(
                        self.verbosity,
                        "Absolute export folder",
                        "The folder the selected files are copied/moved into. Files keep \
                         their source-relative path under it.",
                    )
                    .changed();
                if changed {
                    acts.push(Act::FolderChanged);
                }
                if ui
                    .add(egui::Button::new(
                        RichText::new(format!("{} BROWSE", icon::FOLDER_OPEN)).color(theme::BLACK),
                    ))
                    .explain(
                        self.verbosity,
                        "Pick or create the export folder",
                        "Open a native folder picker to choose (or create) the folder the \
                         selected files go into.",
                    )
                    .clicked()
                {
                    acts.push(Act::BrowseFolder);
                }
            });
        });
    }

    /// Grouping mode (exact/similar) and the invert toggle for a folder export.
    fn mode_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::LILAC).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("MODE").color(theme::TEXT).size(12.0));
                for mode in [SelectMode::Exact, SelectMode::Similar] {
                    let sel = self.select_mode == mode;
                    let fill = if sel { theme::LILAC } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::LILAC };
                    let (short, verbose) = match mode {
                        SelectMode::Exact => (
                            "Group by exact content",
                            "Treat only byte-identical files (same size + hash) as copies of \
                             each other.",
                        ),
                        SelectMode::Similar => (
                            "Group by perceptual similarity",
                            "Treat perceptually similar media (at the Duplicates tab's \
                             similarity threshold) as copies — e.g. one photo per burst.",
                        ),
                    };
                    if ui
                        .add(egui::Button::new(RichText::new(mode.label()).color(col)).fill(fill))
                        .explain(self.verbosity, short, verbose)
                        .clicked()
                    {
                        acts.push(Act::SetMode(mode));
                    }
                }
                ui.separator();
                let fill = if self.invert {
                    theme::ORANGE
                } else {
                    theme::PANEL
                };
                let col = if self.invert {
                    theme::BLACK
                } else {
                    theme::ORANGE
                };
                if ui
                    .add(egui::Button::new(RichText::new("INVERT").color(col)).fill(fill))
                    .explain(
                        self.verbosity,
                        "Export the redundant copies instead",
                        "Off: export the unique files (one best copy per group plus every \
                         singleton). On: export the redundant copies instead (every \
                         non-best member of a group) — what a dedup would remove.",
                    )
                    .clicked()
                {
                    acts.push(Act::ToggleInvert);
                }
            });
            let hint = if self.invert {
                "Exports the redundant copies (every non-best member of a group)."
            } else {
                "Exports the unique files (best copy of each group plus every singleton)."
            };
            ui.label(RichText::new(hint).color(theme::LILAC).size(11.0));
        });
    }

    fn hint(&self, ui: &mut egui::Ui) {
        let text = match self.command {
            Command::Copy => "Copy source files the target does not have into the target repo.",
            Command::Move => {
                "Move source files the target does not have into the target repo \
                 (they are removed from the source directory)."
            }
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
                        .explain(
                            self.verbosity,
                            "Edit this condition",
                            "Open this filter condition's inline editor to change its value.",
                        )
                        .clicked()
                    {
                        acts.push(Act::EditCond(i));
                    }
                    if ui
                        .add(egui::Button::new(RichText::new("×").color(theme::RED)))
                        .explain(
                            self.verbosity,
                            "Remove this condition",
                            "Remove this filter condition. Remaining conditions still combine \
                             with AND.",
                        )
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
                    .explain(
                        self.verbosity,
                        "Add a filter condition",
                        "Show the MIME / NAME / SIZE condition-type picker to add another \
                         filter condition. Multiple conditions combine with AND.",
                    )
                    .clicked()
                {
                    self.adding = !self.adding;
                }
                if self.adding {
                    for kind in [FilterKind::Mime, FilterKind::Name, FilterKind::Size] {
                        let (short, verbose) = match kind {
                            FilterKind::Mime => (
                                "Filter by MIME type",
                                "Match files whose detected MIME type contains this substring \
                                 (e.g. \"image/\" matches every image type).",
                            ),
                            FilterKind::Name => (
                                "Filter by path substring",
                                "Match files whose relative path contains this substring \
                                 (case-sensitive, verbatim — internal spaces are preserved).",
                            ),
                            FilterKind::Size => (
                                "Filter by size",
                                "Match files by size with an operator and byte count, e.g. \
                                 \">=1000\" or \"<500000\".",
                            ),
                        };
                        if ui
                            .add(
                                egui::Button::new(RichText::new(kind.label()).color(theme::BLUE))
                                    .fill(theme::PANEL),
                            )
                            .explain(self.verbosity, short, verbose)
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
                        .explain(
                            self.verbosity,
                            "Remove every condition",
                            "Remove every filter condition, going back to matching all files.",
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
                    .explain(
                        self.verbosity,
                        "Apply this preset",
                        "Replace the current filter conditions with this saved preset.",
                    )
                    .clicked()
                {
                    acts.push(Act::ApplyPreset(i));
                }
                if ui
                    .add(egui::Button::new(RichText::new("×").color(theme::RED)))
                    .explain(
                        self.verbosity,
                        "Forget this preset",
                        "Permanently remove this saved preset. The active filter conditions \
                         are unaffected.",
                    )
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
                )
                .explain(
                    self.verbosity,
                    "Name for the new preset",
                    "Name under which to save the current set of filter conditions as a \
                     reusable preset.",
                );
                let can_save = !self.preset_name.trim().is_empty();
                if ui
                    .add_enabled(
                        can_save,
                        egui::Button::new(RichText::new("SAVE").color(theme::BLACK))
                            .fill(theme::AMBER),
                    )
                    .explain(
                        self.verbosity,
                        "Save as a preset",
                        "Save the current condition set as a named preset for one-click \
                         reuse later.",
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
                .explain(
                    self.verbosity,
                    "Export history + presets",
                    "Export remembered filter values and saved presets to a JSON file, to \
                     back up or share with another machine.",
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
                .explain(
                    self.verbosity,
                    "Import history + presets",
                    "Import remembered filter values and presets from a previously \
                     exported JSON file, merging with what's already saved.",
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
                    .explain(
                        self.verbosity,
                        "Condition value",
                        "The value to match for this condition — its meaning depends on the \
                         condition type (MIME/NAME substring, or a SIZE operator + byte count).",
                    )
                    .changed();
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("DONE").color(theme::BLACK)).fill(theme::AMBER),
                )
                .explain(
                    self.verbosity,
                    "Finish editing",
                    "Close this condition's inline editor. The value is already applied as \
                     you type.",
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
                        .explain(
                            self.verbosity,
                            "Use this MIME value",
                            &format!(
                                "Set the condition value to \"{mime}\" — {count} file(s) in \
                                 the source repo have this MIME type."
                            ),
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
                        .explain(
                            self.verbosity,
                            "Use this recent value",
                            &format!(
                                "Set the condition value to a previously used one: \"{value}\"."
                            ),
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
                let ready = self.ready();
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(RichText::new("PREVIEW").color(theme::BLACK)),
                    )
                    .explain(
                        self.verbosity,
                        "Preview the first transfers",
                        "Show the first matching `from → to` transfers (up to a preview \
                         limit) and a total count, without changing anything on disk. \
                         PREVIEW and RUN are mutually exclusive — starting a run clears the \
                         preview.",
                    )
                    .clicked()
                {
                    acts.push(Act::Preview);
                }
                let run =
                    egui::Button::new(RichText::new("RUN").color(theme::BLACK)).fill(theme::AMBER);
                if ui
                    .add_enabled(ready, run)
                    .explain(
                        self.verbosity,
                        "Run the command",
                        "Run the selected command (COPY/MOVE) on a background thread, \
                         after a confirmation dialog. Progress, the current file, and a \
                         running count are shown live.",
                    )
                    .clicked()
                {
                    acts.push(Act::Ask);
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
                            "Stop the running operation",
                            "Cancel the in-progress operation. Files already transferred \
                             before cancelling stay as they are — this stops further \
                             work, it doesn't roll back.",
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
                        .explain(
                            self.verbosity,
                            "Flag the source repo as triaged (its uniques copied out)",
                            "Mark the source repository triage-done: its unique content has \
                             already been copied out into a sanitized directory, so it shows \
                             a TRIAGED stat in Repository management and can be treated as \
                             fully processed.",
                        )
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
        egui::Modal::new(Id::new("transfer-confirm")).show(&ui.ctx().clone(), |ui| {
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
                    .explain(
                        self.verbosity,
                        "Confirm and run",
                        "Confirm and start the run on a background thread.",
                    )
                    .clicked()
                {
                    acts.push(Act::Confirm);
                }
                if ui
                    .button(RichText::new("CANCEL").color(theme::BLACK))
                    .explain(
                        self.verbosity,
                        "Cancel",
                        "Close this dialog without running anything.",
                    )
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
            Act::SetDestination(dest) => {
                self.destination = dest;
                self.clear_preview();
            }
            Act::SetMode(mode) => {
                self.select_mode = mode;
                self.clear_preview();
            }
            Act::ToggleInvert => {
                self.invert = !self.invert;
                self.clear_preview();
            }
            Act::FolderChanged => self.clear_preview(),
            Act::BrowseFolder => self.browse_folder(ctx),
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

    /// Open the native folder dialog and store the picked absolute path as the
    /// export folder (Destination::Folder).
    fn browse_folder(&mut self, ctx: &egui::Context) {
        let tx = self.folder_tx.clone();
        let repaint = ctx.clone();
        // Start the dialog in the current folder if it is a real directory.
        let start = Some(self.folder.clone()).filter(|f| Path::new(f).is_dir());
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title("Choose the export folder");
            if let Some(dir) = start {
                dialog = dialog.set_directory(dir);
            }
            if let Some(dir) = dialog.pick_folder() {
                let _ = tx.send(Ok(dir.to_string_lossy().into_owned()));
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
        let Some(source) = self.source.clone() else {
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
        match self.destination {
            Destination::Repo => self.run_preview_repo(store, &source),
            Destination::Folder => self.run_preview_folder(store, &source),
        }
    }

    fn run_preview_repo(&mut self, store: &Store, source: &str) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let filter = self.filter_string();
        let references = self.references(&target);
        let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
        match diff_print(store, source, &ref_slice, filter.as_deref()) {
            Ok(items) => {
                // Copy/Move act on content the target lacks (New).
                let matched: Vec<&DiffItem> = items
                    .iter()
                    .filter(|item| matches!(item, DiffItem::New { .. }))
                    .collect();
                self.preview_total = matched.len();
                self.preview = matched
                    .into_iter()
                    .take(PREVIEW_LIMIT)
                    .map(|item| self.preview_row(source, &target, item))
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

    fn run_preview_folder(&mut self, store: &Store, source: &str) {
        let folder = self.folder.trim();
        if folder.is_empty() {
            return;
        }
        let filter = self.filter_string();
        let references = self.folder_references();
        let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
        match plan_folder_export(
            store,
            source,
            &ref_slice,
            self.folder_mode(),
            self.invert,
            filter.as_deref(),
        ) {
            Ok(rels) => {
                self.preview_total = rels.len();
                self.preview = rels
                    .iter()
                    .take(PREVIEW_LIMIT)
                    .map(|rel| PreviewRow {
                        from: format!("{source}/{rel}"),
                        to: format!("{folder}/{rel}"),
                    })
                    .collect();
                let what = if self.invert { "redundant" } else { "unique" };
                self.status = Some(format!(
                    "{} {what} file(s) to {}.",
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
            // Transfer only previews New items (see `run_preview`); content the
            // target already has is never shown as a transfer.
            DiffItem::Equal { rel_path, .. } | DiffItem::DeletedInReference { rel_path } => {
                PreviewRow {
                    from: format!("{source}/{rel_path}"),
                    to: String::new(),
                }
            }
        }
    }

    fn build_prompt(&mut self, store: &Store) -> Option<String> {
        // Refresh the count so the confirmation reflects the current filter.
        self.run_preview(store);
        let source = self.source.as_ref()?;
        let dest = match self.destination {
            Destination::Repo => {
                let target = self.target.as_ref()?;
                let subdir = self.normalized_subdir();
                if subdir.is_empty() {
                    target.to_string()
                } else {
                    format!("{target}/{subdir}")
                }
            }
            Destination::Folder => {
                let folder = self.folder.trim();
                if folder.is_empty() {
                    return None;
                }
                folder.to_string()
            }
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
        })
    }

    fn start(&mut self, store: &Arc<Store>) {
        let Some(source) = self.source.clone() else {
            return;
        };
        // Snapshot everything the worker needs before spawning, branching on
        // where the transfer lands.
        let dest = match self.destination {
            Destination::Repo => {
                let Some(target) = self.target.clone() else {
                    return;
                };
                StartDest::Repo {
                    references: self.references(&target),
                    target,
                    subdir: self.normalized_subdir(),
                }
            }
            Destination::Folder => {
                let folder = self.folder.trim().to_string();
                if folder.is_empty() {
                    return;
                }
                StartDest::Folder {
                    references: self.folder_references(),
                    dir: PathBuf::from(&folder),
                    mode: self.folder_mode(),
                    invert: self.invert,
                }
            }
        };
        let filter = self.filter_string();
        let command = self.command;
        let move_files = command == Command::Move;
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
            let progress = ChannelDiffProgress { tx: tx.clone() };
            let run = DiffRun::new(&progress, &cancel);
            let stats = match &dest {
                StartDest::Repo {
                    references,
                    target,
                    subdir,
                } => {
                    let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
                    match store.get_repo(target) {
                        Ok(meta) => {
                            let target_dir = PathBuf::from(meta.abs_path);
                            let subdir = if subdir.is_empty() {
                                None
                            } else {
                                Some(subdir.as_str())
                            };
                            diff_copy(
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
                            )
                            .map_err(|e| e.to_string())
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
                StartDest::Folder {
                    references,
                    dir,
                    mode,
                    invert,
                } => {
                    let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
                    export_to_folder(
                        &store,
                        &source,
                        &ref_slice,
                        dir,
                        *mode,
                        *invert,
                        move_files,
                        filter.as_deref(),
                        &run,
                    )
                    .map_err(|e| e.to_string())
                }
            };
            let result = match stats {
                Ok(s) => OpResult::Copied {
                    copied: s.copied,
                    cancelled: s.cancelled,
                    moved: move_files,
                },
                Err(e) => OpResult::Error(e),
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
        // Apply any subfolder picked by the native subdir dialog thread.
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
        // Apply any export folder picked by the native folder dialog thread.
        while let Ok(picked) = self.folder_rx.try_recv() {
            got = true;
            match picked {
                Ok(abs) => {
                    self.folder = abs;
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
        let mut view = TransferView::new();
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
        let mut view = TransferView::new();
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

        let mut view = TransferView::new();
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

/// Kittest UI tests, kept separate from the plain unit tests above (which
/// don't need a rendered `egui::Ui`). Mirrors the harness pattern established
/// in `dupes_view.rs`'s `ui_tests` module.
#[cfg(test)]
mod ui_tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    /// A temp store with a `source` and `target` repo, `source` holding a
    /// couple of files so the filter builder and preview have something real
    /// to show.
    fn sample_store() -> (tempfile::TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        let src_dir = tmp.path().join("source");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("holiday.jpg"), b"fake jpeg bytes").unwrap();
        std::fs::write(src_dir.join("notes.txt"), b"fake text bytes").unwrap();
        store
            .create_repo("source", &src_dir.to_string_lossy())
            .unwrap();
        let dst_dir = tmp.path().join("target");
        std::fs::create_dir_all(&dst_dir).unwrap();
        store
            .create_repo("target", &dst_dir.to_string_lossy())
            .unwrap();
        (tmp, Arc::new(store))
    }

    /// Build a headless harness showing the Transfer view over `store`, driven
    /// by the given `setup` (which runs once, before the first frame, to select
    /// repos / destination / etc.).
    fn transfer_harness(
        store: Arc<Store>,
        setup: impl FnOnce(&mut TransferView),
    ) -> Harness<'static, TransferView> {
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        setup(&mut view);

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), 90.0);
                },
                view,
            );
        harness.run();
        harness
    }

    /// In FOLDER mode the folder/mode/invert controls appear and the repo-only
    /// controls (TARGET row and INTO subdir bar) are hidden.
    #[test]
    fn folder_mode_shows_folder_controls_and_hides_repo_controls() {
        let (_tmp, store) = sample_store();
        let harness = transfer_harness(store, |view| {
            view.destination = Destination::Folder;
        });

        // Folder-export controls are present (FOLDER appears twice: the DEST
        // toggle button and the folder-path row label).
        assert!(
            harness.query_all_by_label("FOLDER").next().is_some(),
            "FOLDER destination/label should be shown"
        );
        assert!(
            harness.query_by_label("MODE").is_some(),
            "MODE selector should be shown in folder mode"
        );
        assert!(
            harness.query_by_label("INVERT").is_some(),
            "INVERT toggle should be shown in folder mode"
        );
        // Repo-only controls are hidden.
        assert!(
            harness.query_by_label("TARGET").is_none(),
            "the TARGET row must be hidden in folder mode"
        );
        assert!(
            harness.query_by_label("INTO").is_none(),
            "the INTO subdir bar must be hidden in folder mode"
        );
    }

    /// In REPO mode the TARGET row and INTO subdir bar are shown, and the
    /// folder-export controls are absent.
    #[test]
    fn repo_mode_shows_repo_controls_and_hides_folder_controls() {
        let (_tmp, store) = sample_store();
        let harness = transfer_harness(store, |view| {
            view.destination = Destination::Repo;
            view.target = Some("target".to_string());
        });

        assert!(
            harness.query_by_label("TARGET").is_some(),
            "the TARGET row should be shown in repo mode"
        );
        assert!(
            harness.query_by_label("INTO").is_some(),
            "the INTO subdir bar should be shown in repo mode"
        );
        assert!(
            harness.query_by_label("MODE").is_none(),
            "the MODE selector must be hidden in repo mode"
        );
    }

    /// Doc screenshot: the Transfer tab with a source/target picked
    /// and a MIME filter condition active, to
    /// `docs/screenshots/files_tab.png`. Run with `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_files_tab() {
        let (_tmp, store) = sample_store();
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.target = Some("target".to_string());
        view.filters = vec![FilterCond {
            kind: FilterKind::Mime,
            value: "image/".to_string(),
            editing: false,
        }];

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), 90.0);
                },
                view,
            );
        harness.run();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("files_tab.png");
        let img = harness.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}

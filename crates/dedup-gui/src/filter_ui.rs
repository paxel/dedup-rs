//! The shared FILTER wizard used across the app (Transfer, Grooming, …) so
//! every tab builds filters the same way.
//!
//! [`FilterBuilder`] is a self-contained widget: it owns the active condition
//! pills, the MIME/NAME/SIZE inline editors, saved presets (persisted as JSON in
//! the store's config dir), MIME-type suggestions, and a debounced live match
//! count. A host embeds one, calls [`FilterBuilder::ui`] each frame, and reads
//! back the composed expression with [`FilterBuilder::filter_string`].

use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::filter::{FileFilter, count_matches};
use dedup_core::store::Store;
use egui::RichText;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long after the last edit the NAME match count is (re)computed.
const COUNT_DEBOUNCE: Duration = Duration::from_millis(350);

/// How many recent values per kind are remembered and offered as quick-picks.
const HISTORY_LIMIT: usize = 12;

/// File name of the persisted filter history inside the store's config dir.
pub const HISTORY_FILE: &str = "filter_history.json";

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
            FilterKind::Name => "*.db or copy_of*",
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

/// Deferred UI action, collected during a frame and applied after the render
/// closures release their borrow of the builder.
enum Act {
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
    FilterChanged,
}

/// What a frame of [`FilterBuilder::ui`] produced for the host to act on.
#[derive(Default)]
pub struct FilterOutcome {
    /// The composed filter changed this frame (host should drop a stale preview).
    pub changed: bool,
    /// A transient status message to surface (e.g. "Saved preset 'x'").
    pub status: Option<String>,
    /// A transient error message to surface.
    pub error: Option<String>,
}

/// The embeddable FILTER wizard. One per host tab.
pub struct FilterBuilder {
    filters: Vec<FilterCond>,
    /// Whether the `+` type picker is currently expanded.
    adding: bool,
    history: FilterHistory,
    history_loaded: bool,
    /// Name typed for the preset about to be saved.
    preset_name: String,
    /// The repo whose MIME stats / match count the wizard reflects, updated when
    /// the host passes a different repo to [`Self::ui`].
    repo: Option<String>,
    /// The `repo`'s MIME stats, cached for the MIME editor's suggestions.
    mime_stats: Vec<(String, u64)>,
    // Background export/import file-dialog results.
    io_tx: Sender<HistoryIo>,
    io_rx: Receiver<HistoryIo>,
    // Debounced background NAME match count against the repo index. A `None`
    // count means the count failed (unparsable filter or unreadable index).
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
    /// Transient status/error messages surfaced via [`FilterOutcome`].
    status: Option<String>,
    error: Option<String>,
    /// Tooltip wording for this frame, set at the top of [`Self::ui`].
    verbosity: TooltipVerbosity,
}

impl FilterBuilder {
    pub fn new() -> Self {
        let (io_tx, io_rx) = crossbeam_channel::unbounded();
        let (count_tx, count_rx) = crossbeam_channel::unbounded();
        Self {
            filters: Vec::new(),
            adding: false,
            history: FilterHistory::default(),
            history_loaded: false,
            preset_name: String::new(),
            repo: None,
            mime_stats: Vec::new(),
            io_tx,
            io_rx,
            count_tx,
            count_rx,
            count_token: 0,
            count_deadline: None,
            count_in_flight: false,
            count_result: None,
            status: None,
            error: None,
            verbosity: TooltipVerbosity::default(),
        }
    }

    /// The composed filter expression, or `None` when there are no non-blank
    /// conditions (match-all).
    pub fn filter_string(&self) -> Option<String> {
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

    /// Render the FILTER section. `count_repo` is the repo whose index backs the
    /// MIME suggestions and the live NAME match count (or `None` to disable
    /// both). Returns what the host should react to this frame.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        store: &Arc<Store>,
        count_repo: Option<&str>,
        verbosity: TooltipVerbosity,
    ) -> FilterOutcome {
        self.verbosity = verbosity;
        if !self.history_loaded {
            self.history = FilterHistory::load(&store.config_dir().join(HISTORY_FILE));
            self.history_loaded = true;
        }
        // Refresh MIME suggestions (and re-gate the count) when the repo changes.
        if self.repo.as_deref() != count_repo {
            self.repo = count_repo.map(str::to_string);
            self.mime_stats = match count_repo {
                Some(r) => store.get_mime_stats(r).unwrap_or_default(),
                None => Vec::new(),
            };
            self.schedule_count();
        }

        self.drain(store);

        let mut acts: Vec<Act> = Vec::new();
        self.filter_bar(ui, &mut acts);

        let mut changed = false;
        for act in acts {
            changed |= self.apply(store, act);
        }

        self.maybe_launch_count(store, &ui.ctx().clone());

        FilterOutcome {
            changed,
            status: self.status.take(),
            error: self.error.take(),
        }
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
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
                                "Filter by path (with wildcards)",
                                "Match files by relative path: a plain substring, or a `*` \
                                 wildcard glob like \"*.db\" (ends with) or \"copy_of*\" \
                                 (starts with).",
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
                self.cond_editor(ui, idx, acts);
            }

            self.preset_row(ui, acts);
        });
    }

    /// The saved-preset row: one pill per preset (click to apply, `×` to
    /// forget), a name field + SAVE for the current condition set, and
    /// EXPORT / IMPORT of the whole history as a JSON file.
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
    /// data-driven assistance (cached MIME suggestions from the repo, and
    /// remembered-value quick-picks). Pushes [`Act::FilterChanged`] when the
    /// value changes.
    fn cond_editor(&mut self, ui: &mut egui::Ui, idx: usize, acts: &mut Vec<Act>) {
        let Some(kind) = self.filters.get(idx).map(|c| c.kind) else {
            return;
        };
        let current = self
            .filters
            .get(idx)
            .map(|c| c.value.clone())
            .unwrap_or_default();
        let mut changed = false;

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
                         condition type (MIME/NAME substring or `*` glob, or a SIZE operator \
                         + byte count).",
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

            // Live, debounced match count for NAME conditions (repo-gated).
            // Shows nothing when the last count failed (unparsable filter or
            // unreadable index) rather than a forever-stuck "counting…".
            if kind == FilterKind::Name && self.repo.is_some() {
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

        // MIME suggestions: the repo's actual MIME types (with counts), cached
        // per repo and filtered by the typed substring. Empty (and hidden) when
        // no repo is selected.
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
                                 the repo have this MIME type."
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

        if changed {
            acts.push(Act::FilterChanged);
        }
    }

    /// Apply one deferred action. Returns whether the composed filter changed.
    fn apply(&mut self, store: &Arc<Store>, act: Act) -> bool {
        match act {
            Act::FilterChanged => {
                self.schedule_count();
                true
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
                self.schedule_count();
                true
            }
            Act::RemoveCond(i) => {
                if i < self.filters.len() {
                    self.filters.remove(i);
                }
                self.schedule_count();
                true
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
                false
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
                false
            }
            Act::ClearConds => {
                self.filters.clear();
                self.adding = false;
                self.schedule_count();
                true
            }
            Act::SavePreset => {
                self.save_preset(store);
                false
            }
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
                    self.schedule_count();
                    return true;
                }
                false
            }
            Act::RemovePreset(i) => {
                if i < self.history.presets.len() {
                    self.history.presets.remove(i);
                    self.save_history(store);
                }
                false
            }
            Act::ExportHistory => {
                self.export_history();
                false
            }
            Act::ImportHistory => {
                self.import_history();
                false
            }
        }
    }

    /// Arm the debounced NAME match count. Only counts when a repo is selected
    /// and at least one NAME condition exists; otherwise clears any pending
    /// count and result.
    fn schedule_count(&mut self) {
        let has_name = self.filters.iter().any(|c| c.kind == FilterKind::Name);
        if self.repo.is_some() && has_name {
            self.count_deadline = Some(Instant::now() + COUNT_DEBOUNCE);
            self.count_result = None;
        } else {
            self.count_deadline = None;
            self.count_result = None;
        }
    }

    /// Launch the background count once the debounce deadline has passed. The
    /// count runs off the UI thread and reports back tagged with the current
    /// generation token so stale results can be discarded.
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
        let Some(repo) = self.repo.clone() else {
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
                    .open_repo_db(&repo)
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

    /// Open a native save dialog and write the whole history as JSON to the
    /// chosen file. Runs off the UI thread.
    fn export_history(&mut self) {
        let json = match serde_json::to_vec_pretty(&self.history) {
            Ok(json) => json,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        let tx = self.io_tx.clone();
        std::thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Export filter history")
                .set_file_name("dedup-filters.json")
                .add_filter("JSON", &["json"])
                .save_file()
            {
                let msg = match std::fs::write(&path, json) {
                    Ok(()) => HistoryIo::Exported(path),
                    Err(e) => HistoryIo::Failed(e.to_string()),
                };
                let _ = tx.send(msg);
            }
        });
    }

    /// Open a native open dialog and merge the chosen JSON history file. Runs
    /// off the UI thread.
    fn import_history(&mut self) {
        let tx = self.io_tx.clone();
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
            }
        });
    }

    /// Drain background export/import and count results into state.
    fn drain(&mut self, store: &Arc<Store>) {
        while let Ok(msg) = self.io_rx.try_recv() {
            match msg {
                HistoryIo::Imported(history) => {
                    self.history.merge(history);
                    self.save_history(store);
                    self.status = Some("Imported filter history.".to_string());
                }
                HistoryIo::Exported(path) => {
                    self.status = Some(format!("Exported filter history to {}.", path.display()));
                }
                HistoryIo::Failed(e) => self.error = Some(e),
            }
        }
        while let Ok((token, count)) = self.count_rx.try_recv() {
            if token == self.count_token {
                self.count_in_flight = false;
                self.count_result = count;
            }
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
        let mut fb = FilterBuilder::new();
        assert_eq!(fb.filter_string(), None);

        fb.filters = vec![
            cond(FilterKind::Mime, "image/"),
            cond(FilterKind::Name, "foo"),
            cond(FilterKind::Size, ">=100"),
        ];
        assert_eq!(
            fb.filter_string().as_deref(),
            Some("mime:image/ name:foo size:>=100")
        );
    }

    #[test]
    fn blank_conditions_are_skipped() {
        let mut fb = FilterBuilder::new();
        fb.filters = vec![cond(FilterKind::Mime, "  "), cond(FilterKind::Name, "foo")];
        assert_eq!(fb.filter_string().as_deref(), Some("name:foo"));
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

        let mut fb = FilterBuilder::new();
        fb.filters = vec![
            cond(FilterKind::Mime, "image/"),
            cond(FilterKind::Size, ">=100"),
        ];
        fb.preset_name = "big images".to_string();
        fb.apply(&store, Act::SavePreset);

        assert_eq!(fb.history.presets.len(), 1);
        assert!(fb.preset_name.is_empty());
        // The preset (and the recorded values) hit the disk immediately.
        let loaded = FilterHistory::load(&store.config_dir().join(HISTORY_FILE));
        assert_eq!(loaded.presets, fb.history.presets);
        assert_eq!(loaded.mime, vec!["image/"]);

        // Applying the preset replaces the active conditions.
        fb.filters.clear();
        fb.apply(&store, Act::ApplyPreset(0));
        assert_eq!(
            fb.filter_string().as_deref(),
            Some("mime:image/ size:>=100")
        );
        Ok(())
    }
}

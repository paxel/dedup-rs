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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FilterKind {
    Mime,
    Name,
    Size,
    Tag,
}

/// Every condition kind, in the order they appear in the type picker.
const FILTER_KINDS: [FilterKind; 4] = [
    FilterKind::Mime,
    FilterKind::Name,
    FilterKind::Size,
    FilterKind::Tag,
];

impl FilterKind {
    fn label(self) -> &'static str {
        match self {
            FilterKind::Mime => "MIME",
            FilterKind::Name => "NAME",
            FilterKind::Size => "SIZE",
            FilterKind::Tag => "TAG",
        }
    }

    /// The lowercase prefix used in the generated filter expression. Also used
    /// as the stable serialized tag for a saved condition.
    fn prefix(self) -> &'static str {
        match self {
            FilterKind::Mime => "mime",
            FilterKind::Name => "name",
            FilterKind::Size => "size",
            FilterKind::Tag => "tag",
        }
    }

    /// Parse a kind back from its serialized [`prefix`](Self::prefix) tag.
    fn from_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "mime" => Some(FilterKind::Mime),
            "name" => Some(FilterKind::Name),
            "size" => Some(FilterKind::Size),
            "tag" => Some(FilterKind::Tag),
            _ => None,
        }
    }

    fn hint(self) -> &'static str {
        match self {
            FilterKind::Mime => "image/",
            FilterKind::Name => "*.db or copy_of*",
            FilterKind::Size => ">=1000",
            FilterKind::Tag => "keeper",
        }
    }
}

/// A single active filter condition in the builder. Exactly one condition may
/// be `editing` at a time, which reveals its inline editor panel.
struct FilterCond {
    kind: FilterKind,
    value: String,
    editing: bool,
    /// Whether the condition is inverted (`!name:*.mp3` — "everything that is
    /// *not* an MP3").
    negated: bool,
}

/// A single saved condition inside a named preset. `kind` is the stable
/// prefix tag (`mime` / `name` / `size`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SavedCond {
    kind: String,
    value: String,
    /// Absent in presets saved before negation existed, hence the default.
    #[serde(default)]
    negated: bool,
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
    tag: Vec<String>,
    presets: Vec<FilterPreset>,
}

impl FilterHistory {
    fn for_kind(&self, kind: FilterKind) -> &Vec<String> {
        match kind {
            FilterKind::Mime => &self.mime,
            FilterKind::Name => &self.name,
            FilterKind::Size => &self.size,
            FilterKind::Tag => &self.tag,
        }
    }

    fn for_kind_mut(&mut self, kind: FilterKind) -> &mut Vec<String> {
        match kind {
            FilterKind::Mime => &mut self.mime,
            FilterKind::Name => &mut self.name,
            FilterKind::Size => &mut self.size,
            FilterKind::Tag => &mut self.tag,
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
}

/// Split a filter expression into wizard conditions, mirroring the way
/// `filter_string` composes it: each `mime:`/`name:`/`size:`/`tag:` prefix at a
/// whitespace boundary starts a new condition whose value runs to the next
/// prefix. Text before the first prefix is ignored (the wizard only builds
/// these kinds). Returns the conditions plus whether the expression carried the
/// `case:insensitive` modifier.
///
/// `case:` is recognised as a group opener even though it is not a condition, so
/// that it terminates the preceding condition's value instead of being swallowed
/// into it. A group may open with `!`, which negates it.
fn parse_conditions(expr: &str) -> (Vec<FilterCond>, bool) {
    let prefixes: Vec<(String, FilterKind)> = FILTER_KINDS
        .iter()
        .map(|&k| (format!("{}:", k.prefix()), k))
        .collect();
    let bytes = expr.as_bytes();
    // `None` marks the `case:` modifier, which opens a group but yields no
    // condition.
    let mut starts: Vec<(usize, Option<FilterKind>, bool)> = Vec::new();
    for i in 0..expr.len() {
        if !expr.is_char_boundary(i) {
            continue;
        }
        if !(i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            continue;
        }
        let (rest, negated) = match expr[i..].strip_prefix('!') {
            Some(r) => (r, true),
            None => (&expr[i..], false),
        };
        if rest.starts_with("case:") {
            starts.push((i, None, negated));
            continue;
        }
        for (p, kind) in &prefixes {
            if rest.starts_with(p.as_str()) {
                starts.push((i, Some(*kind), negated));
            }
        }
    }
    let mut conds = Vec::new();
    let mut case_insensitive = false;
    for (idx, &(start, kind, negated)) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).map(|(s, ..)| *s).unwrap_or(expr.len());
        // Skip the optional '!' and the "<prefix>:" opening this group.
        let after_bang = start + usize::from(negated);
        let Some(kind) = kind else {
            case_insensitive |= expr[after_bang + "case:".len()..end].trim() == "insensitive";
            continue;
        };
        let value = expr[after_bang + kind.prefix().len() + 1..end]
            .trim()
            .to_string();
        if !value.is_empty() {
            conds.push(FilterCond {
                kind,
                value,
                editing: false,
                negated,
            });
        }
    }
    (conds, case_insensitive)
}

/// Deferred UI action, collected during a frame and applied after the render
/// closures release their borrow of the builder.
enum Act {
    AddCond(FilterKind),
    RemoveCond(usize),
    EditCond(usize),
    CommitCond,
    ClearConds,
    StorePreset,
    ApplyPreset(usize),
    RemovePreset(usize),
    CommitRenamePreset(usize),
    /// Invert a single condition (`!name:*.mp3`).
    ToggleNegate(usize),
    /// Switch text matching between case-sensitive and case-insensitive.
    ToggleCase,
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
    /// Index of the preset currently being renamed inline, if any.
    renaming_preset: Option<usize>,
    /// Text buffer for the in-progress preset rename.
    rename_buf: String,
    /// Set for one frame when a rename just started, so its text field can
    /// grab keyboard focus.
    focus_rename_pending: bool,
    /// The repo whose MIME stats / match count the wizard reflects, updated when
    /// the host passes a different repo to [`Self::ui`].
    repo: Option<String>,
    /// The `repo`'s MIME stats, cached for the MIME editor's suggestions.
    mime_stats: Vec<(String, u64)>,
    /// The `repo`'s existing annotation tags (sorted, deduped), cached for the
    /// TAG editor's suggestions so a tag can be picked instead of retyped.
    tags: Vec<String>,
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
    /// Whether text conditions match without regard to case, emitted as the
    /// `case:insensitive` modifier.
    case_insensitive: bool,
}

impl FilterBuilder {
    pub fn new() -> Self {
        let (count_tx, count_rx) = crossbeam_channel::unbounded();
        Self {
            filters: Vec::new(),
            adding: false,
            history: FilterHistory::default(),
            history_loaded: false,
            renaming_preset: None,
            rename_buf: String::new(),
            focus_rename_pending: false,
            repo: None,
            mime_stats: Vec::new(),
            tags: Vec::new(),
            count_tx,
            count_rx,
            count_token: 0,
            count_deadline: None,
            count_in_flight: false,
            count_result: None,
            status: None,
            error: None,
            verbosity: TooltipVerbosity::default(),
            case_insensitive: false,
        }
    }

    /// Replace the active conditions with those parsed from a filter expression
    /// (the `mime:`/`name:`/`size:` groups produced by [`Self::filter_string`]).
    /// Used to restore a saved rule/preset into the wizard.
    pub fn set_expression(&mut self, expr: &str) {
        let (filters, case_insensitive) = parse_conditions(expr);
        self.filters = filters;
        self.case_insensitive = case_insensitive;
        self.adding = false;
    }

    /// The composed filter expression, or `None` when there are no non-blank
    /// conditions (match-all).
    pub fn filter_string(&self) -> Option<String> {
        let mut parts = Vec::new();
        for cond in &self.filters {
            let value = cond.value.trim();
            if !value.is_empty() {
                let bang = if cond.negated { "!" } else { "" };
                parts.push(format!("{bang}{}:{value}", cond.kind.prefix()));
            }
        }
        if parts.is_empty() {
            // The case modifier alone narrows nothing, so an expression with no
            // conditions is still match-all.
            return None;
        }
        if self.case_insensitive {
            parts.insert(0, "case:insensitive".to_string());
        }
        Some(parts.join(" "))
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
                Some(r) => crate::util::or_log_default(
                    store.get_mime_stats(r),
                    "mime stats for the filter wizard",
                ),
                None => Vec::new(),
            };
            self.reload_tags(store);
            self.schedule_count();
        }
        // The tag list grows as files are annotated (in Browse), and the repo
        // doesn't change while that happens — so reload it whenever a TAG
        // condition's editor is open, keeping the pick-list current. Cheap: the
        // annotations table only holds annotated files.
        if self
            .filters
            .iter()
            .any(|c| c.kind == FilterKind::Tag && c.editing)
        {
            self.reload_tags(store);
        }

        self.drain();

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

    /// Reload the current repo's distinct annotation tags (sorted) for the TAG
    /// editor's pick-list. Empty when no repo is selected.
    fn reload_tags(&mut self, store: &Store) {
        self.tags = match self.repo.as_deref() {
            Some(r) => {
                let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
                for tags in crate::util::or_log_default(
                    store.all_annotations(r),
                    "tags for the filter wizard",
                )
                .into_values()
                {
                    set.extend(tags);
                }
                set.into_iter().collect()
            }
            None => Vec::new(),
        };
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "FILTER — NARROW WHICH FILES COUNT",
            theme::LILAC,
            |ui| {
                let mut editing_idx = None;
                ui.horizontal_wrapped(|ui| {
                    // One pill per active condition: a clickable label opening its
                    // editor, followed by a small remove button.
                    for (i, cond) in self.filters.iter().enumerate() {
                        if cond.editing {
                            editing_idx = Some(i);
                        }
                        let shown = cond.value.trim();
                        // A negated condition reads "NOT NAME: *.mp3", so the
                        // inversion is visible on the chip itself rather than
                        // only inside the editor.
                        let label = if cond.negated {
                            format!("NOT {}", cond.kind.label())
                        } else {
                            cond.kind.label().to_string()
                        };
                        let text = if shown.is_empty() {
                            format!("{label}: …")
                        } else {
                            format!("{label}: {shown}")
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
                        for kind in FILTER_KINDS {
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
                                FilterKind::Tag => (
                                    "Filter by tag",
                                    "Match files carrying a tag that contains this text. Add \
                                 tags to files in the Browse tab.",
                                ),
                            };
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(kind.label()).color(theme::BLUE),
                                    )
                                    .fill(theme::PANEL),
                                )
                                .explain(self.verbosity, short, verbose)
                                .clicked()
                            {
                                acts.push(Act::AddCond(kind));
                            }
                        }
                    }

                    // Case mode applies to every text condition at once, so it is
                    // a bar-level toggle rather than a per-condition one.
                    if !self.filters.is_empty() {
                        if crate::lcars::toggle_button(ui, "Aa", self.case_insensitive, theme::BLUE)
                            .explain(
                                self.verbosity,
                                "Ignore upper/lower case",
                                "When on, text conditions match regardless of capitalisation, so \
                             *.jpg also finds PHOTO.JPG. Size and date conditions are \
                             unaffected.",
                            )
                            .clicked()
                        {
                            acts.push(Act::ToggleCase);
                        }

                        if ui
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
                    }
                });

                // Inline editor panel for the condition currently being edited.
                if let Some(idx) = editing_idx {
                    self.cond_editor(ui, idx, acts);
                }

                self.preset_row(ui, acts);
            },
        );
    }

    /// The saved-preset row: one pill per preset (click to apply, right-click
    /// to rename, `×` to forget), and a STORE PRESET pill that saves the
    /// current condition set under an auto-generated name.
    fn preset_row(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        if !self.adding && self.filters.is_empty() && self.history.presets.is_empty() {
            return;
        }
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("PRESETS").color(theme::TEXT).size(12.0));
            // Snapshot names first so the loop body is free to mutate `self`
            // (rename state) without fighting a borrow of `self.history`.
            let presets: Vec<(usize, String)> = self
                .history
                .presets
                .iter()
                .enumerate()
                .map(|(i, p)| (i, p.name.clone()))
                .collect();
            for (i, name) in presets {
                if self.renaming_preset == Some(i) {
                    let resp = ui
                        .add(egui::TextEdit::singleline(&mut self.rename_buf).desired_width(120.0));
                    if self.focus_rename_pending {
                        resp.request_focus();
                        self.focus_rename_pending = false;
                    }
                    if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.renaming_preset = None;
                    } else if resp.lost_focus() {
                        acts.push(Act::CommitRenamePreset(i));
                    }
                    continue;
                }
                let resp = ui
                    .add(
                        egui::Button::new(RichText::new(&name).color(theme::TAN))
                            .fill(theme::PANEL),
                    )
                    .explain(
                        self.verbosity,
                        "Apply this preset",
                        "Replace the current filter conditions with this saved preset. \
                         Right-click to rename it.",
                    );
                if resp.clicked() {
                    acts.push(Act::ApplyPreset(i));
                }
                resp.context_menu(|ui| {
                    if ui.button("Rename").clicked() {
                        self.renaming_preset = Some(i);
                        self.rename_buf = name.clone();
                        self.focus_rename_pending = true;
                        ui.close();
                    }
                });
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
            // Storing needs at least one non-blank condition.
            let has_conds = self.filters.iter().any(|c| !c.value.trim().is_empty());
            if has_conds
                && ui
                    .add(
                        egui::Button::new(RichText::new("STORE PRESET").color(theme::BLACK))
                            .fill(theme::AMBER),
                    )
                    .explain(
                        self.verbosity,
                        "Store the current filter as a preset",
                        "Save the current condition set as a new preset, named \"Preset #n\" \
                         automatically. Right-click a preset afterwards to give it a better \
                         name.",
                    )
                    .clicked()
            {
                acts.push(Act::StorePreset);
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
            // NOT inverts just this condition; conditions still combine with AND,
            // so "images, except thumbnails" is MIME image + NOT NAME *thumb*.
            let negated = self.filters.get(idx).is_some_and(|c| c.negated);
            if crate::lcars::toggle_button(ui, "NOT", negated, theme::ORANGE)
                .explain(
                    self.verbosity,
                    "Invert this condition",
                    "Match everything this condition does *not* select — for example NOT \
                     NAME *.mp3 keeps every file that is not an MP3.",
                )
                .clicked()
            {
                acts.push(Act::ToggleNegate(idx));
            }
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

        // TAG suggestions: the repo's existing annotation tags, filtered by the
        // typed substring, so a tag is picked rather than retyped. Empty (and
        // hidden) when the repo has no tags yet.
        if kind == FilterKind::Tag && !self.tags.is_empty() {
            let tags = self.tags.clone();
            let query = current.trim().to_lowercase();
            ui.horizontal_wrapped(|ui| {
                for tag in &tags {
                    if !query.is_empty() && !tag.to_lowercase().contains(&query) {
                        continue;
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new(tag).color(theme::BLUE))
                                .fill(theme::PANEL),
                        )
                        .explain(
                            self.verbosity,
                            "Use this tag value",
                            &format!("Set the condition value to the existing tag \"{tag}\"."),
                        )
                        .clicked()
                        && let Some(cond) = self.filters.get_mut(idx)
                    {
                        cond.value = tag.clone();
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
                    negated: false,
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
            Act::ToggleNegate(i) => {
                let Some(cond) = self.filters.get_mut(i) else {
                    return false;
                };
                cond.negated = !cond.negated;
                self.schedule_count();
                true
            }
            Act::ToggleCase => {
                self.case_insensitive = !self.case_insensitive;
                self.schedule_count();
                true
            }
            Act::StorePreset => {
                self.store_preset(store);
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
                                negated: c.negated,
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
            Act::CommitRenamePreset(i) => {
                self.renaming_preset = None;
                let new_name = self.rename_buf.trim().to_string();
                if !new_name.is_empty()
                    && let Some(preset) = self.history.presets.get_mut(i)
                {
                    preset.name = new_name;
                    self.save_history(store);
                }
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

    /// Save the current non-blank conditions as a new preset named
    /// `Preset #n` (the next unused number) and remember their values for
    /// quick-picks.
    fn store_preset(&mut self, store: &Store) {
        let conds: Vec<SavedCond> = self
            .filters
            .iter()
            .filter(|c| !c.value.trim().is_empty())
            .map(|c| SavedCond {
                kind: c.kind.prefix().to_string(),
                value: c.value.trim().to_string(),
                negated: c.negated,
            })
            .collect();
        if conds.is_empty() {
            return;
        }
        for cond in &conds {
            if let Some(kind) = FilterKind::from_prefix(&cond.kind) {
                self.history.record_value(kind, &cond.value);
            }
        }
        let name = self.next_preset_name();
        self.history.presets.push(FilterPreset {
            name: name.clone(),
            conds,
        });
        self.save_history(store);
        self.status = Some(format!("Saved filter preset '{name}'."));
    }

    /// The next unused `Preset #n` name, based on what's already saved.
    fn next_preset_name(&self) -> String {
        let mut n = self.history.presets.len() + 1;
        loop {
            let candidate = format!("Preset #{n}");
            if !self.history.presets.iter().any(|p| p.name == candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Drain background count results into state.
    fn drain(&mut self) {
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
    use egui_kittest::kittest::Queryable;

    fn cond(kind: FilterKind, value: &str) -> FilterCond {
        FilterCond {
            kind,
            value: value.to_string(),
            editing: false,
            negated: false,
        }
    }

    fn negated_cond(kind: FilterKind, value: &str) -> FilterCond {
        FilterCond {
            negated: true,
            ..cond(kind, value)
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
    fn negated_condition_emits_a_bang() {
        let mut fb = FilterBuilder::new();
        fb.filters = vec![
            cond(FilterKind::Mime, "image"),
            negated_cond(FilterKind::Name, "*thumb*"),
        ];
        assert_eq!(
            fb.filter_string().as_deref(),
            Some("mime:image !name:*thumb*")
        );
    }

    #[test]
    fn case_toggle_emits_the_modifier() {
        let mut fb = FilterBuilder::new();
        fb.filters = vec![cond(FilterKind::Name, "*.jpg")];
        assert_eq!(fb.filter_string().as_deref(), Some("name:*.jpg"));

        fb.case_insensitive = true;
        assert_eq!(
            fb.filter_string().as_deref(),
            Some("case:insensitive name:*.jpg")
        );

        // The modifier alone narrows nothing, so it is not emitted on its own.
        fb.filters.clear();
        assert_eq!(fb.filter_string(), None);
    }

    #[test]
    fn expression_round_trips_through_the_builder() {
        // What filter_string emits, set_expression must read back identically —
        // this is the path a saved rule or preset takes.
        for expr in [
            "mime:image !name:*thumb*",
            "case:insensitive name:*.jpg",
            "case:insensitive !mime:audio size:>=100",
            "name:!important",
        ] {
            let mut fb = FilterBuilder::new();
            fb.set_expression(expr);
            assert_eq!(
                fb.filter_string().as_deref(),
                Some(expr),
                "round trip {expr}"
            );
        }
    }

    /// Drive a real `FilterBuilder` through `ui()` in a headless harness, so the
    /// NOT / `Aa` controls are actually rendered and clicked rather than having
    /// their state poked directly. Returns the builder for assertions.
    fn harness_with(
        conds: Vec<FilterCond>,
        click: &str,
    ) -> Result<FilterBuilder, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = Arc::new(Store::open_at(dir.path().to_path_buf())?);
        let mut fb = FilterBuilder::new();
        fb.filters = conds;
        let mut init = false;

        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, fb: &mut FilterBuilder| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    fb.ui(ui, &store, None, TooltipVerbosity::default());
                },
                fb,
            );

        harness.run();
        harness.get_by_label(click).click();
        harness.run();
        Ok(harness.into_state())
    }

    #[test]
    fn clicking_not_negates_that_condition() -> Result<(), Box<dyn std::error::Error>> {
        // The condition must be `editing` for its inline editor (which hosts NOT)
        // to be on screen.
        let mut editing = cond(FilterKind::Name, "*.mp3");
        editing.editing = true;
        let fb = harness_with(vec![editing], "NOT")?;

        assert!(
            fb.filters[0].negated,
            "clicking NOT inverts the condition it belongs to"
        );
        assert_eq!(
            fb.filter_string().as_deref(),
            Some("!name:*.mp3"),
            "and that reaches the composed expression"
        );
        Ok(())
    }

    #[test]
    fn clicking_aa_switches_to_case_insensitive() -> Result<(), Box<dyn std::error::Error>> {
        let fb = harness_with(vec![cond(FilterKind::Name, "*.jpg")], "Aa")?;

        assert!(
            fb.case_insensitive,
            "clicking Aa turns off case sensitivity"
        );
        assert_eq!(
            fb.filter_string().as_deref(),
            Some("case:insensitive name:*.jpg"),
            "and that reaches the composed expression"
        );
        Ok(())
    }

    #[test]
    fn the_filter_bar_stays_inside_a_narrow_window() -> Result<(), Box<dyn std::error::Error>> {
        // The Aa toggle shares the chips' wrapped row, so assert geometrically
        // that nothing escapes the window — a label query would pass even clipped.
        let dir = tempfile::tempdir()?;
        let store = Arc::new(Store::open_at(dir.path().to_path_buf())?);
        let mut fb = FilterBuilder::new();
        fb.filters = vec![
            cond(FilterKind::Mime, "image/"),
            negated_cond(FilterKind::Name, "*thumbnail*"),
            cond(FilterKind::Size, ">=100000"),
        ];
        fb.case_insensitive = true;
        let mut init = false;

        let width = 560.0;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 700.0))
            .build_ui_state(
                move |ui, fb: &mut FilterBuilder| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    fb.ui(ui, &store, None, TooltipVerbosity::default());
                },
                fb,
            );
        harness.run();

        for label in ["Aa", "CLEAR"] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                rect.max.x <= width,
                "'{label}' escapes the {width}px window: {rect:?}"
            );
            assert!(rect.min.x >= 0.0, "'{label}' starts off-screen: {rect:?}");
        }
        Ok(())
    }

    #[test]
    fn parsing_reads_negation_and_case_back() {
        let (conds, ci) = parse_conditions("case:insensitive mime:image !name:*thumb*");
        assert!(ci, "case modifier recognised");
        assert_eq!(conds.len(), 2);
        assert_eq!(conds[0].kind, FilterKind::Mime);
        assert_eq!(conds[0].value, "image");
        assert!(!conds[0].negated);
        assert_eq!(conds[1].kind, FilterKind::Name);
        assert_eq!(conds[1].value, "*thumb*");
        assert!(conds[1].negated, "negation survives the round trip");

        // The case modifier must terminate the preceding value rather than being
        // absorbed into it.
        let (conds, ci) = parse_conditions("name:foo case:insensitive");
        assert!(ci);
        assert_eq!(conds.len(), 1);
        assert_eq!(conds[0].value, "foo");

        // A '!' inside a value is ordinary text, not a negation.
        let (conds, _) = parse_conditions("name:!important");
        assert_eq!(conds.len(), 1);
        assert_eq!(conds[0].value, "!important");
        assert!(!conds[0].negated);
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
                negated: false,
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
    fn store_and_apply_preset() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = Arc::new(Store::open_at(dir.path().to_path_buf())?);

        let mut fb = FilterBuilder::new();
        fb.filters = vec![
            cond(FilterKind::Mime, "image/"),
            cond(FilterKind::Size, ">=100"),
        ];
        fb.apply(&store, Act::StorePreset);

        assert_eq!(fb.history.presets.len(), 1);
        assert_eq!(fb.history.presets[0].name, "Preset #1");
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

        // Storing a second preset auto-numbers past the first.
        fb.filters = vec![cond(FilterKind::Name, "foo")];
        fb.apply(&store, Act::StorePreset);
        assert_eq!(fb.history.presets[1].name, "Preset #2");

        // Renaming a preset persists the new name.
        fb.rename_buf = "big images".to_string();
        fb.apply(&store, Act::CommitRenamePreset(0));
        assert_eq!(fb.history.presets[0].name, "big images");
        let loaded = FilterHistory::load(&store.config_dir().join(HISTORY_FILE));
        assert_eq!(loaded.presets[0].name, "big images");
        Ok(())
    }
}

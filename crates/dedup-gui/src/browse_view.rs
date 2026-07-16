//! The Browse tab: a DB-driven, directory-based file browser for one repo — a
//! superfile/lazygit-style two-column view (subdirs | files) built entirely from
//! the indexed entries, so real files are only touched later for previews.
//!
//! The selected file drives a bottom preview dock (image → thumbnail, audio →
//! waveform, text → scrollable lines, else a hex-header + strings dump) with a
//! command column (open with the default app, reveal in the file manager).
//!
//! The file table (egui_extras) has drag-resizable columns and click-to-sort
//! headers; both panes keep their selection marked (bright when focused, dim
//! otherwise) so the preview always matches a visible row.
//!
//! The shared FILTER wizard (between the repo picker and breadcrumb) prunes the
//! whole navigation: only matching files show, and only subdirs that lead to a
//! match survive.
//!
//! Keyboard: in the subdirs pane `←` goes to the parent, `→` enters the selected
//! dir, `↑`/`↓` move the selection; `Tab` switches to the files pane. Mouse works
//! everywhere. (Annotations, multi-select and the flatten toggle land in later
//! increments.)

use crate::external;
use crate::filter_ui::FilterBuilder;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::{ExplainExt, format_mtime, format_size, shortcut_bar};
use crate::waveform::WaveCache;
use dedup_core::filter::FileFilter;
use dedup_core::store::{FileEntry, Store, for_each_file_entry};
use dedup_core::thumbnail::hash_hex;
use egui::{Color32, Key, Modifiers, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Dirs,
    Files,
}

/// The file-table column the rows are sorted by (click a header to change it).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortCol {
    Name,
    Size,
    Type,
    Modified,
}

/// Broad file categories that pick how the preview dock renders a file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cat {
    Image,
    Video,
    Audio,
    Text,
    Other,
}

impl Cat {
    fn of(mime: &str) -> Cat {
        if mime.starts_with("image/") {
            Cat::Image
        } else if mime.starts_with("video/") {
            Cat::Video
        } else if mime.starts_with("audio/") {
            Cat::Audio
        } else if mime.starts_with("text/")
            || matches!(
                mime,
                "application/json" | "application/xml" | "application/x-sh"
            )
        {
            Cat::Text
        } else {
            Cat::Other
        }
    }
}

/// The lazily-loaded body of the preview dock for text/binary files (images,
/// video and audio render straight from their background caches, so they don't
/// need a cached body here).
enum Preview {
    /// First lines of a text file (already truncated per line).
    Text(Vec<String>),
    /// A hex dump of the header plus any printable strings found in it.
    Bytes {
        hex: Vec<String>,
        strings: Vec<String>,
    },
    /// The file couldn't be read.
    Error(String),
    /// An image/video/audio file — nothing to cache here.
    Media,
}

/// One file row in the current directory (denormalized from its entry so the
/// drawing pass never re-borrows the entry list).
#[derive(Clone)]
struct FileRow {
    rel: String,
    name: String,
    size: u64,
    mime: String,
    modified_ms: i64,
    hash: [u8; 32],
}

pub struct BrowseView {
    repos: Vec<String>,
    /// Repo name → its absolute root, so a selected file's on-disk path can be
    /// resolved (`root/rel`) only when the preview dock actually needs it.
    roots: HashMap<String, String>,
    loaded: bool,
    repo: Option<String>,
    /// `(rel_path, entry)` for the selected repo, loaded once per repo, sorted.
    entries: Vec<(String, FileEntry)>,
    entries_repo: Option<String>,
    /// Current directory as path segments (empty = repo root).
    cur: Vec<String>,
    dir_sel: usize,
    file_sel: usize,
    /// The selected file's rel-path — the *identity* of the selection, so it
    /// survives re-sorting and refreshes (`file_sel` is just its index in the
    /// current display order, recomputed from this each frame).
    sel_rel: Option<String>,
    focus: Pane,
    /// One-shot: scroll the file table to `file_sel` next draw (set by keyboard
    /// moves, so the table follows arrow-key navigation without repainting
    /// forever from a per-frame scroll request).
    scroll_file: bool,
    sort_col: SortCol,
    sort_asc: bool,
    /// The shared FILTER wizard; when non-empty it prunes the whole navigation to
    /// the matching files (and the dirs that lead to them).
    filter: FilterBuilder,
    error: Option<String>,
    /// Thumbnails / audio-viz for the preview dock (background decode pools).
    thumbs: ThumbCache,
    waves: WaveCache,
    /// Cached text/binary preview body, keyed by the rel-path it was built for.
    preview: Option<Preview>,
    preview_key: Option<String>,
    verbosity: TooltipVerbosity,
}

impl BrowseView {
    pub fn new() -> Self {
        Self {
            repos: Vec::new(),
            roots: HashMap::new(),
            loaded: false,
            repo: None,
            entries: Vec::new(),
            entries_repo: None,
            cur: Vec::new(),
            dir_sel: 0,
            file_sel: 0,
            sel_rel: None,
            focus: Pane::Dirs,
            scroll_file: false,
            sort_col: SortCol::Name,
            sort_asc: true,
            filter: FilterBuilder::new(),
            error: None,
            thumbs: ThumbCache::new(2),
            waves: WaveCache::new(1),
            preview: None,
            preview_key: None,
            verbosity: TooltipVerbosity::default(),
        }
    }

    fn reload(&mut self, store: &Store) {
        match store.list_repos() {
            Ok(list) => {
                self.repos = list.iter().map(|(n, _, _)| n.clone()).collect();
                self.roots = list
                    .into_iter()
                    .map(|(n, meta, _)| (n, meta.abs_path))
                    .collect();
                if let Some(r) = &self.repo
                    && !self.repos.contains(r)
                {
                    self.repo = None;
                }
                self.loaded = true;
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Absolute path of the current repo's root, if known.
    fn repo_root(&self) -> Option<&String> {
        self.repo.as_ref().and_then(|r| self.roots.get(r))
    }

    fn load_entries(&mut self, store: &Store, repo: &str) {
        let mut entries = Vec::new();
        match store.open_repo_db(repo) {
            Ok(db) => {
                let _ = for_each_file_entry(&db, |rel, entry| {
                    if !entry.missing {
                        entries.push((rel.to_string(), entry));
                    }
                    Ok(())
                });
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        self.entries = entries;
        self.entries_repo = Some(repo.to_string());
        self.cur.clear();
        self.dir_sel = 0;
        self.file_sel = 0;
        self.focus = Pane::Dirs;
    }

    /// The immediate subdirectories and files of the current directory, derived
    /// from the indexed rel-paths (no filesystem access). `filter` prunes the
    /// navigation to matches: a file shows only if it matches, and a subdir shows
    /// only if at least one file beneath it matches (so the tree only leads to
    /// matches). A match-all filter yields the full listing.
    fn listing(&self, filter: &FileFilter) -> (Vec<String>, Vec<FileRow>) {
        let prefix = if self.cur.is_empty() {
            String::new()
        } else {
            format!("{}/", self.cur.join("/"))
        };
        let mut dirs = std::collections::BTreeSet::new();
        let mut files = Vec::new();
        for (rel, entry) in &self.entries {
            let Some(rest) = rel.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            match rest.split_once('/') {
                // A subdir is shown only if this descendant matches, so a subdir
                // survives iff at least one file beneath it matches.
                Some((seg, _)) => {
                    if filter.matches(rel, entry) {
                        dirs.insert(seg.to_string());
                    }
                }
                None => {
                    if filter.matches(rel, entry) {
                        files.push(FileRow {
                            rel: rel.clone(),
                            name: rest.to_string(),
                            size: entry.size,
                            mime: entry.mime.clone().unwrap_or_default(),
                            modified_ms: entry.modified_ms,
                            hash: entry.hash,
                        });
                    }
                }
            }
        }
        (dirs.into_iter().collect(), files)
    }

    fn enter_dir(&mut self, seg: &str) {
        self.cur.push(seg.to_string());
        self.dir_sel = 0;
        self.file_sel = 0;
    }

    fn go_parent(&mut self) {
        if self.cur.pop().is_some() {
            self.dir_sel = 0;
            self.file_sel = 0;
        }
    }

    fn handle_keys(&mut self, ui: &egui::Ui, dirs: &[String], files_len: usize) {
        // Only defer while text is actually being typed. `egui_wants_keyboard_input`
        // is too broad here: `Tab` focuses one of our selectable rows, which would
        // then block navigation forever.
        if ui.ctx().text_edit_focused() {
            return;
        }
        // Own the keyboard for TUI-style navigation: surrender any widget focus
        // egui grabbed (via Tab/arrows) so its focus system doesn't fight ours.
        if let Some(id) = ui.ctx().memory(|m| m.focused()) {
            ui.ctx().memory_mut(|m| m.surrender_focus(id));
        }
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Tab) {
                self.focus = match self.focus {
                    Pane::Dirs => Pane::Files,
                    Pane::Files => Pane::Dirs,
                };
            }
            let up = i.consume_key(Modifiers::NONE, Key::ArrowUp);
            let down = i.consume_key(Modifiers::NONE, Key::ArrowDown);
            let left = i.consume_key(Modifiers::NONE, Key::ArrowLeft);
            let right = i.consume_key(Modifiers::NONE, Key::ArrowRight);
            match self.focus {
                Pane::Dirs => {
                    if up {
                        self.dir_sel = self.dir_sel.saturating_sub(1);
                    }
                    if down && !dirs.is_empty() {
                        self.dir_sel = (self.dir_sel + 1).min(dirs.len() - 1);
                    }
                    if left {
                        self.go_parent();
                    }
                    if right && let Some(seg) = dirs.get(self.dir_sel).cloned() {
                        self.enter_dir(&seg);
                    }
                }
                Pane::Files => {
                    if up {
                        self.file_sel = self.file_sel.saturating_sub(1);
                        self.scroll_file = true;
                    }
                    if down && files_len > 0 {
                        self.file_sel = (self.file_sel + 1).min(files_len - 1);
                        self.scroll_file = true;
                    }
                    if left {
                        self.focus = Pane::Dirs;
                    }
                }
            }
        });
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, verbosity: TooltipVerbosity) {
        self.verbosity = verbosity;
        if !self.loaded {
            self.reload(store);
        }

        ui.add_space(6.0);
        ui.label(
            RichText::new("BROWSE")
                .color(theme::AMBER)
                .size(18.0)
                .strong(),
        );
        if let Some(err) = &self.error {
            ui.colored_label(theme::RED, err);
        }

        // Repo picker (single repo).
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("REPO").color(theme::TEXT).size(12.0));
            for name in self.repos.clone() {
                let sel = self.repo.as_deref() == Some(name.as_str());
                let (fill, col) = if sel {
                    (theme::AMBER, theme::BLACK)
                } else {
                    (theme::PANEL, theme::TEXT)
                };
                if ui
                    .add(egui::Button::new(RichText::new(&name).color(col)).fill(fill))
                    .clicked()
                {
                    self.repo = Some(name);
                }
            }
        });

        let Some(repo) = self.repo.clone() else {
            ui.add_space(8.0);
            ui.colored_label(theme::TEXT, "Pick a repository to browse.");
            return;
        };
        if self.entries_repo.as_deref() != Some(repo.as_str()) {
            self.load_entries(store, &repo);
        }

        // Shared FILTER wizard: it prunes the whole navigation to matches. The
        // repo backs its MIME suggestions and live match count.
        let outcome = self.filter.ui(ui, store, Some(&repo), self.verbosity);
        if outcome.error.is_some() {
            self.error = outcome.error;
        }
        if outcome.changed {
            // The active selection may no longer match; start fresh.
            self.dir_sel = 0;
            self.file_sel = 0;
            self.sel_rel = None;
        }
        let filter =
            FileFilter::parse(self.filter.filter_string().as_deref()).unwrap_or(FileFilter::All);

        // Clickable breadcrumb.
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            if ui.link(RichText::new(&repo).color(theme::LILAC)).clicked() {
                self.cur.clear();
                self.dir_sel = 0;
                self.file_sel = 0;
            }
            let segs = self.cur.clone();
            for (i, seg) in segs.iter().enumerate() {
                ui.label(RichText::new("›").color(theme::HAIRLINE));
                if ui.link(RichText::new(seg).color(theme::LILAC)).clicked() {
                    self.cur.truncate(i + 1);
                    self.dir_sel = 0;
                    self.file_sel = 0;
                }
            }
        });

        // Compute + sort the listing into display order, then relocate the
        // selection to the same file (by rel path) so re-sorting or refreshing
        // keeps it selected. Then run the keys (which may move within the list or
        // change directory).
        let (dirs, mut files) = self.listing(&filter);
        self.sort_files(&mut files);
        self.relocate_selection(&files);
        let cur_before = self.cur.clone();
        self.handle_keys(ui, &dirs, files.len());
        // A directory change from the keys invalidates the listing — recompute.
        let (dirs, files) = if self.cur == cur_before {
            (dirs, files)
        } else {
            let (d, mut f) = self.listing(&filter);
            self.sort_files(&mut f);
            (d, f)
        };
        self.dir_sel = self.dir_sel.min(dirs.len().saturating_sub(1));
        self.file_sel = self.file_sel.min(files.len().saturating_sub(1));

        // Keep the background decode pools flowing while a preview is pending.
        if self.thumbs.poll(ui.ctx()) || self.waves.poll(ui.ctx()) {
            ui.ctx().request_repaint();
        }

        ui.separator();

        // Hard, resizable panel layout (superfile/lazygit style): a fixed-width
        // subdirs pane on the left, the file table filling the centre, and a
        // resizable preview dock pinned to the bottom. Panels keep their size as
        // you navigate, so changing directory never reflows the page — only the
        // user's splitter drags resize anything.
        let sel = files.get(self.file_sel).cloned();

        egui::Panel::bottom("browse_hint")
            .show_separator_line(false)
            .exact_size(22.0)
            .show(ui, |ui| {
                shortcut_bar(
                    ui,
                    "Up/Down move · Left parent · Right enter dir · Tab switch pane",
                );
            });

        egui::Panel::bottom("browse_preview")
            .resizable(true)
            .default_size(210.0)
            .min_size(120.0)
            .max_size(480.0)
            .show(ui, |ui| {
                self.preview_dock(ui, sel.as_ref());
            });

        egui::Panel::left("browse_dirs")
            .resizable(true)
            .default_size(240.0)
            .min_size(150.0)
            .max_size(460.0)
            .show(ui, |ui| {
                self.draw_dirs(ui, &dirs);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            self.draw_files(ui, &files);
        });

        // Remember the selection by identity (rel path) after any click/key this
        // frame, so it survives the next re-sort or refresh.
        self.sel_rel = files.get(self.file_sel).map(|f| f.rel.clone());
    }

    /// Point `file_sel` at the remembered selection's new index, so it stays on
    /// the same file across re-sorting and refreshes. No-op if it's gone (e.g.
    /// after changing directory).
    fn relocate_selection(&mut self, files: &[FileRow]) {
        if let Some(rel) = &self.sel_rel
            && let Some(pos) = files.iter().position(|f| &f.rel == rel)
        {
            self.file_sel = pos;
        }
    }

    /// The left subdirs pane. Names are truncated so a long directory can't push
    /// the pane wider (its width is set by the splitter, not by content). The
    /// selected dir stays marked even when the files pane has focus (only dimmer).
    fn draw_dirs(&mut self, ui: &mut egui::Ui, dirs: &[String]) {
        list_visuals(ui, self.focus == Pane::Dirs);
        egui::ScrollArea::vertical()
            .id_salt("browse-dirs")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if !self.cur.is_empty() {
                    let up = format!("{}  ..", crate::icon::CARET_LEFT);
                    if ui.selectable_label(false, up).clicked() {
                        self.go_parent();
                    }
                }
                for (i, d) in dirs.iter().enumerate() {
                    let selected = i == self.dir_sel;
                    let label = format!("{}  {}", icon_folder(), truncate(d, 30));
                    let resp = ui.selectable_label(selected, label);
                    if resp.clicked() {
                        self.focus = Pane::Dirs;
                        self.dir_sel = i;
                        self.enter_dir(d);
                    }
                    if selected {
                        resp.scroll_to_me(None);
                    }
                }
            });
    }

    /// Sort `files` into the current display order (column + direction), always
    /// tie-breaking on the name so the order is stable.
    fn sort_files(&self, files: &mut [FileRow]) {
        files.sort_by(|a, b| {
            let by_name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
            let ord = match self.sort_col {
                SortCol::Name => by_name(),
                SortCol::Size => a.size.cmp(&b.size).then_with(by_name),
                SortCol::Type => a.mime.cmp(&b.mime).then_with(by_name),
                SortCol::Modified => a.modified_ms.cmp(&b.modified_ms).then_with(by_name),
            };
            if self.sort_asc { ord } else { ord.reverse() }
        });
    }

    /// The centre file table (egui_extras): drag-resizable columns and click-to-
    /// sort headers. The selected file stays marked whichever pane has focus, so
    /// its preview always corresponds to a visible selection.
    fn draw_files(&mut self, ui: &mut egui::Ui, files: &[FileRow]) {
        table_visuals(ui, self.focus == Pane::Files);
        let cols = [
            (SortCol::Name, "NAME"),
            (SortCol::Size, "SIZE"),
            (SortCol::Type, "TYPE"),
            (SortCol::Modified, "MODIFIED"),
        ];
        let (sort_col, sort_asc, file_sel) = (self.sort_col, self.sort_asc, self.file_sel);
        let mut clicked_header: Option<SortCol> = None;
        let mut clicked_row: Option<usize> = None;
        // Only follow the selection when a keyboard move asked for it; a per-frame
        // scroll request would repaint forever.
        let scroll_to = std::mem::take(&mut self.scroll_file).then_some(file_sel);

        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(
                Column::initial(240.0)
                    .at_least(120.0)
                    .clip(true)
                    .resizable(true),
            )
            .column(Column::initial(90.0).at_least(60.0).resizable(true))
            .column(
                Column::initial(150.0)
                    .at_least(80.0)
                    .clip(true)
                    .resizable(true),
            )
            .column(Column::remainder().at_least(120.0));
        if let Some(row) = scroll_to {
            table = table.scroll_to_row(row, Some(egui::Align::Center));
        }
        table
            .header(24.0, |mut header| {
                for (col, title) in cols {
                    header.col(|ui| {
                        if sort_header(ui, title, sort_col == col, sort_asc).clicked() {
                            clicked_header = Some(col);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, files.len(), |mut row| {
                    let i = row.index();
                    let f = &files[i];
                    row.set_selected(i == file_sel);
                    row.col(|ui| {
                        ui.add(egui::Label::new(&f.name).truncate());
                    });
                    row.col(|ui| {
                        ui.monospace(format_size(f.size));
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(&f.mime).truncate());
                    });
                    row.col(|ui| {
                        ui.monospace(format_mtime(f.modified_ms));
                    });
                    if row.response().clicked() {
                        clicked_row = Some(i);
                    }
                });
            });

        if let Some(col) = clicked_header {
            if self.sort_col == col {
                self.sort_asc = !self.sort_asc;
            } else {
                self.sort_col = col;
                self.sort_asc = true;
            }
        }
        if let Some(i) = clicked_row {
            self.focus = Pane::Files;
            self.file_sel = i;
        }
    }

    /// The bottom preview dock: a per-type preview on the left, a command column
    /// on the right. The on-disk file is only touched here (never during
    /// navigation), and text/binary bodies are cached per rel-path.
    fn preview_dock(&mut self, ui: &mut egui::Ui, sel: Option<&FileRow>) {
        let Some(sel) = sel.cloned() else {
            ui.centered_and_justified(|ui| {
                ui.colored_label(theme::HAIRLINE, "Select a file to preview it.");
            });
            return;
        };
        let cat = Cat::of(&sel.mime);
        let abs = self.repo_root().map(|r| Path::new(r).join(&sel.rel));
        self.ensure_preview(&sel.rel, abs.as_deref(), cat);

        egui::Panel::right("browse_cmds")
            .resizable(false)
            .exact_size(220.0)
            .show(ui, |ui| {
                self.draw_commands(ui, &sel, abs.as_deref());
            });
        egui::CentralPanel::default().show(ui, |ui| {
            let height = ui.available_height();
            self.draw_preview(ui, &sel, cat, abs.as_deref(), height);
        });
    }

    /// Lazily build (and cache) the text/binary preview body for `rel`. Images,
    /// video and audio stream straight from their caches, so they store `Media`.
    fn ensure_preview(&mut self, rel: &str, abs: Option<&Path>, cat: Cat) {
        if self.preview_key.as_deref() == Some(rel) {
            return;
        }
        self.preview_key = Some(rel.to_string());
        self.preview = Some(match (cat, abs) {
            (Cat::Image | Cat::Video | Cat::Audio, _) => Preview::Media,
            (_, None) => Preview::Error("repository root unknown".into()),
            (Cat::Text, Some(abs)) => match read_head(abs, 128 * 1024) {
                Ok(bytes) => build_text(&bytes),
                Err(e) => Preview::Error(e.to_string()),
            },
            (Cat::Other, Some(abs)) => match read_head(abs, 4096) {
                Ok(bytes) => build_bytes(&bytes),
                Err(e) => Preview::Error(e.to_string()),
            },
        });
    }

    fn draw_preview(
        &mut self,
        ui: &mut egui::Ui,
        sel: &FileRow,
        cat: Cat,
        abs: Option<&Path>,
        height: f32,
    ) {
        match cat {
            Cat::Image | Cat::Video => {
                let tex = abs.and_then(|abs| {
                    let hex = hash_hex(&sel.hash);
                    if cat == Cat::Video {
                        self.thumbs.get_video(&hex, abs, 2, 5)
                    } else {
                        self.thumbs.get(&hex, abs)
                    }
                });
                if let Some(tex) = tex {
                    ui.centered_and_justified(|ui| {
                        ui.add(
                            egui::Image::new(egui::load::SizedTexture::from_handle(&tex))
                                .max_height(height - 8.0)
                                .maintain_aspect_ratio(true)
                                .corner_radius(4),
                        );
                    });
                } else {
                    placeholder(ui, "decoding…");
                }
            }
            Cat::Audio => {
                let viz = abs.and_then(|abs| self.waves.get(&hash_hex(&sel.hash), abs));
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), height - 8.0),
                    egui::Sense::hover(),
                );
                let p = ui.painter_at(rect);
                p.rect_filled(rect, 4.0, theme::PANEL);
                match viz.map(|v| v.envelope.clone()).filter(|e| !e.is_empty()) {
                    Some(env) => {
                        let mid = rect.center().y;
                        let bw = rect.width() / env.len() as f32;
                        for (i, &amp) in env.iter().enumerate() {
                            let h = amp * rect.height() * 0.46;
                            let x = rect.left() + i as f32 * bw;
                            p.rect_filled(
                                egui::Rect::from_min_max(
                                    egui::pos2(x, mid - h),
                                    egui::pos2(x + bw.max(1.0), mid + h),
                                ),
                                0.0,
                                theme::BLUE,
                            );
                        }
                    }
                    None => {
                        p.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "analyzing…",
                            egui::FontId::proportional(15.0),
                            theme::TAN,
                        );
                    }
                }
            }
            Cat::Text | Cat::Other => {
                egui::ScrollArea::vertical()
                    .id_salt("browse-preview")
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.preview.as_ref() {
                        Some(Preview::Text(lines)) => {
                            for l in lines {
                                ui.label(RichText::new(l).monospace().size(12.0));
                            }
                        }
                        Some(Preview::Bytes { hex, strings }) => {
                            ui.label(RichText::new("HEADER").color(theme::AMBER).size(11.0));
                            for l in hex {
                                ui.label(
                                    RichText::new(l).monospace().size(12.0).color(theme::TEXT),
                                );
                            }
                            if !strings.is_empty() {
                                ui.add_space(4.0);
                                ui.label(RichText::new("STRINGS").color(theme::AMBER).size(11.0));
                                for s in strings {
                                    ui.label(
                                        RichText::new(s).monospace().size(12.0).color(theme::LILAC),
                                    );
                                }
                            }
                        }
                        Some(Preview::Error(e)) => {
                            ui.colored_label(theme::RED, e);
                        }
                        _ => {}
                    });
            }
        }
    }

    fn draw_commands(&mut self, ui: &mut egui::Ui, sel: &FileRow, abs: Option<&Path>) {
        ui.vertical(|ui| {
            ui.label(
                RichText::new(truncate(&sel.name, 30))
                    .color(theme::TEXT)
                    .strong(),
            );
            ui.label(
                RichText::new(format!("{} · {}", format_size(sel.size), sel.mime))
                    .color(theme::HAIRLINE)
                    .size(11.0),
            );
            ui.label(
                RichText::new(format_mtime(sel.modified_ms))
                    .color(theme::HAIRLINE)
                    .size(11.0),
            );
            ui.add_space(8.0);

            let enabled = abs.is_some();
            if amber_button(ui, enabled, "Open with default app")
                .explain(
                    self.verbosity,
                    "Open in the system's default application",
                    "Hand the file to the OS default app — the full-fidelity escape hatch \
                     for any type the in-app preview can't fully render.",
                )
                .clicked()
                && let Some(abs) = abs
                && let Err(e) = external::open(abs)
            {
                self.error = Some(e.to_string());
            }
            if amber_button(ui, enabled, "Reveal in file manager")
                .explain(
                    self.verbosity,
                    "Show the file's folder in the file manager",
                    "Open the containing folder in the system file manager (portable \
                     lowest-common-denominator: it reveals the parent directory).",
                )
                .clicked()
                && let Some(abs) = abs
                && let Err(e) = external::reveal(abs)
            {
                self.error = Some(e.to_string());
            }
        });
    }
}

/// A clickable file-table column header: the title, plus an up/down caret when
/// it's the active sort column (amber then, cream otherwise). No background, so
/// it never turns into an unreadable pill on hover.
fn sort_header(ui: &mut egui::Ui, title: &str, active: bool, asc: bool) -> egui::Response {
    let text = if active {
        let caret = if asc {
            crate::icon::CARET_UP
        } else {
            crate::icon::CARET_DOWN
        };
        format!("{title} {caret}")
    } else {
        title.to_string()
    };
    let color = if active { theme::AMBER } else { theme::TEXT };
    ui.add(egui::Label::new(RichText::new(text).color(color).strong()).sense(egui::Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Bright/dim blue selection so both panes keep their selection marked TUI-style
/// (bright when focused, dim otherwise). Shared by the list panes.
fn selection_colors(ui: &mut egui::Ui, active: bool) {
    let sel = &mut ui.visuals_mut().selection;
    sel.bg_fill = if active {
        Color32::from_rgb(0x2E, 0x45, 0x80)
    } else {
        Color32::from_rgb(0x1A, 0x22, 0x30)
    };
    sel.stroke.color = theme::TEXT;
}

/// Interaction colours for the **dirs** pane (interactive `selectable_label`s):
/// cream text when idle, black text on the amber hover pill (the theme default
/// would leave cream text on amber, unreadable).
fn list_visuals(ui: &mut egui::Ui, active: bool) {
    let w = &mut ui.visuals_mut().widgets;
    w.inactive.fg_stroke.color = theme::TEXT;
    w.hovered.fg_stroke.color = theme::BLACK;
    w.active.fg_stroke.color = theme::BLACK;
    selection_colors(ui, active);
}

/// Interaction colours for the **file table**, whose cells are non-interactive
/// `Label`s with fixed cream text. egui_extras paints the hovered row with
/// `widgets.hovered.bg_fill`; the theme's amber there would hide the cream text,
/// so use a subtle dark row highlight instead (selection stays blue).
fn table_visuals(ui: &mut egui::Ui, active: bool) {
    let w = &mut ui.visuals_mut().widgets;
    w.noninteractive.fg_stroke.color = theme::TEXT;
    w.hovered.bg_fill = Color32::from_rgb(0x2C, 0x37, 0x4E);
    selection_colors(ui, active);
}

/// A full-width command button with readable black text on the amber pill (the
/// theme's default cream-on-amber is barely legible).
fn amber_button(ui: &mut egui::Ui, enabled: bool, label: &str) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).color(theme::BLACK))
            .fill(theme::AMBER)
            .min_size(egui::vec2(ui.available_width(), 0.0)),
    )
}

/// Read up to `max` bytes from the head of a file (previews never need more).
fn read_head(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max];
    let mut filled = 0;
    while filled < max {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// First lines of a text file, lossily decoded and per-line truncated so one
/// runaway line can't blow up the layout.
fn build_text(bytes: &[u8]) -> Preview {
    let text = String::from_utf8_lossy(bytes);
    let lines = text.lines().take(800).map(|l| truncate(l, 400)).collect();
    Preview::Text(lines)
}

/// A classic 16-byte-per-row hex dump of the header plus the printable ASCII
/// runs (≥4 chars) found in it — the "unknown binary" fallback preview.
fn build_bytes(bytes: &[u8]) -> Preview {
    let head = &bytes[..bytes.len().min(256)];
    let mut hex = Vec::new();
    for (row, chunk) in head.chunks(16).enumerate() {
        let mut cols = String::new();
        for b in chunk {
            cols.push_str(&format!("{b:02x} "));
        }
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        hex.push(format!("{:04x}  {:<48} {ascii}", row * 16, cols));
    }
    let mut strings = Vec::new();
    let mut run = String::new();
    for &b in bytes {
        if (0x20..0x7f).contains(&b) {
            run.push(b as char);
        } else {
            if run.len() >= 4 {
                strings.push(run.clone());
            }
            run.clear();
        }
        if strings.len() >= 40 {
            break;
        }
    }
    if run.len() >= 4 && strings.len() < 40 {
        strings.push(run);
    }
    Preview::Bytes { hex, strings }
}

/// A centred grey note used while a media preview is still decoding.
fn placeholder(ui: &mut egui::Ui, text: &str) {
    ui.centered_and_justified(|ui| {
        ui.colored_label(theme::TAN, text);
    });
}

fn icon_folder() -> &'static str {
    crate::icon::FOLDER_OPEN
}

/// Truncate to `n` chars with an ellipsis, for the fixed-width file columns.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use std::sync::Arc;

    fn entry() -> FileEntry {
        FileEntry {
            size: 100,
            hash: [0u8; 32],
            modified_ms: 0,
            missing: false,
            mime: Some("image/png".into()),
            img_fingerprint: None,
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: None,
            origin: None,
            exif: None,
        }
    }

    fn frow(name: &str, size: u64, mime: &str, modified_ms: i64) -> FileRow {
        FileRow {
            rel: name.into(),
            name: name.into(),
            size,
            mime: mime.into(),
            modified_ms,
            hash: [0u8; 32],
        }
    }

    /// `sort_files` orders by the chosen column and direction, tie-breaking on
    /// name so the order is stable.
    #[test]
    fn file_sort_by_column_and_direction() {
        let mut v = BrowseView::new();
        let names = |files: &[FileRow]| files.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
        let mut files = vec![
            frow("b.txt", 30, "text/plain", 200),
            frow("a.txt", 10, "image/png", 300),
            frow("c.txt", 20, "audio/mpeg", 100),
        ];

        v.sort_col = SortCol::Name;
        v.sort_asc = true;
        v.sort_files(&mut files);
        assert_eq!(names(&files), ["a.txt", "b.txt", "c.txt"]);

        v.sort_col = SortCol::Size;
        v.sort_files(&mut files);
        assert_eq!(names(&files), ["a.txt", "c.txt", "b.txt"]);

        v.sort_asc = false;
        v.sort_files(&mut files);
        assert_eq!(names(&files), ["b.txt", "c.txt", "a.txt"]);

        v.sort_col = SortCol::Modified;
        v.sort_asc = true;
        v.sort_files(&mut files);
        assert_eq!(names(&files), ["c.txt", "b.txt", "a.txt"]);
    }

    /// Changing the sort keeps the *same file* selected (selection tracked by rel
    /// path, not by row index).
    #[test]
    fn sorting_keeps_the_same_file_selected() {
        let mut v = BrowseView::new();
        let mkentry = |size: u64| {
            let mut e = entry();
            e.size = size;
            e
        };
        v.entries = vec![
            ("a.txt".into(), mkentry(30)),
            ("b.txt".into(), mkentry(10)),
            ("c.txt".into(), mkentry(20)),
        ];

        // Sorted by name asc (a, b, c); select b.txt.
        let (_d, mut files) = v.listing(&FileFilter::All);
        v.sort_files(&mut files);
        v.file_sel = 1;
        v.sel_rel = Some(files[v.file_sel].rel.clone());
        assert_eq!(files[v.file_sel].rel, "b.txt");

        // Now sort by size asc (b=10, c=20, a=30): b.txt moves to index 0, and the
        // selection must follow it there.
        v.sort_col = SortCol::Size;
        let (_d, mut files) = v.listing(&FileFilter::All);
        v.sort_files(&mut files);
        v.relocate_selection(&files);
        assert_eq!(
            files[v.file_sel].rel, "b.txt",
            "selection stays on the same file across a re-sort"
        );
        assert_eq!(v.file_sel, 0, "b.txt is now the first row");
    }

    /// The FILTER prunes the whole navigation: only files that match show, and
    /// only subdirs that lead to a match survive.
    #[test]
    fn filter_prunes_navigation_to_matches() {
        let emime = |mime: &str| {
            let mut e = entry();
            e.mime = Some(mime.into());
            e
        };
        let mut v = BrowseView::new();
        v.entries = vec![
            ("2019/Trips/IMG_01.jpg".into(), emime("image/jpeg")),
            ("2019/notes.txt".into(), emime("text/plain")),
            ("2020/a.png".into(), emime("image/png")),
            ("readme.md".into(), emime("text/markdown")),
        ];
        let names = |files: &[FileRow]| files.iter().map(|f| f.name.clone()).collect::<Vec<_>>();

        // At the root, `mime:image` keeps both dated dirs (each has an image) but
        // no root files (readme.md is text).
        let f = FileFilter::parse(Some("mime:image")).unwrap();
        let (dirs, files) = v.listing(&f);
        assert_eq!(dirs, ["2019", "2020"]);
        assert!(files.is_empty());

        // `mime:text` keeps only 2019 (notes.txt) and the root readme.md.
        let f = FileFilter::parse(Some("mime:text")).unwrap();
        let (dirs, files) = v.listing(&f);
        assert_eq!(dirs, ["2019"]);
        assert_eq!(names(&files), ["readme.md"]);

        // Inside 2019, `mime:image` leads only to Trips, with no direct files.
        v.cur = vec!["2019".into()];
        let f = FileFilter::parse(Some("mime:image")).unwrap();
        let (dirs, files) = v.listing(&f);
        assert_eq!(dirs, ["Trips"]);
        assert!(files.is_empty());
    }

    /// The subdir/file panes and `←`/`→` navigation are built entirely from the
    /// indexed rel-paths — no filesystem access.
    #[test]
    fn dir_navigation_from_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let repo_dir = tmp.path().join("r");
        std::fs::create_dir_all(&repo_dir).unwrap();
        store.create_repo("r", &repo_dir.to_string_lossy()).unwrap();
        for rel in [
            "2019/Trips/IMG_01.jpg",
            "2019/Trips/IMG_02.jpg",
            "2019/notes.txt",
            "2020/a.png",
            "readme.md",
        ] {
            store.update_file_entry("r", rel, &entry()).unwrap();
        }

        let mut view = BrowseView::new();
        view.repo = Some("r".into());
        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut BrowseView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default());
                },
                view,
            );
        h.run();
        assert!(h.state().cur.is_empty(), "starts at the repo root");

        // → enters the first subdir (2019), → again into its only subdir (Trips).
        h.key_press(egui::Key::ArrowRight);
        h.run();
        h.run();
        assert_eq!(h.state().cur, vec!["2019".to_string()], "→ enters 2019");

        h.key_press(egui::Key::ArrowRight);
        h.run();
        h.run();
        assert_eq!(
            h.state().cur,
            vec!["2019".to_string(), "Trips".to_string()],
            "→ enters Trips"
        );

        // ← climbs back to the parent.
        h.key_press(egui::Key::ArrowLeft);
        h.run();
        h.run();
        assert_eq!(h.state().cur, vec!["2019".to_string()], "← goes to parent");

        // Tab switches the active pane from dirs to files.
        assert!(h.state().focus == Pane::Dirs, "starts on the dirs pane");
        h.key_press(egui::Key::Tab);
        h.run();
        h.run();
        assert!(
            h.state().focus == Pane::Files,
            "Tab switches to the files pane"
        );
    }

    /// Renders the Browse tab (two-column dir/file view) to
    /// `target/dupes_browse.png` for manual inspection. `--ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_browse() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let repo_dir = tmp.path().join("Photos");
        std::fs::create_dir_all(&repo_dir).unwrap();
        store
            .create_repo("Photos", &repo_dir.to_string_lossy())
            .unwrap();
        let mk = |rel: &str, size: u64, mime: &str, body: &[u8]| {
            let mut e = entry();
            e.size = size;
            e.mime = Some(mime.into());
            let abs = repo_dir.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, body).unwrap();
            store.update_file_entry("Photos", rel, &e).unwrap();
        };
        mk("2019/Trips/IMG_0001.jpg", 2_100_000, "image/jpeg", b"");
        mk("2019/Trips/IMG_0002.jpg", 1_800_000, "image/jpeg", b"");
        mk("2019/Camera/DSC_9.arw", 24_000_000, "image/x-sony-arw", b"");
        mk(
            "2019/notes.txt",
            3072,
            "text/plain",
            b"Trip to the coast, spring 2019.\nFilm rolls: 3 (Portra 400)\nBackup: done -> external SSD\nTODO: scan the last roll\n",
        );
        mk("2019/diary.txt", 2048, "text/plain", b"Dear diary...\n");
        mk("2020/a.png", 500_000, "image/png", b"");
        mk("readme.md", 1024, "text/markdown", b"# Photos\n");

        let mut view = BrowseView::new();
        view.repo = Some("Photos".into());
        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1100.0, 640.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut BrowseView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    // The app hosts the view in a CentralPanel (height-bounded to
                    // the viewport); bound it here too so the preview dock lands
                    // on-screen instead of below an unbounded harness ui.
                    ui.allocate_ui(egui::vec2(ui.available_width(), 600.0), |ui| {
                        view.show(ui, &store_ui, TooltipVerbosity::default());
                    });
                },
                view,
            );
        h.run();
        // Enter 2019, switch to the files pane so notes.txt drives the preview.
        h.key_press(egui::Key::ArrowRight);
        h.run();
        h.run();
        h.key_press(egui::Key::Tab);
        h.run();
        h.run();
        // Hover the non-selected row to prove the hover state is legible (dark
        // highlight + cream text, not cream-on-amber). diary.txt sorts first, so
        // notes.txt is the unselected row.
        h.get_by_label("notes.txt").hover();
        h.run();
        let img = h.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dupes_browse.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}

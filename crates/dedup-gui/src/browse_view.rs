//! The Browse tab: a DB-driven, directory-based file browser for one repo — a
//! superfile/lazygit-style two-column view (subdirs | files) built entirely from
//! the indexed entries, so real files are only touched later for previews.
//!
//! Keyboard: in the subdirs pane `←` goes to the parent, `→` enters the selected
//! dir, `↑`/`↓` move the selection; `Tab` switches to the files pane. Mouse works
//! everywhere. (Filter pruning, preview + commands, annotations, multi-select and
//! the flatten toggle land in later increments.)

use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::{format_mtime, format_size, shortcut_bar};
use dedup_core::store::{FileEntry, Store, for_each_file_entry};
use egui::{Key, Modifiers, RichText};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Dirs,
    Files,
}

/// One file row in the current directory (denormalized from its entry so the
/// drawing pass never re-borrows the entry list).
struct FileRow {
    rel: String,
    name: String,
    size: u64,
    mime: String,
    modified_ms: i64,
}

pub struct BrowseView {
    repos: Vec<String>,
    loaded: bool,
    repo: Option<String>,
    /// `(rel_path, entry)` for the selected repo, loaded once per repo, sorted.
    entries: Vec<(String, FileEntry)>,
    entries_repo: Option<String>,
    /// Current directory as path segments (empty = repo root).
    cur: Vec<String>,
    dir_sel: usize,
    file_sel: usize,
    focus: Pane,
    error: Option<String>,
    verbosity: TooltipVerbosity,
}

impl BrowseView {
    pub fn new() -> Self {
        Self {
            repos: Vec::new(),
            loaded: false,
            repo: None,
            entries: Vec::new(),
            entries_repo: None,
            cur: Vec::new(),
            dir_sel: 0,
            file_sel: 0,
            focus: Pane::Dirs,
            error: None,
            verbosity: TooltipVerbosity::default(),
        }
    }

    fn reload(&mut self, store: &Store) {
        match store.list_repos() {
            Ok(list) => {
                self.repos = list.into_iter().map(|(n, _, _)| n).collect();
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
    /// from the indexed rel-paths (no filesystem access).
    fn listing(&self) -> (Vec<String>, Vec<FileRow>) {
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
                Some((seg, _)) => {
                    dirs.insert(seg.to_string());
                }
                None => files.push(FileRow {
                    rel: rel.clone(),
                    name: rest.to_string(),
                    size: entry.size,
                    mime: entry.mime.clone().unwrap_or_default(),
                    modified_ms: entry.modified_ms,
                }),
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
        if ui.ctx().egui_wants_keyboard_input() {
            return;
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
                    }
                    if down && files_len > 0 {
                        self.file_sel = (self.file_sel + 1).min(files_len - 1);
                    }
                    if left {
                        self.focus = Pane::Dirs;
                    }
                }
            }
        });
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Store, verbosity: TooltipVerbosity) {
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

        // Keyboard nav (may change `cur`), then recompute for drawing.
        let (dirs, files) = self.listing();
        self.handle_keys(ui, &dirs, files.len());
        let (dirs, files) = self.listing();
        self.dir_sel = self.dir_sel.min(dirs.len().saturating_sub(1));
        self.file_sel = self.file_sel.min(files.len().saturating_sub(1));

        ui.separator();
        let body_h = (ui.available_height() - 28.0).max(80.0);
        ui.horizontal_top(|ui| {
            // Subdirs column.
            ui.allocate_ui(egui::vec2(240.0, body_h), |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("browse-dirs")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if !self.cur.is_empty() && ui.selectable_label(false, "↑  ..").clicked() {
                            self.go_parent();
                        }
                        for (i, d) in dirs.iter().enumerate() {
                            let selected = self.focus == Pane::Dirs && i == self.dir_sel;
                            let resp = ui.selectable_label(
                                selected,
                                RichText::new(format!("{}  {d}", icon_folder())).color(theme::TEXT),
                            );
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
            });
            ui.separator();
            // Files column.
            ui.allocate_ui(egui::vec2(ui.available_width(), body_h), |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("browse-files")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if files.is_empty() {
                            ui.add_space(4.0);
                            ui.colored_label(theme::HAIRLINE, "(no files in this folder)");
                        }
                        for (i, f) in files.iter().enumerate() {
                            let selected = self.focus == Pane::Files && i == self.file_sel;
                            let line = format!(
                                "{:<32}  {:>9}  {:<16}  {}",
                                truncate(&f.name, 32),
                                format_size(f.size),
                                truncate(&f.mime, 16),
                                format_mtime(f.modified_ms),
                            );
                            let resp = ui.selectable_label(
                                selected,
                                RichText::new(line).color(theme::TEXT).monospace(),
                            );
                            if resp.clicked() {
                                self.focus = Pane::Files;
                                self.file_sel = i;
                            }
                            if selected {
                                resp.scroll_to_me(None);
                            }
                            let _ = &f.rel; // used by the preview/commands increment
                        }
                    });
            });
        });

        shortcut_bar(ui, "↑/↓ move · ← parent · → enter dir · Tab switch pane");
    }
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
        let mk = |rel: &str, size: u64, mime: &str| {
            let mut e = entry();
            e.size = size;
            e.mime = Some(mime.into());
            store.update_file_entry("Photos", rel, &e).unwrap();
        };
        mk("2019/Trips/IMG_0001.jpg", 2_100_000, "image/jpeg");
        mk("2019/Trips/IMG_0002.jpg", 1_800_000, "image/jpeg");
        mk("2019/Camera/DSC_9.arw", 24_000_000, "image/x-sony-arw");
        mk("2019/notes.txt", 3072, "text/plain");
        mk("2020/a.png", 500_000, "image/png");
        mk("readme.md", 1024, "text/markdown");

        let mut view = BrowseView::new();
        view.repo = Some("Photos".into());
        view.cur = vec!["2019".into()];
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
                    view.show(ui, &store_ui, TooltipVerbosity::default());
                },
                view,
            );
        h.run();
        let img = h.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dupes_browse.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}

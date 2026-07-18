//! The shared repo chip: a per-repo identicon + name grouped in a bordered
//! pill, accent-filled when selected. Every tab's repo selector renders repos
//! with this one widget, so a repo looks and reads the same everywhere. The
//! Duplicates tab passes a lock state, which adds a padlock as a third grouped
//! item; every other tab shows just the identicon + name.

use crate::icon;
use crate::theme;
use egui::{Color32, Rect, RichText, Sense, Vec2};

/// Point size of the square identicon glyph.
const GLYPH: f32 = 18.0;

/// Paint a deterministic, left-right-symmetric 5×5 identicon for `name` into
/// `rect`, in a stable name-hashed pastel color on a dark tile. The same name
/// always yields the same glyph, so a repo keeps one visual identity across
/// tabs; different names almost always differ (hash-derived).
pub fn identicon(painter: &egui::Painter, rect: Rect, name: &str) {
    // Square the rect from its center so the grid stays regular.
    let side = rect.width().min(rect.height());
    let rect = Rect::from_center_size(rect.center(), Vec2::splat(side));
    // A dark tile makes the pastel cells legible on any chip fill.
    painter.rect_filled(rect, 3.0, theme::BLACK);

    let hash = theme::name_hash(name);
    let color = theme::hsl((hash % 360) as f32, 0.55, 0.70);
    let cell = side / 5.0;
    // Bits above the hue byte choose which cells are on: 3 free columns (the
    // other two mirror them) × 5 rows = 15 bits.
    let mut bits = hash >> 9;
    for col in 0..3u32 {
        for row in 0..5u32 {
            let on = bits & 1 == 1;
            bits >>= 1;
            if !on {
                continue;
            }
            for c in [col, 4 - col] {
                let min = rect.min + Vec2::new(c as f32 * cell, row as f32 * cell);
                painter.rect_filled(Rect::from_min_size(min, Vec2::splat(cell)), 0.0, color);
            }
        }
    }
}

/// The click targets of a rendered chip. Callers attach their own tooltips and
/// read `.clicked()` — so the same widget serves single-select, multi-toggle,
/// and the Duplicates include/lock pair.
pub struct RepoChipResponse {
    /// The whole chip frame — its rect measures the chip for layout/wrapping.
    pub outer: egui::Response,
    /// The identicon + name area (select / toggle the repo).
    pub name: egui::Response,
    /// The padlock sub-button, present only when `lock` was `Some`.
    pub lock: Option<egui::Response>,
}

/// Render a repo chip for `name`. When `selected`, the pill is filled with
/// `accent` and black text; otherwise it's a dark panel with an `accent`
/// outline and `accent` text. `lock: Some(read_only)` adds a padlock toggle as
/// a third grouped item (Duplicates only).
pub fn repo_chip(
    ui: &mut egui::Ui,
    name: &str,
    selected: bool,
    accent: Color32,
    lock: Option<bool>,
) -> RepoChipResponse {
    let (fill, fg) = if selected {
        (accent, theme::BLACK)
    } else {
        (theme::PANEL, accent)
    };
    let mut lock_resp = None;
    let inner = egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, accent))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // The identicon + name form one clickable, labelled unit (the
                // name is painted, not a child Label, so the whole area is a
                // single click target — and one queryable node in tests). Any
                // lock is a separate button appended after it.
                let font = egui::FontId::proportional(14.0);
                let gap = 6.0;
                let galley = ui.painter().layout_no_wrap(name.to_owned(), font, fg);
                let h = GLYPH.max(galley.size().y);
                let size = Vec2::new(GLYPH + gap + galley.size().x, h);
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                if ui.is_rect_visible(rect) {
                    let gy = rect.min.y + (h - GLYPH) / 2.0;
                    let grect = Rect::from_min_size(egui::pos2(rect.min.x, gy), Vec2::splat(GLYPH));
                    identicon(ui.painter(), grect, name);
                    let tpos = egui::pos2(
                        rect.min.x + GLYPH + gap,
                        rect.center().y - galley.size().y / 2.0,
                    );
                    ui.painter().galley(tpos, galley, fg);
                }
                resp.widget_info(|| {
                    egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, name)
                });
                if let Some(read_only) = lock {
                    lock_resp = Some(lock_button(ui, read_only));
                }
                resp
            })
            .inner
        });
    RepoChipResponse {
        outer: inner.response,
        name: inner.inner,
        lock: lock_resp,
    }
}

/// A labelled row of repo chips that **wraps** onto multiple lines when the
/// window is too narrow (egui can't wrap composite chips itself — it only wraps
/// items whose size it knows before layout). Chips are greedy-packed using each
/// chip's size measured the previous frame, cached in egui temp memory keyed by
/// `id_salt` (so callers need no per-row fields). `chip(ui, i)` renders chip `i`
/// and must return its frame response, whose rect feeds the packing.
pub fn chip_row(
    ui: &mut egui::Ui,
    id_salt: &str,
    label: &str,
    n: usize,
    mut chip: impl FnMut(&mut egui::Ui, usize) -> egui::Response,
) {
    let id = ui.id().with(id_salt);
    let mut sizes: Vec<Vec2> = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    sizes.resize(n, Vec2::ZERO);

    let avail = ui.available_width();
    let spacing = ui.spacing().item_spacing.x;
    // The label leads the first row; measure it for packing only.
    let label_w = ui
        .painter()
        .layout_no_wrap(
            label.to_owned(),
            egui::FontId::proportional(12.0),
            theme::TEXT,
        )
        .size()
        .x;

    // Greedy packing: a chip's footprint is its measured width plus the trailing
    // add_space(8); every chip also carries a leading item_spacing. Row 0 begins
    // already occupied by the label.
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut used = label_w;
    for (i, s) in sizes.iter().enumerate() {
        let w = spacing + s.x + 8.0;
        if !rows.last().unwrap().is_empty() && used + w > avail {
            rows.push(Vec::new());
            used = 0.0;
        }
        used += w;
        rows.last_mut().unwrap().push(i);
    }

    let mut changed = false;
    ui.vertical(|ui| {
        for (r, row) in rows.iter().enumerate() {
            ui.horizontal_top(|ui| {
                if r == 0 {
                    ui.label(RichText::new(label).color(theme::TEXT).size(12.0));
                }
                for &i in row {
                    let resp = chip(ui, i);
                    if (resp.rect.size() - sizes[i]).length() > 0.5 {
                        sizes[i] = resp.rect.size();
                        changed = true;
                    }
                    ui.add_space(8.0);
                }
            });
        }
    });

    ui.data_mut(|d| d.insert_temp(id, sizes));
    // A chip's size changed under us: re-pack the rows with it this frame.
    if changed {
        ui.ctx().request_repaint();
    }
}

/// The padlock toggle inside a Duplicates chip. Closed blue padlock = read-only
/// (protected); open padlock = deletable. Colored independently of the chip
/// accent so "locked" always reads the same.
fn lock_button(ui: &mut egui::Ui, read_only: bool) -> egui::Response {
    let (glyph, fill, text) = if read_only {
        (icon::LOCK, theme::BLUE, theme::BLACK)
    } else {
        (icon::LOCK_OPEN, theme::PANEL, theme::BLUE)
    };
    ui.add(egui::Button::new(RichText::new(glyph).color(text)).fill(fill))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    /// The identicon is deterministic per name and (almost always) differs
    /// between names — a stable per-repo identity.
    #[test]
    fn identicon_is_deterministic_and_distinct() {
        // Same name → identical hue + cell bits.
        assert_eq!(theme::name_hash("Photos"), theme::name_hash("Photos"));
        // Distinct names → distinct hashes (hence distinct glyphs) for a
        // handful of realistic repo names.
        let names = ["Photos", "Videos", "Archive", "Automatic Upload", "data"];
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                assert_ne!(
                    theme::name_hash(a),
                    theme::name_hash(b),
                    "identicon hash collision between {a} and {b}"
                );
            }
        }
    }

    /// The chip renders the name and reports a click on it; with `lock` set it
    /// also exposes a padlock response.
    #[test]
    fn chip_renders_name_and_lock() {
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(300.0, 80.0))
            .build_ui_state(
                move |ui, clicked: &mut bool| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let r = repo_chip(ui, "Photos", true, theme::ORANGE, Some(true));
                    if r.name.clicked() {
                        *clicked = true;
                    }
                    assert!(r.lock.is_some(), "lock response present when lock is Some");
                },
                false,
            );
        harness.run();
        assert!(
            harness.query_by_label("Photos").is_some(),
            "name label shows"
        );
        harness.get_by_label("Photos").click();
        harness.run();
        assert!(*harness.state(), "clicking the chip reports a name click");
    }

    /// Without a lock state there is no padlock (every non-Duplicates tab).
    #[test]
    fn chip_without_lock_has_no_padlock() {
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(300.0, 80.0))
            .build_ui(move |ui| {
                if !init {
                    crate::icon::install(ui.ctx());
                    crate::theme::apply(ui.ctx());
                    init = true;
                }
                let r = repo_chip(ui, "Videos", false, theme::BLUE, None);
                assert!(r.lock.is_none(), "no lock response when lock is None");
            });
        harness.run();
        assert!(harness.query_by_label("Videos").is_some());
    }
}

//! The shared repo chip: a per-repo identicon + name grouped in a bordered
//! pill, accent-filled when selected. Every tab's repo selector renders repos
//! with this one widget, so a repo looks and reads the same everywhere. A repo
//! that is the main of a sync group carries a star badge; the Duplicates tab
//! additionally passes a lock state, which adds a padlock as a further grouped
//! item.

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
    let tile = Rect::from_center_size(rect.center(), Vec2::splat(side));
    // A dark tile makes the pastel cells legible on any chip fill.
    painter.rect_filled(tile, 3.0, theme::black());

    let hash = theme::name_hash(name);
    let color = theme::hsl((hash % 360) as f32, 0.55, 0.70);
    // Equal, integer-sized cells centered in the tile with a margin, so every
    // column is the same width and the grid is exactly left-right symmetric — and
    // no cell sits flush against the tile edge (which clipped the last column thin).
    let cell = (side * 0.8 / 5.0).round().max(1.0);
    let grid = cell * 5.0;
    // Snap the grid origin to whole pixels. Otherwise the columns land on
    // fractional coordinates and anti-aliasing renders the left and right
    // (mirrored) columns with slightly different coverage — visibly asymmetric.
    let origin = (tile.center() - Vec2::splat(grid / 2.0)).round();
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
                let min = origin + Vec2::new(c as f32 * cell, row as f32 * cell);
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
/// outline and `accent` text. `main` adds the sync-group main badge;
/// `lock: Some(read_only)` adds a padlock toggle (Duplicates only).
///
/// `name` must be the repo's registry name and nothing else — the identicon is
/// hashed from it, so decorating the string (a mode, a count) silently gives the
/// same repo a different glyph here than on every other tab.
pub fn repo_chip(
    ui: &mut egui::Ui,
    name: &str,
    selected: bool,
    accent: Color32,
    main: bool,
    lock: Option<bool>,
) -> RepoChipResponse {
    let (fill, fg) = if selected {
        (accent, theme::black())
    } else {
        (theme::panel(), accent)
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
                if main {
                    main_badge(ui);
                }
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
    // The label (if any) leads the first row; measure it for packing only.
    let label_w = if label.is_empty() {
        0.0
    } else {
        ui.painter()
            .layout_no_wrap(
                label.to_owned(),
                egui::FontId::proportional(12.0),
                theme::text(),
            )
            .size()
            .x
    };

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
                if r == 0 && !label.is_empty() {
                    ui.label(RichText::new(label).color(theme::text()).size(12.0));
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

/// A compact accent-outlined button, shared by the bulk controls (MARK ALL /
/// NONE on multi-select repo rows, and the per-group MARK/HIDE actions).
pub fn small_button(ui: &mut egui::Ui, label: &str, accent: Color32) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).color(accent).size(11.0))
            .fill(theme::panel())
            .stroke(egui::Stroke::new(1.0, accent)),
    )
}

/// The "this repo is the main of a sync group" badge: a star on a filled amber
/// tile. Not interactive — it reports state, it does not change it.
///
/// Colored independently of the chip accent (like the padlock) so a main always
/// reads the same whether or not the chip is selected. The vendored Phosphor
/// subset carries a single star codepoint, so filled-vs-outline is not available
/// to separate this from the MAKE MAIN *action* button; the filled tile behind
/// the glyph is what distinguishes them.
///
/// Announced as "MAIN" rather than as the raw glyph, so tests and screen readers
/// get a word.
fn main_badge(ui: &mut egui::Ui) -> egui::Response {
    let pad = Vec2::new(5.0, 2.0);
    let galley = ui.painter().layout_no_wrap(
        icon::STAR.to_owned(),
        egui::FontId::proportional(13.0),
        theme::black(),
    );
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, 4.0, theme::amber());
        ui.painter().galley(rect.min + pad, galley, theme::black());
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "MAIN"));
    resp
}

/// The padlock toggle inside a Duplicates chip. Closed blue padlock = read-only
/// (protected); open padlock = deletable. Colored independently of the chip
/// accent so "locked" always reads the same.
fn lock_button(ui: &mut egui::Ui, read_only: bool) -> egui::Response {
    let (glyph, fill, text) = if read_only {
        (icon::LOCK, theme::blue(), theme::black())
    } else {
        (icon::LOCK_OPEN, theme::panel(), theme::blue())
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
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let r = repo_chip(ui, "Photos", true, theme::orange(), false, Some(true));
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
                    crate::theme::apply(ui.ctx(), crate::theme::DARK);
                    init = true;
                }
                let r = repo_chip(ui, "Videos", false, theme::blue(), false, None);
                assert!(r.lock.is_none(), "no lock response when lock is None");
            });
        harness.run();
        assert!(harness.query_by_label("Videos").is_some());
        assert!(
            harness.query_by_label("MAIN").is_none(),
            "an ordinary repo carries no main badge"
        );
    }

    /// A sync-group main is badged, and the badge sits inside the chip frame
    /// rather than spilling past it — a geometric check, because a label query
    /// alone passes even when the glyph is painted outside its parent.
    #[test]
    fn main_chip_shows_a_badge_inside_the_frame() {
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(300.0, 80.0))
            .build_ui(move |ui| {
                if !init {
                    crate::icon::install(ui.ctx());
                    crate::theme::apply(ui.ctx(), crate::theme::DARK);
                    init = true;
                }
                let r = repo_chip(ui, "Photos", false, theme::orange(), true, None);
                let outer = r.outer.rect;
                ui.ctx()
                    .memory_mut(|m| m.data.insert_temp("outer".into(), outer));
            });
        harness.run();

        let badge = harness.get_by_label("MAIN").rect();
        let outer: egui::Rect = harness
            .ctx
            .memory(|m| m.data.get_temp("outer".into()))
            .expect("chip frame rect recorded");
        assert!(
            outer.contains_rect(badge),
            "badge {badge:?} must sit inside the chip frame {outer:?}"
        );
        assert!(
            harness.query_by_label("Photos").is_some(),
            "the name still renders alongside the badge"
        );
    }

    /// The badge widens the chip instead of overlapping the name — `chip_row`
    /// packs rows from the measured frame width, so a badge that did not claim
    /// space would make chips overlap when a row wraps.
    #[test]
    fn main_badge_widens_the_chip() {
        fn width(main: bool) -> f32 {
            let mut init = false;
            let mut harness = Harness::builder()
                .with_size(egui::vec2(300.0, 80.0))
                .build_ui_state(
                    move |ui, w: &mut f32| {
                        if !init {
                            crate::icon::install(ui.ctx());
                            crate::theme::apply(ui.ctx(), crate::theme::DARK);
                            init = true;
                        }
                        *w = repo_chip(ui, "Photos", false, theme::orange(), main, None)
                            .outer
                            .rect
                            .width();
                    },
                    0.0,
                );
            harness.run();
            *harness.state()
        }
        let plain = width(false);
        let badged = width(true);
        assert!(
            badged > plain + 8.0,
            "badged chip ({badged}) must claim more width than plain ({plain})"
        );
    }
}

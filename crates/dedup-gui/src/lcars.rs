//! LCARS chrome primitives: the pill/bar buttons and the elbow-rail section
//! frame that give the app its "Library Computer Access/Retrieval System" look.
//!
//! Two building blocks, reused across every tab:
//! - [`toggle_button`] — a stadium-capped bar. Selected = filled accent + black
//!   text; unselected = dark panel with an accent **border** and accent text, so
//!   it always reads as clickable (the old borderless pills did not).
//! - [`section_lcars`] — a section framed by a left vertical rail that curves
//!   (elbows) into a horizontal header cap bar carrying the section title. This
//!   is the "lines that reach out from the edges" chrome.

use crate::icon;
use crate::theme;
use egui::{Align2, Color32, CornerRadius, Sense, Stroke, StrokeKind, Vec2, pos2};

/// A dim, same-hue fill for the hover state of an unselected button.
fn tint(accent: Color32) -> Color32 {
    accent.gamma_multiply(0.35)
}

/// An LCARS pill/bar toggle. `selected` fills it with `accent` (black text);
/// otherwise it's a dark bar with an `accent` border + `accent` text that lights
/// up on hover. Returns the click response; the caller reads `.clicked()`.
pub fn toggle_button(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    accent: Color32,
) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text_w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), theme::text())
        .size();
    // Roomy horizontal padding gives the classic long-bar proportions.
    let pad = Vec2::new(16.0, 6.0);
    let size = Vec2::new(text_w.x + pad.x * 2.0, text_w.y + pad.y * 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let (fill, fg, border) = if selected {
            (accent, theme::ink_on(accent), accent)
        } else if resp.hovered() {
            (tint(accent), theme::text(), accent)
        } else {
            (theme::panel(), accent, accent)
        };
        // Stadium caps: the corner radius is half the height, so the ends are
        // fully round bars.
        let r = CornerRadius::same((rect.height() / 2.0) as u8);
        let p = ui.painter();
        p.rect_filled(rect, r, fill);
        p.rect_stroke(rect, r, Stroke::new(1.5, border), StrokeKind::Inside);
        p.text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    }
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, label)
    });
    resp
}

/// A filled LCARS action bar (FIND, AUTO-RESOLVE, DELETE MARKED, …). Always
/// filled with `accent` + black text; dims and stops sensing clicks when
/// `enabled` is false. Returns the click response.
pub fn action_button(
    ui: &mut egui::Ui,
    label: &str,
    enabled: bool,
    accent: Color32,
) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text_w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), theme::black())
        .size();
    let pad = Vec2::new(16.0, 6.0);
    let size = Vec2::new(text_w.x + pad.x * 2.0, text_w.y + pad.y * 2.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let fill = if !enabled {
            accent.gamma_multiply(0.4)
        } else if resp.hovered() {
            accent.gamma_multiply(1.2)
        } else {
            accent
        };
        let fg = if enabled {
            theme::ink_on(accent)
        } else {
            theme::ink_on(accent).gamma_multiply(0.6)
        };
        let r = CornerRadius::same((rect.height() / 2.0) as u8);
        let p = ui.painter();
        p.rect_filled(rect, r, fill);
        p.text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    resp
}

/// Header-bar height (points).
const HEAD_H: f32 = 22.0;

/// A titled section (ADR 0004: LCARS as skin, not layout): a stadium header
/// bar in `accent` with the title set into it, and the body on a panel below.
/// The elbow rail that used to run down the left is gone — it cost every
/// board a rail's width and framed nothing a header bar does not.
///
/// Every section is collapsible: clicking the header bar folds the body down to
/// a single stadium bar (the caret before the title shows the state), and the
/// open state persists under an id derived from `title`. Starts open. Returns
/// the body result, or `None` while collapsed (the body closure is not run).
pub fn section_lcars<R>(
    ui: &mut egui::Ui,
    title: &str,
    accent: Color32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    section_lcars_collapsible(ui, title, accent, true, add)
}

/// A [`section_lcars`] with an explicit initial state (`default_open`), for
/// sections that should start folded.
pub fn section_lcars_collapsible<R>(
    ui: &mut egui::Ui,
    title: &str,
    accent: Color32,
    default_open: bool,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    let id = ui.id().with(("lcars_sec", title));
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        default_open,
    );
    let (out, header) = if state.is_open() {
        let (r, header) = section_impl(ui, title, accent, add);
        (Some(r), header)
    } else {
        (None, collapsed_header(ui, title, accent))
    };
    if header.clicked() {
        state.toggle(ui);
    }
    state.store(ui.ctx());
    out
}

/// The folded state of a view's sections, keyed by section title. Every
/// section is open until the view folds them all (after a FIND, a REVIEW, a
/// PREVIEW); from then on each one flips back open on its own header, and the
/// rest stay folded.
pub struct Folds {
    open: std::collections::HashMap<String, bool>,
    /// What a section not yet in the map is: open until the view folds.
    default_open: bool,
}

impl Default for Folds {
    fn default() -> Self {
        Self {
            open: std::collections::HashMap::new(),
            default_open: true,
        }
    }
}

impl Folds {
    /// Whether `title`'s section is open right now.
    pub fn is_open(&self, title: &str) -> bool {
        *self.open.get(title).unwrap_or(&self.default_open)
    }

    /// Record what the header click left the section as.
    pub fn set(&mut self, title: &str, open: bool) {
        self.open.insert(title.to_string(), open);
    }

    /// Fold every section, including ones this view has not drawn yet (the
    /// titles change with the command).
    pub fn fold_all(&mut self) {
        self.open.clear();
        self.default_open = false;
    }
}

/// A section whose folded state the caller owns, rather than egui's memory:
/// drawn open or as its stadium bar according to `open`, which a click on the
/// header flips. For a view that folds a whole group of sections itself (the
/// selection sections after a FIND) and still lets each one be flipped back
/// open on its own.
pub fn section_lcars_folding<R>(
    ui: &mut egui::Ui,
    title: &str,
    accent: Color32,
    open: &mut bool,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    let (out, header) = if *open {
        let (r, header) = section_impl(ui, title, accent, add);
        (Some(r), header)
    } else {
        (None, collapsed_header(ui, title, accent))
    };
    if header.clicked() {
        *open = !*open;
    }
    out
}

/// The open form of a section: the header bar with a caret-down before the
/// title, and the body on its panel; the clickable header bar's response is
/// returned alongside the body's result.
fn section_impl<R>(
    ui: &mut egui::Ui,
    title: &str,
    accent: Color32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> (R, egui::Response) {
    let header = header_bar(ui, &format!("{} {title}", icon::CARET_DOWN), title, accent);
    let inner = egui::Frame::new()
        .fill(theme::panel())
        .corner_radius(CornerRadius {
            nw: 0,
            ne: 0,
            sw: 8,
            se: 8,
        })
        .inner_margin(egui::Margin {
            left: 10,
            right: 10,
            top: 8,
            bottom: 10,
        })
        .outer_margin(egui::Margin {
            left: 0,
            right: 0,
            top: 0,
            bottom: 8,
        })
        .show(ui, |ui| {
            // Always claim the full panel width, so the header bar spans the
            // whole panel even when the body's own content is narrow.
            ui.set_min_width(ui.available_width());
            add(ui)
        });
    (inner.inner, header)
}

/// The full-width stadium header bar of a section, `painted` set into it in
/// the accent's ink; `title` is what accesskit (tests, screen readers) sees.
fn header_bar(ui: &mut egui::Ui, painted: &str, title: &str, accent: Color32) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), HEAD_H), Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.hovered() {
            accent.gamma_multiply(1.2)
        } else {
            accent
        };
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same((HEAD_H / 2.0) as u8), fill);
        p.text(
            pos2(rect.min.x + 12.0, rect.center().y),
            Align2::LEFT_CENTER,
            painted,
            egui::FontId::proportional(13.0),
            theme::ink_on(accent),
        );
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, title));
    resp
}

/// The collapsed form of a collapsible section: just the header bar, with a
/// "click to open" caret before the title. Same width and bottom spacing as
/// the open section, so collapsing doesn't shift siblings sideways.
fn collapsed_header(ui: &mut egui::Ui, title: &str, accent: Color32) -> egui::Response {
    let resp = header_bar(ui, &format!("{} {title}", icon::CARET_RIGHT), title, accent);
    ui.add_space(8.0); // match the open section's bottom outer margin
    resp
}

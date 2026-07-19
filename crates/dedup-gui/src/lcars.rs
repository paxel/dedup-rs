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
use egui::{Align2, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2, pos2};

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
        .layout_no_wrap(label.to_owned(), font.clone(), theme::TEXT)
        .size();
    // Roomy horizontal padding gives the classic long-bar proportions.
    let pad = Vec2::new(16.0, 6.0);
    let size = text_w + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let (fill, fg, border) = if selected {
            (accent, theme::BLACK, accent)
        } else if resp.hovered() {
            (tint(accent), theme::TEXT, accent)
        } else {
            (theme::PANEL, accent, accent)
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
        .layout_no_wrap(label.to_owned(), font.clone(), theme::BLACK)
        .size();
    let pad = Vec2::new(16.0, 6.0);
    let size = text_w + pad * 2.0;
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
            theme::BLACK
        } else {
            theme::BLACK.gamma_multiply(0.6)
        };
        let r = CornerRadius::same((rect.height() / 2.0) as u8);
        let p = ui.painter();
        p.rect_filled(rect, r, fill);
        p.text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    resp
}

/// Rail width, header-bar height, and the concave inner-elbow radius (points).
const RAIL_W: f32 = 16.0;
const HEAD_H: f32 = 22.0;
const ELBOW_R: f32 = 12.0;

/// A section framed by the LCARS elbow rail. `title` is set into the header cap
/// bar (black on `accent`); `add` renders the section body, inset to the right of
/// the rail and below the header.
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

/// The open form of a section: elbow chrome, caret-down before the title, and
/// the clickable header bar whose response is returned alongside the body's
/// result.
fn section_impl<R>(
    ui: &mut egui::Ui,
    title: &str,
    accent: Color32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> (R, egui::Response) {
    // Reserve a paint slot *behind* the body: the elbow chrome (and the dark body
    // background that forms the concave curve) draw here, so the body's own
    // widgets always render on top of it and are never overpainted.
    let bg = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Frame::new()
        // No fill — the body-panel shape painted into `bg` is the background.
        .inner_margin(egui::Margin {
            // Clear the rail *and* the concave elbow so the first item isn't
            // clipped by the curve; still hug the header vertically.
            left: (RAIL_W + ELBOW_R + 2.0) as i8,
            right: 10,
            top: (HEAD_H + 3.0) as i8,
            bottom: 10,
        })
        .outer_margin(egui::Margin {
            left: 0,
            right: 0,
            top: 0,
            bottom: 8,
        })
        .show(ui, |ui| {
            // Always claim the full panel width, so the elbow rail/header spans
            // the whole panel even when the body's own content is narrow.
            ui.set_min_width(ui.available_width());
            add(ui)
        });
    let r = inner.response.rect;
    let painted = format!("{} {title}", icon::CARET_DOWN);
    ui.painter()
        .set(bg, egui::Shape::Vec(elbow_shapes(ui, r, &painted, accent)));
    // Expose the painted title to accesskit (so tests and screen readers can find
    // the section by name) without affecting layout. The whole header bar is
    // the collapse click target.
    let title_rect = Rect::from_min_max(
        pos2(r.min.x + RAIL_W, r.min.y),
        pos2(r.max.x, r.min.y + HEAD_H),
    );
    let header = ui.interact(
        title_rect,
        ui.id().with(("lcars_title", title)),
        Sense::click(),
    );
    header.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, title));
    (inner.inner, header)
}

/// The collapsed form of a collapsible section: just the header bar, a full
/// stadium with a "click to open" caret before the title. Same width and bottom
/// spacing as the open section, so collapsing doesn't shift siblings sideways.
fn collapsed_header(ui: &mut egui::Ui, title: &str, accent: Color32) -> egui::Response {
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
            pos2(rect.min.x + RAIL_W + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            format!("{} {title}", icon::CARET_RIGHT),
            egui::FontId::proportional(13.0),
            theme::BLACK,
        );
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, title));
    ui.add_space(8.0); // match the open section's bottom outer margin
    resp
}

/// The elbow chrome as a back-to-front shape list: accent rail, accent header cap
/// bar, the dark body panel (its rounded top-left corner reveals the accent
/// behind it — the concave elbow), and the title. Painted behind the body.
fn elbow_shapes(ui: &egui::Ui, rect: Rect, title: &str, accent: Color32) -> Vec<egui::Shape> {
    let out = (RAIL_W / 2.0) as u8; // rounded outer corners
    let cap = (HEAD_H / 2.0) as u8; // rounded right cap on the header bar
    let galley = ui.painter().layout_no_wrap(
        title.to_owned(),
        egui::FontId::proportional(13.0),
        theme::BLACK,
    );
    // Safety net: the panel is always full-width (see `section_lcars`), so this
    // only bites for a title too long even for that — widen the chrome rather
    // than let the galley overflow past the header bar uncontained.
    let min_width = RAIL_W + 10.0 + galley.size().x + 10.0;
    let rect = if rect.width() < min_width {
        Rect::from_min_max(rect.min, pos2(rect.min.x + min_width, rect.max.y))
    } else {
        rect
    };
    let rail = Rect::from_min_max(rect.min, pos2(rect.min.x + RAIL_W, rect.max.y));
    let head = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.min.y + HEAD_H));
    let body = Rect::from_min_max(pos2(rect.min.x + RAIL_W, rect.min.y + HEAD_H), rect.max);
    let tpos = pos2(
        rect.min.x + RAIL_W + 10.0,
        rect.min.y + HEAD_H / 2.0 - galley.size().y / 2.0,
    );
    vec![
        egui::Shape::rect_filled(
            rail,
            CornerRadius {
                nw: out,
                sw: out,
                ne: 0,
                se: 0,
            },
            accent,
        ),
        egui::Shape::rect_filled(
            head,
            CornerRadius {
                nw: out,
                ne: cap,
                sw: 0,
                se: cap,
            },
            accent,
        ),
        // The body panel's rounded top-left corner carves the concave elbow.
        egui::Shape::rect_filled(
            body,
            CornerRadius {
                nw: ELBOW_R as u8,
                ne: 0,
                sw: 0,
                se: 0,
            },
            theme::PANEL,
        ),
        egui::Shape::galley(tpos, galley, theme::BLACK),
    ]
}

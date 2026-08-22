//! The Text tab's aligned, side-by-side diff of two documents' extracted text.
//!
//! Comparing two documents shows their *readable content* line-aligned: equal
//! lines sit across from each other, a line present on only one side leaves the
//! other blank, and a line that changed shows both versions with the differing
//! characters marked. It reuses the same review-board vocabulary as the hex diff
//! — **green** where text exists on only one side (a gap), **amber** where it
//! changed on both — and never renders a verdict: two identical documents simply
//! show no marks.
//!
//! Rows follow the document's own line breaks (not a fixed grid): a line-level
//! LCS pairs equal lines, and [`dedup_core::align`] does the per-character
//! highlight *within* a changed pair.

use crate::theme;
use dedup_core::align::{SegmentKind, align};
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText};

/// Largest line-LCS table (`a_lines × b_lines`) computed exactly; past it the
/// pairing degrades to a positional one and says so. Bounds memory and time.
const LCS_BUDGET: usize = 4_000_000;
/// Ceiling on rendered rows, a guard on very large documents.
const MAX_ROWS: usize = 20_000;

/// How a run of characters relates across the two sides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mark {
    /// Identical on both sides.
    Equal,
    /// Present on only one side — an inserted/removed run (green).
    Gap,
    /// Changed: both sides carry a differing run at this spot (amber).
    Change,
}

/// A coloured run of text within one side of one row.
#[derive(Clone)]
struct Cell {
    text: String,
    mark: Mark,
}

/// One aligned row: the left side's cells and the right side's cells (either may
/// be empty — a blank/gap), and whether this row differs at all.
#[derive(Clone)]
struct Row {
    left: Vec<Cell>,
    right: Vec<Cell>,
    changed: bool,
}

/// A built, side-by-side aligned diff of two documents' extracted text.
pub struct TextDiff {
    rows: Vec<Row>,
    /// The line pairing degraded to a positional match (document too large).
    pub degraded: bool,
    /// The document had more lines than [`MAX_ROWS`] and was cut.
    pub truncated: bool,
}

impl TextDiff {
    /// Build the aligned diff of two extracted-text strings.
    pub fn build(a: &str, b: &str) -> Self {
        let la: Vec<&str> = a.lines().collect();
        let lb: Vec<&str> = b.lines().collect();
        let degraded = la.len().saturating_mul(lb.len()) > LCS_BUDGET;
        let matches = if degraded {
            positional_matches(&la, &lb)
        } else {
            lcs_line_matches(&la, &lb)
        };

        let mut rows = Vec::new();
        let (mut ai, mut bj) = (0usize, 0usize);
        // The trailing sentinel `(len, len)` flushes the final unmatched run; it
        // is not itself an equal line (guarded by `mi < la.len()`).
        for (mi, mj) in matches
            .into_iter()
            .chain(std::iter::once((la.len(), lb.len())))
        {
            emit_changed_block(&mut rows, &la[ai..mi], &lb[bj..mj]);
            if mi < la.len() {
                rows.push(Row {
                    left: vec![Cell {
                        text: la[mi].to_string(),
                        mark: Mark::Equal,
                    }],
                    right: vec![Cell {
                        text: lb[mj].to_string(),
                        mark: Mark::Equal,
                    }],
                    changed: false,
                });
            }
            ai = mi + 1;
            bj = mj + 1;
            if rows.len() >= MAX_ROWS {
                return Self {
                    rows,
                    degraded,
                    truncated: true,
                };
            }
        }
        Self {
            rows,
            degraded,
            truncated: false,
        }
    }

    /// The colour for a mark, in the shared review-board vocabulary.
    fn color(mark: Mark) -> Color32 {
        match mark {
            Mark::Equal => theme::text(),
            Mark::Gap => theme::green(),
            Mark::Change => theme::amber(),
        }
    }

    /// A monospace [`LayoutJob`] for one side of one row, each run in its mark's
    /// colour.
    fn side_job(cells: &[Cell], font: &FontId) -> LayoutJob {
        let mut job = LayoutJob::default();
        for cell in cells {
            job.append(
                &cell.text,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: Self::color(cell.mark),
                    ..Default::default()
                },
            );
        }
        job
    }

    /// Render the diff: two aligned columns in one vertical scroll, a faint band
    /// behind rows that changed.
    pub fn show(&self, ui: &mut egui::Ui) {
        let font = FontId::monospace(12.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.degraded {
                    ui.label(
                        RichText::new(
                            "Very large document — lines paired coarsely; some alignment \
                             may be approximate.",
                        )
                        .color(theme::grey())
                        .size(11.0),
                    );
                }
                let gap = 12.0;
                for row in &self.rows {
                    // Two equal columns. Each side is laid out to `col_w` up
                    // front — its own galley, already wrapped, which egui then
                    // paints without re-flowing — and placed in a child ui at its
                    // column's x. Laying the galley out ourselves is what keeps a
                    // long line wrapping *inside* its column: `set_width` on a
                    // vertical nested in a horizontal did not bound the label's
                    // wrap width, so lines never wrapped and instead ran under the
                    // centre divider and off the far edge. A `Label` (not a bare
                    // painted galley) keeps the text in the accessibility tree, so
                    // it stays queryable in tests and by screen readers.
                    let full = ui.available_width();
                    let col_w = ((full - gap) / 2.0).max(40.0);
                    let mut left = Self::side_job(&row.left, &font);
                    let mut right = Self::side_job(&row.right, &font);
                    left.wrap.max_width = col_w;
                    right.wrap.max_width = col_w;
                    let left = ui.ctx().fonts_mut(|f| f.layout_job(left));
                    let right = ui.ctx().fonts_mut(|f| f.layout_job(right));
                    let row_h = left.rect.height().max(right.rect.height());
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(full, row_h), egui::Sense::hover());
                    if row.changed {
                        ui.painter()
                            .rect_filled(rect, 0.0, theme::amber().gamma_multiply(0.08));
                    }
                    let mut col = |x: f32, galley: std::sync::Arc<egui::Galley>| {
                        let at = egui::Rect::from_min_size(
                            egui::pos2(x, rect.min.y),
                            egui::vec2(col_w, row_h),
                        );
                        let mut child = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(at)
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                        );
                        // Reading (and copying) the text is the point of this
                        // pane, so it opts back into egui's text selection,
                        // which the app-wide style turns off for rows.
                        child.add(egui::Label::new(galley).selectable(true));
                    };
                    col(rect.min.x, left);
                    col(rect.min.x + col_w + gap, right);
                }
                if self.truncated {
                    ui.label(
                        RichText::new("… long document truncated.")
                            .color(theme::grey())
                            .size(11.0),
                    );
                }
            });
    }
}

/// The equal-line pairs `(i, j)` of a line-level LCS: the longest run of lines
/// that appear, in order, in both documents. Everything between consecutive
/// pairs is a changed/inserted/deleted block.
fn lcs_line_matches(la: &[&str], lb: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (la.len(), lb.len());
    // dp[i][j] = LCS length of la[i..] vs lb[j..]. Row-major, (n+1)×(m+1).
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if la[i] == lb[j] {
                dp[at(i + 1, j + 1)] + 1
            } else {
                dp[at(i + 1, j)].max(dp[at(i, j + 1)])
            };
        }
    }
    let mut matches = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if la[i] == lb[j] {
            matches.push((i, j));
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    matches
}

/// Degraded pairing for a document too large for the LCS: match lines that
/// happen to be equal at the same index, nothing more. Cheap, and honest about
/// being approximate (the caller flags it).
fn positional_matches(la: &[&str], lb: &[&str]) -> Vec<(usize, usize)> {
    (0..la.len().min(lb.len()))
        .filter(|&i| la[i] == lb[i])
        .map(|i| (i, i))
        .collect()
}

/// Emit rows for a changed block — a run of unmatched lines on each side. Lines
/// are paired positionally (line 1 of A's block against line 1 of B's block,
/// character-diffed); any surplus lines on one side become one-sided gap rows.
fn emit_changed_block(rows: &mut Vec<Row>, a_lines: &[&str], b_lines: &[&str]) {
    let paired = a_lines.len().min(b_lines.len());
    for idx in 0..paired {
        rows.push(changed_pair(a_lines[idx], b_lines[idx]));
    }
    for line in &a_lines[paired..] {
        rows.push(Row {
            left: vec![Cell {
                text: line.to_string(),
                mark: Mark::Gap,
            }],
            right: Vec::new(),
            changed: true,
        });
    }
    for line in &b_lines[paired..] {
        rows.push(Row {
            left: Vec::new(),
            right: vec![Cell {
                text: line.to_string(),
                mark: Mark::Gap,
            }],
            changed: true,
        });
    }
}

/// A row for two differing lines: the shared prefix/suffix stay plain, the
/// differing characters are amber (changed on both) or green (only one side).
/// Character alignment reuses [`dedup_core::align`]; slices are taken with
/// [`String::from_utf8_lossy`] so a byte boundary inside a multi-byte character
/// can never panic.
fn changed_pair(a: &str, b: &str) -> Row {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let alignment = align(ab, bb);
    let mut left = Vec::new();
    let mut right = Vec::new();
    for seg in &alignment.segments {
        let at = String::from_utf8_lossy(&ab[seg.a.clone()]).into_owned();
        let bt = String::from_utf8_lossy(&bb[seg.b.clone()]).into_owned();
        match seg.kind {
            SegmentKind::Equal => {
                if !at.is_empty() {
                    left.push(Cell {
                        text: at,
                        mark: Mark::Equal,
                    });
                }
                if !bt.is_empty() {
                    right.push(Cell {
                        text: bt,
                        mark: Mark::Equal,
                    });
                }
            }
            SegmentKind::Diff => {
                let mark = if !at.is_empty() && !bt.is_empty() {
                    Mark::Change
                } else {
                    Mark::Gap
                };
                if !at.is_empty() {
                    left.push(Cell { text: at, mark });
                }
                if !bt.is_empty() {
                    right.push(Cell { text: bt, mark });
                }
            }
        }
    }
    Row {
        left,
        right,
        changed: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn left_text(d: &TextDiff, i: usize) -> String {
        d.rows[i].left.iter().map(|c| c.text.as_str()).collect()
    }
    fn right_text(d: &TextDiff, i: usize) -> String {
        d.rows[i].right.iter().map(|c| c.text.as_str()).collect()
    }
    fn marked(d: &TextDiff, mark: Mark) -> Vec<String> {
        d.rows
            .iter()
            .flat_map(|r| r.left.iter().chain(r.right.iter()))
            .filter(|c| c.mark == mark)
            .map(|c| c.text.clone())
            .collect()
    }

    #[test]
    fn a_changed_line_is_marked_and_aligns_with_the_equal_ones() {
        let a = "Dear Bob\nAmount is A\nRegards";
        let b = "Dear Bob\nAmount is B\nRegards";
        let d = TextDiff::build(a, b);
        assert_eq!(d.rows.len(), 3);
        assert!(!d.rows[0].changed, "greeting is equal");
        assert!(d.rows[1].changed, "the amount line changed");
        assert!(!d.rows[2].changed, "sign-off is equal");
        assert_eq!(left_text(&d, 1), "Amount is A");
        assert_eq!(right_text(&d, 1), "Amount is B");
        // Only the differing character is amber — the shared prefix is not.
        let amber = marked(&d, Mark::Change);
        assert!(amber.contains(&"A".to_string()), "A-side change: {amber:?}");
        assert!(amber.contains(&"B".to_string()), "B-side change: {amber:?}");
        assert!(
            amber.iter().all(|t| !t.contains("Amount")),
            "the shared prefix is not marked: {amber:?}"
        );
    }

    #[test]
    fn an_inserted_line_is_a_one_sided_gap() {
        let a = "one\nthree";
        let b = "one\ntwo\nthree";
        let d = TextDiff::build(a, b);
        assert_eq!(d.rows.len(), 3);
        assert!(!d.rows[0].changed);
        assert!(d.rows[1].changed, "the inserted line is a change row");
        assert_eq!(left_text(&d, 1), "", "left blank where B inserted a line");
        assert_eq!(right_text(&d, 1), "two");
        assert!(!d.rows[2].changed);
        // An inserted line is a gap (green), not a substitution (amber).
        assert!(marked(&d, Mark::Gap).contains(&"two".to_string()));
        assert!(marked(&d, Mark::Change).is_empty());
    }

    #[test]
    fn identical_text_has_no_changed_rows() {
        let d = TextDiff::build("same\ncontent\nhere", "same\ncontent\nhere");
        assert_eq!(d.rows.len(), 3);
        assert!(
            (0..d.rows.len()).all(|i| !d.rows[i].changed),
            "nothing is marked when the documents match — and no verdict is drawn"
        );
    }
}

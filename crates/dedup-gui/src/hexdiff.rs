//! The Text tab's full-file, aligned, paginated hex diff.
//!
//! [`HexDiff::build`] runs the core byte-alignment engine over both files and
//! flattens the result into a stream of aligned *units* — one per byte position,
//! carrying the byte on each side (either may be absent, which is a gap). The
//! stream renders as a dual hex dump where equal bytes line up, a gap shows as a
//! blank on one side, and a substitution shows differing bytes on both, coloured
//! in the review-board vocabulary (green = gap, amber = substitution). It
//! paginates, jumps between differences, and says when it read only part of a
//! very large file or fell back to a coarse alignment.

use crate::theme;
use dedup_core::align::{SegmentKind, align};
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

/// Bytes per row, per side.
const ROW: usize = 16;
/// Rows shown per page.
pub const ROWS_PER_PAGE: usize = 40;
/// Largest slice read from each file — more than "the header", bounded so a huge
/// file can't exhaust memory. Truncation is surfaced, never silent.
const MAX_READ: usize = 8 * 1024 * 1024;
/// Ceiling on the flattened unit stream, a second guard on memory.
const MAX_UNITS: usize = 4 * 1024 * 1024;

/// One aligned byte position: the byte on each side (either may be absent — a
/// gap), and whether this position is part of a differing run. Offsets are `u32`
/// — inputs are capped at [`MAX_READ`], well under 4 GiB — to keep the unit
/// small, since there is one per byte.
#[derive(Clone, Copy)]
struct Unit {
    a: Option<(u32, u8)>,
    b: Option<(u32, u8)>,
    diff: bool,
}

/// A built, paginable aligned hex diff of two byte streams.
pub struct HexDiff {
    units: Vec<Unit>,
    /// Row indices (into the paginated grid) that contain at least one diff.
    diff_rows: Vec<usize>,
    /// The alignment degraded to a coarse block-level match (large input).
    pub degraded: bool,
    /// One or both sides were longer than [`MAX_READ`] and were truncated.
    pub truncated: bool,
    /// A side could not be read from disk (treated as empty here) — surfaced so
    /// the diff is not silently read as "the whole other side was inserted".
    pub read_failed: bool,
}

impl HexDiff {
    /// Build the aligned diff from the two files' (already-read, possibly
    /// truncated) byte slices. `truncated` says whether the caller cut either
    /// side to [`MAX_READ`].
    pub fn build(a: &[u8], b: &[u8], truncated: bool, read_failed: bool) -> Self {
        let alignment = align(a, b);
        let mut units = Vec::new();
        let mut truncated = truncated;
        'outer: for seg in &alignment.segments {
            let diff = seg.kind == SegmentKind::Diff;
            let (mut ai, mut bi) = (seg.a.start, seg.b.start);
            while ai < seg.a.end || bi < seg.b.end {
                let ua = (ai < seg.a.end).then(|| (ai as u32, a[ai]));
                let ub = (bi < seg.b.end).then(|| (bi as u32, b[bi]));
                if ua.is_some() {
                    ai += 1;
                }
                if ub.is_some() {
                    bi += 1;
                }
                units.push(Unit { a: ua, b: ub, diff });
                if units.len() >= MAX_UNITS {
                    truncated = true;
                    break 'outer;
                }
            }
        }
        let mut diff_rows = Vec::new();
        for (row, chunk) in units.chunks(ROW).enumerate() {
            if chunk.iter().any(|u| u.diff) {
                diff_rows.push(row);
            }
        }
        Self {
            units,
            diff_rows,
            degraded: alignment.degraded,
            truncated,
            read_failed,
        }
    }

    /// Total number of rows across all pages.
    pub fn rows(&self) -> usize {
        self.units.len().div_ceil(ROW)
    }

    /// Number of pages.
    pub fn pages(&self) -> usize {
        self.rows().div_ceil(ROWS_PER_PAGE).max(1)
    }

    /// Whether the two streams are identical (no differing run at all).
    pub fn is_identical(&self) -> bool {
        self.diff_rows.is_empty()
    }

    /// The page holding the first difference at or after `from_row`, wrapping to
    /// the top — for "jump to next difference".
    pub fn next_diff_page(&self, current_page: usize) -> Option<usize> {
        if self.diff_rows.is_empty() {
            return None;
        }
        let after = (current_page + 1) * ROWS_PER_PAGE;
        let target = self
            .diff_rows
            .iter()
            .copied()
            .find(|&r| r >= after)
            .or_else(|| self.diff_rows.first().copied())?;
        Some(target / ROWS_PER_PAGE)
    }

    /// The page holding the last difference before `current_page`, wrapping — for
    /// "jump to previous difference".
    pub fn prev_diff_page(&self, current_page: usize) -> Option<usize> {
        if self.diff_rows.is_empty() {
            return None;
        }
        let before = current_page * ROWS_PER_PAGE;
        let target = self
            .diff_rows
            .iter()
            .rev()
            .copied()
            .find(|&r| r < before)
            .or_else(|| self.diff_rows.last().copied())?;
        Some(target / ROWS_PER_PAGE)
    }

    /// Colour for a differing unit: green when it is a gap (a byte on only one
    /// side), amber when both sides carry a differing byte (a substitution).
    fn diff_color(u: &Unit) -> Color32 {
        if u.a.is_some() && u.b.is_some() {
            theme::amber()
        } else {
            theme::green()
        }
    }

    /// Build a monospace [`LayoutJob`] for one side of one row: offset, then 16
    /// hex bytes (blank where that side has a gap), then the ASCII gutter.
    fn side_job(&self, row: usize, left: bool, font: &FontId) -> LayoutJob {
        let base = row * ROW;
        let mut job = LayoutJob::default();
        let plain = |job: &mut LayoutJob, s: &str, c: Color32| {
            job.append(
                s,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: c,
                    ..Default::default()
                },
            );
        };
        // Offset: the first present byte's offset on this side in this row.
        let offset = (0..ROW)
            .filter_map(|i| self.units.get(base + i))
            .find_map(|u| if left { u.a } else { u.b })
            .map(|(off, _)| off);
        match offset {
            Some(off) => plain(&mut job, &format!("{off:08x}  "), theme::grey()),
            None => plain(&mut job, "          ", theme::grey()),
        }
        // Hex bytes.
        let mut ascii = String::new();
        for i in 0..ROW {
            match self.units.get(base + i) {
                Some(u) => {
                    let side = if left { u.a } else { u.b };
                    match side {
                        Some((_, byte)) => {
                            let color = if u.diff {
                                Self::diff_color(u)
                            } else {
                                theme::text()
                            };
                            plain(&mut job, &format!("{byte:02x} "), color);
                            ascii.push(if (0x20..0x7f).contains(&byte) {
                                byte as char
                            } else {
                                '.'
                            });
                        }
                        None => {
                            plain(&mut job, "   ", theme::text());
                            ascii.push(' ');
                        }
                    }
                }
                None => {
                    plain(&mut job, "   ", theme::text());
                    ascii.push(' ');
                }
            }
        }
        plain(&mut job, " ", theme::text());
        plain(&mut job, &ascii, theme::tan());
        job
    }

    /// Render one page: the aligned rows, left side and right side per row, with
    /// a faint band behind rows that carry a difference.
    pub fn show_page(&self, ui: &mut egui::Ui, page: usize) {
        // A touch smaller than the single-file Hex view (12.0): here two full
        // hex+ASCII rows must sit side by side, each in half the width, so 11.0
        // lets the ASCII gutter fit without clipping at a normal window size.
        let font = FontId::monospace(11.0);
        let row_h = ui
            .painter()
            .layout_no_wrap("0".to_owned(), font.clone(), theme::grey())
            .rect
            .height();
        let start = page * ROWS_PER_PAGE;
        let end = (start + ROWS_PER_PAGE).min(self.rows());
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Split each row into two equal, explicitly-sized halves. The
                // bug this fixes: the left label used to take its natural width
                // and the right got only whatever was left, which — once the
                // vertical scrollbar shaved a few pixels — was too little for
                // the right side, so its ASCII gutter wrapped to a second line
                // while the left stayed on one. Sizing both sides from the same
                // half makes the two columns symmetric at any window width; each
                // side clips its (non-wrapping) row at the column edge, so a hex
                // dump always reads as a grid.
                let gap = 8.0;
                for row in start..end {
                    let is_diff = self
                        .units
                        .get(row * ROW..(row * ROW + ROW).min(self.units.len()))
                        .is_some_and(|c| c.iter().any(|u| u.diff));
                    let half = ((ui.available_width() - gap) / 2.0).max(10.0);
                    let resp = ui.horizontal(|ui| {
                        Self::hex_side(ui, self.side_job(row, true, &font), half, row_h);
                        ui.add_space(gap);
                        Self::hex_side(ui, self.side_job(row, false, &font), half, row_h);
                    });
                    // A faint band marks the differing region as a whole, over
                    // which the per-byte green/amber shows exactly what changed.
                    if is_diff {
                        ui.painter().rect_filled(
                            resp.response.rect.expand2(egui::vec2(0.0, 1.0)),
                            0.0,
                            theme::amber().gamma_multiply(0.10),
                        );
                    }
                }
            });
    }

    /// Draw one side's row in a fixed-width column, clipped so a row that would
    /// overrun its half is cut cleanly at the edge rather than wrapping. A clean
    /// clip loses fewer forensic bytes than an ellipsis would hide, and keeps the
    /// two sides aligned.
    fn hex_side(ui: &mut egui::Ui, job: LayoutJob, w: f32, h: f32) {
        ui.allocate_ui_with_layout(
            egui::vec2(w, h),
            egui::Layout::left_to_right(egui::Align::TOP),
            |ui| {
                ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
            },
        );
    }
}

/// Bytes read from one side for the diff, capped at [`MAX_READ`].
pub struct Capped {
    pub bytes: Vec<u8>,
    /// The file was longer than [`MAX_READ`] and only its head was read.
    pub truncated: bool,
    /// The file could not be read at all (missing/permission) — the caller must
    /// not present the empty result as genuine content.
    pub read_ok: bool,
}

/// Read up to [`MAX_READ`] bytes of a file, reporting truncation and whether the
/// read succeeded at all.
pub fn read_capped(path: &std::path::Path) -> Capped {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() > MAX_READ => Capped {
            bytes: bytes[..MAX_READ].to_vec(),
            truncated: true,
            read_ok: true,
        },
        Ok(bytes) => Capped {
            bytes,
            truncated: false,
            read_ok: true,
        },
        Err(_) => Capped {
            bytes: Vec::new(),
            truncated: false,
            read_ok: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inserted_header_aligns_the_shared_payload_and_marks_the_gap() {
        let a = b"PAYLOAD-shared-and-fairly-long-so-it-spans-rows".to_vec();
        let mut b = b"HDR".to_vec();
        b.extend_from_slice(&a);
        let diff = HexDiff::build(&a, &b, false, false);
        assert!(!diff.degraded);
        assert!(!diff.is_identical(), "the inserted header is a difference");
        assert!(diff.rows() > 0);
        // There is a jump target, and it lands on a real page.
        let jump = diff.next_diff_page(0);
        assert!(jump.is_some(), "there is a difference to jump to");
    }

    #[test]
    fn identical_inputs_have_no_differences_to_jump_to() {
        let a = b"exactly the same on both sides".to_vec();
        let diff = HexDiff::build(&a, &a, false, false);
        assert!(diff.is_identical());
        assert_eq!(diff.next_diff_page(0), None);
        assert_eq!(diff.prev_diff_page(0), None);
    }

    #[test]
    fn jump_wraps_around_the_file() {
        // A difference near the end; from page 0, next-diff finds it, and
        // prev-diff from page 0 wraps back to it.
        let a: Vec<u8> = (0..4000u32).map(|i| i as u8).collect();
        let mut b = a.clone();
        *b.last_mut().expect("non-empty") = 0xFF;
        let diff = HexDiff::build(&a, &b, false, false);
        assert!(!diff.is_identical());
        let last_page = diff.pages() - 1;
        assert_eq!(diff.next_diff_page(0), Some(last_page));
        assert_eq!(
            diff.prev_diff_page(0),
            Some(last_page),
            "prev wraps to the end"
        );
    }
}

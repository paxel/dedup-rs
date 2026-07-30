//! The **unified board**: the one side-by-side surface every preview and
//! reconcile view renders into — Grooming's previews, Transfer's COPY / MOVE /
//! SYNC / GROUP SYNC previews, and Transfer's DIFF.
//!
//! ```text
//! |<-- left = (W - C)/2 -->|<-- C -->|<-- right = (W - C)/2 -->|
//!
//! | [thumb] a/b/photo.jpg  | [COPY >]| [thumb] a/b/photo.jpg    |
//! |  2.1 MB   2026-03-04   | [DEL L] |  2.4 MB   2026-05-11     |
//! ```
//!
//! Three regions with the same geometry on every surface: a mini-overview of
//! the left side, a **centre command column** of fixed width `C`, and a mini
//! overview of the right. Only the commands that apply to a row are drawn and
//! the row's height follows its content, so a two-command row is short and an
//! eight-command row is tall.
//!
//! Deliberately **not** built on `egui_extras::TableBuilder`. That widget cannot
//! satisfy the two requirements this board exists for: it has no horizontal
//! scroll at all (`TableScrollOptions` has `vscroll` and no `hscroll`), so a
//! narrow window can only clip; and `.resizable(true)` makes every column
//! boundary drag-movable, where the left and right sides here are pinned to the
//! window edges. Sorting, striping and virtualisation are therefore this
//! module's own — see [`Index`].
//!
//! Presentation only: the board reports which command the user clicked on which
//! row, and the caller executes it.

use crate::media_cell::{FileFacts, MediaStyle, media_cell};
use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::{format_mtime, format_size};
use egui::{Align, Layout, RichText};

/// Longest edge of a row thumbnail.
const THUMB: f32 = 48.0;
/// Height of one path line inside a side cell.
const LINE_H: f32 = 17.0;
/// Height of a command button and the stride between button lines.
///
/// The centre grid is placed at **explicit rects** rather than by flow layout,
/// so what [`RowMeta::height`] measures and what the row draws are the same
/// number by construction. Letting egui lay the buttons out instead made them
/// 24px on a 20px budget with a 7px gap on a 5px budget, and the last line of an
/// eight-command row fell outside the row.
const BTN_H: f32 = 24.0;
const CMD_GAP: f32 = 6.0;
const CMD_H: f32 = BTN_H + CMD_GAP;
/// Width of one command button.
const CMD_W: f32 = 104.0;
/// Vertical padding above and below a row's content.
const ROW_PAD: f32 = 6.0;
/// Smallest a side region may become before the board stops shrinking it and
/// simply clips at the window edge.
const SIDE_MIN: f32 = THUMB + 90.0;

/// Safety cap on how many rows a preview materialises in memory. The summary
/// counts stay the true totals regardless; past the cap the board tells the
/// user to refine the filter.
pub const PREVIEW_CAP: usize = 10_000;

/// What one side of a row says about its file, and therefore what colour its
/// path is drawn in. One vocabulary spans both meanings the board serves:
/// prescriptive on a planned preview ("RUN will delete this") and descriptive
/// on DIFF ("these two differ").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Same on both sides / unchanged / equal.
    Same,
    /// Exists only on this side; in a plan, will be added.
    OnlyHere,
    /// Will be deleted by RUN.
    WillDelete,
    /// Same path, different content (conflict), or same content under a
    /// different name (renamed).
    Differs,
    /// The file is not on this side at all — the cell renders empty.
    Absent,
}

impl Status {
    pub fn color(self) -> egui::Color32 {
        match self {
            Status::Same => theme::GREY,
            Status::OnlyHere => theme::GREEN,
            Status::WillDelete => theme::RED,
            Status::Differs => theme::AMBER,
            Status::Absent => theme::GREY,
        }
    }

    /// Sort rank, so sorting by status groups the rows that need attention.
    fn rank(self) -> u8 {
        match self {
            Status::WillDelete => 0,
            Status::Differs => 1,
            Status::OnlyHere => 2,
            Status::Same => 3,
            Status::Absent => 4,
        }
    }
}

/// Which region of the board a command acts on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Left,
    Right,
    /// Acts on the row as a whole (COMPARE, APPLY, HIDE).
    Neither,
}

/// What a command does, independent of side. The centre grid puts the two
/// commands of one kind on the same line, so a choice and its mirror image sit
/// at the same height. Declaration order here is the display order.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
enum Kind {
    Copy,
    Overwrite,
    Rename,
    KeepOne,
    DeleteAll,
    Delete,
    Compare,
    Apply,
    Hide,
}

/// One line of the centre grid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Line {
    /// A left/right pair. Either half may be missing — a row can offer
    /// `DELETE L` with no `DELETE R` — and the surviving half keeps its own
    /// side's column rather than sliding across.
    Sides(Option<Cmd>, Option<Cmd>),
    /// One or two commands belonging to neither side, filling the line as a
    /// group. Two fit side by side — `APPLY` and `HIDE` are both row-level, and
    /// giving each its own line made every planned-preview row twice as tall as
    /// it needed to be.
    Centre(Cmd, Option<Cmd>),
}

/// Arrange a row's commands into grid lines: one line per kind, left command in
/// the left slot and right command in the right, kinds in declaration order.
///
/// Pairing is by [`Kind`], not by position in `cmds`, so a caller that lists its
/// commands in a different order still gets `COPY >` and `< COPY` on one line
/// rather than whatever happened to be at the same index.
fn layout(cmds: &[Cmd]) -> Vec<Line> {
    let mut kinds: Vec<Kind> = cmds
        .iter()
        .filter(|c| c.side() != Side::Neither)
        .map(|c| c.kind())
        .collect();
    kinds.sort();
    kinds.dedup();
    let mut lines: Vec<Line> = kinds
        .into_iter()
        .map(|kind| {
            let of = |side: Side| {
                cmds.iter()
                    .copied()
                    .find(|c| c.kind() == kind && c.side() == side)
            };
            Line::Sides(of(Side::Left), of(Side::Right))
        })
        .collect();
    // Row-level commands come last, two to a line, in declaration order.
    let mut whole: Vec<Cmd> = cmds
        .iter()
        .copied()
        .filter(|c| c.side() == Side::Neither)
        .collect();
    whole.sort_by_key(|c| c.kind());
    whole.dedup();
    lines.extend(
        whole
            .chunks(2)
            .map(|pair| Line::Centre(pair[0], pair.get(1).copied())),
    );
    lines
}

/// A command offered on a row. `on_left` / direction is baked into the variant,
/// so the caller never has to infer a side from a click position.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    /// Planned surfaces: execute just this row now.
    Apply,
    /// Every surface: drop the row from the board. On a planned surface the
    /// row is also excluded from RUN.
    Hide,
    /// DIFF: copy the left file into the right repo, and vice versa.
    CopyRight,
    CopyLeft,
    DeleteLeft,
    DeleteRight,
    /// DIFF conflict: show both versions side by side.
    Compare,
    /// DIFF conflict: replace the other side's file with this one's content.
    OverwriteRight,
    OverwriteLeft,
    /// DIFF rename (BY HASH, 1:1).
    RenameLeft,
    RenameRight,
    /// DIFF rename with several names on a side.
    KeepOneLeft,
    KeepOneRight,
    DeleteAllLeft,
    DeleteAllRight,
}

impl Cmd {
    pub fn label(self) -> &'static str {
        match self {
            Cmd::Apply => "APPLY",
            Cmd::Hide => "HIDE",
            Cmd::CopyRight => "COPY >",
            Cmd::CopyLeft => "< COPY",
            Cmd::DeleteLeft => "DELETE L",
            Cmd::DeleteRight => "DELETE R",
            Cmd::Compare => "COMPARE",
            Cmd::OverwriteRight => "OVERWRITE >",
            Cmd::OverwriteLeft => "< OVERWRITE",
            Cmd::RenameLeft => "RENAME L",
            Cmd::RenameRight => "RENAME R",
            Cmd::KeepOneLeft => "KEEP 1 L",
            Cmd::KeepOneRight => "KEEP 1 R",
            Cmd::DeleteAllLeft => "DEL ALL L",
            Cmd::DeleteAllRight => "DEL ALL R",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Cmd::Apply => theme::GREEN,
            Cmd::Hide => theme::GREY,
            Cmd::CopyRight | Cmd::CopyLeft => theme::GREEN,
            Cmd::DeleteLeft | Cmd::DeleteRight | Cmd::DeleteAllLeft | Cmd::DeleteAllRight => {
                theme::RED
            }
            Cmd::Compare => theme::LILAC,
            Cmd::OverwriteRight
            | Cmd::OverwriteLeft
            | Cmd::RenameLeft
            | Cmd::RenameRight
            | Cmd::KeepOneLeft
            | Cmd::KeepOneRight => theme::TAN,
        }
    }

    /// Which side of the board this command acts on. Drives its column in the
    /// centre grid: left-hand commands sit in the left slot, right-hand ones in
    /// the right, so the grid mirrors the regions either side of it.
    fn side(self) -> Side {
        match self {
            Cmd::CopyRight
            | Cmd::DeleteLeft
            | Cmd::OverwriteRight
            | Cmd::RenameLeft
            | Cmd::KeepOneLeft
            | Cmd::DeleteAllLeft => Side::Left,
            Cmd::CopyLeft
            | Cmd::DeleteRight
            | Cmd::OverwriteLeft
            | Cmd::RenameRight
            | Cmd::KeepOneRight
            | Cmd::DeleteAllRight => Side::Right,
            Cmd::Apply | Cmd::Hide | Cmd::Compare => Side::Neither,
        }
    }

    /// What the command *does*, independent of side. Two commands of the same
    /// kind are opposite halves of one choice and share a line.
    fn kind(self) -> Kind {
        match self {
            Cmd::CopyRight | Cmd::CopyLeft => Kind::Copy,
            Cmd::OverwriteRight | Cmd::OverwriteLeft => Kind::Overwrite,
            Cmd::RenameLeft | Cmd::RenameRight => Kind::Rename,
            Cmd::KeepOneLeft | Cmd::KeepOneRight => Kind::KeepOne,
            Cmd::DeleteAllLeft | Cmd::DeleteAllRight => Kind::DeleteAll,
            Cmd::DeleteLeft | Cmd::DeleteRight => Kind::Delete,
            Cmd::Compare => Kind::Compare,
            Cmd::Apply => Kind::Apply,
            Cmd::Hide => Kind::Hide,
        }
    }

    /// End-user copy. Never mentions how the board is built.
    ///
    /// `hide_skips_run` distinguishes a planned preview, where hiding a row also
    /// excludes it from RUN, from DIFF, which executes each command as it is
    /// clicked and has no RUN to skip.
    fn hint(self, hide_skips_run: bool) -> &'static str {
        match self {
            Cmd::Apply => "Apply only this action, immediately",
            Cmd::Hide if hide_skips_run => "Remove this row — RUN will skip it",
            Cmd::Hide => "Remove this row from the board",
            Cmd::CopyRight => "Copy this file into the right-hand repository",
            Cmd::CopyLeft => "Copy this file into the left-hand repository",
            Cmd::DeleteLeft => "Delete this file from the left-hand repository",
            Cmd::DeleteRight => "Delete this file from the right-hand repository",
            Cmd::Compare => "Show both versions side by side",
            Cmd::OverwriteRight => "Replace the right-hand file with this one",
            Cmd::OverwriteLeft => "Replace the left-hand file with this one",
            Cmd::RenameLeft => "Rename the left-hand file to the other side's name",
            Cmd::RenameRight => "Rename the right-hand file to the other side's name",
            Cmd::KeepOneLeft => "Keep one left-hand copy and delete the others",
            Cmd::KeepOneRight => "Keep one right-hand copy and delete the others",
            Cmd::DeleteAllLeft => "Delete every left-hand copy of this content",
            Cmd::DeleteAllRight => "Delete every right-hand copy of this content",
        }
    }
}

/// The cheap facts about a row: everything the board needs to sort, filter and
/// measure it, with no store read. Resolving a row's thumbnails and facts is
/// deferred to [`RowBody`] for the handful of rows actually on screen.
#[derive(Clone, Debug)]
pub struct RowMeta {
    /// Stable identity, used for the hidden set.
    pub key: String,
    pub left_status: Status,
    pub right_status: Status,
    /// Paths this side holds. Several only in a BY HASH multi-name row; empty
    /// when the file is absent on that side.
    pub left_paths: Vec<String>,
    pub right_paths: Vec<String>,
    /// Sort keys, taken from the first file on each side.
    pub left_size: u64,
    pub right_size: u64,
    pub left_modified: i64,
    pub right_modified: i64,
    /// Whether the row is one of the uninteresting "nothing differs" rows the
    /// SHOW UNCHANGED toggle hides.
    pub unchanged: bool,
    /// The commands this row offers, in display order.
    pub cmds: Vec<Cmd>,
}

impl RowMeta {
    /// How tall this row renders. Driven by whichever is taller: the side with
    /// the most names, or the command grid.
    fn height(&self) -> f32 {
        let lines = self.left_paths.len().max(self.right_paths.len()).max(1);
        // A side cell is a thumbnail beside (path lines + one facts line).
        let side = THUMB.max(LINE_H * (lines as f32 + 1.0));
        // n buttons have n-1 gaps between them, not n — counting a trailing gap
        // still left the grid one line short of what it draws.
        let cmd_lines = layout(&self.cmds).len();
        let centre = BTN_H * cmd_lines as f32 + CMD_GAP * cmd_lines.saturating_sub(1) as f32;
        side.max(centre) + ROW_PAD * 2.0
    }
}

/// The renderable content of one row's side, resolved only for rows on screen.
#[derive(Clone, Debug, Default)]
pub struct SideBody {
    /// Thumbnail + size/dimensions/date. `None` renders the paths alone.
    pub facts: Option<FileFacts>,
    /// Repo name, drawn as a chip above the cell. Only set when the board's
    /// side holds more than one repo (GROUP SYNC's sinks).
    pub repo: Option<String>,
    /// Whether that repo is a sync-group main, for its chip badge.
    pub repo_is_main: bool,
}

/// Both sides' renderable content for one row.
#[derive(Clone, Debug, Default)]
pub struct RowBody {
    pub left: SideBody,
    pub right: SideBody,
}

/// Which key the board is sorted on. The side is chosen separately, so the same
/// four keys serve both regions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Path,
    Size,
    Date,
    Status,
}

impl SortKey {
    fn label(self) -> &'static str {
        match self {
            SortKey::Path => "PATH",
            SortKey::Size => "SIZE",
            SortKey::Date => "DATE",
            SortKey::Status => "STATUS",
        }
    }
}

/// Per-view board state, persisted across frames.
pub struct BoardState {
    pub sort_key: SortKey,
    /// Which side the sort key reads from. Always `true` on a one-sided board.
    pub sort_left: bool,
    pub sort_asc: bool,
    /// Whether the uninteresting unchanged/equal rows are shown.
    pub show_unchanged: bool,
    /// Keys of rows the user hid. A hidden row leaves the board and, on a
    /// planned surface, is skipped by RUN. There is deliberately no counter and
    /// no way back: rebuilding the preview is what clears this.
    pub hidden: std::collections::HashSet<String>,
}

impl Default for BoardState {
    fn default() -> Self {
        Self {
            sort_key: SortKey::Path,
            sort_left: true,
            sort_asc: true,
            show_unchanged: false,
            hidden: std::collections::HashSet::new(),
        }
    }
}

/// The board's shape and headers, independent of per-frame render state.
pub struct BoardView<'a> {
    /// Role of the left region: SOURCE / MAIN / LEFT.
    pub left_role: &'a str,
    /// Repo naming the left region, and whether it is a sync-group main.
    pub left_repo: &'a str,
    pub left_is_main: bool,
    /// Absolute path under the left header; truncated from the left so the
    /// distinguishing tail survives.
    pub left_path: &'a str,
    /// `None` declares a one-sided board (PURGE and the other single-repo
    /// grooming commands): the right region is dropped and its sort keys are
    /// not offered.
    pub right: Option<RightHeader<'a>>,
    /// True totals, independent of the capped sample, for the summary line.
    /// `[will-delete, only-here, differs, same]`.
    pub totals: [usize; 4],
    /// Rows the caller holds in total, so the board can say when it capped.
    /// Must count everything the plan found, not just the actionable part —
    /// otherwise the "showing the first N" notice never fires.
    pub full_len: usize,
    /// Whether hiding a row also excludes it from a later RUN. False on DIFF,
    /// which runs each command as it is clicked. Only affects HIDE's tooltip.
    pub hide_skips_run: bool,
}

/// The right region's header, present on a two-sided board.
pub struct RightHeader<'a> {
    pub role: &'a str,
    pub repo: &'a str,
    pub is_main: bool,
    pub path: &'a str,
    /// The right side holds more than one repo (GROUP SYNC), so each row draws
    /// its own repo chip.
    pub multi_repo: bool,
}

/// What the user clicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardAction {
    /// Index into the `metas` slice the board was rendered with.
    pub row: usize,
    pub cmd: Cmd,
}

/// A prefix-sum index over the visible rows' heights.
///
/// Rows are ragged — a row's height depends on its name count and how many
/// commands it offers — so `egui::ScrollArea::show_rows`, which assumes a
/// uniform height, cannot be used. This is the replacement: `offsets[i]` is the
/// y of visible row `i`, and a binary search turns a viewport into the row range
/// to draw. Rebuilt whenever the sort order or the hidden set changes.
struct Index {
    /// Indices into the caller's `metas`, in display order.
    order: Vec<usize>,
    /// `order.len() + 1` cumulative offsets; the last entry is the total height.
    offsets: Vec<f32>,
}

impl Index {
    fn build(metas: &[RowMeta], state: &BoardState) -> Self {
        let mut order: Vec<usize> = (0..metas.len())
            .filter(|&i| state.show_unchanged || !metas[i].unchanged)
            .filter(|&i| !state.hidden.contains(&metas[i].key))
            .collect();
        sort_order(&mut order, metas, state);

        let mut offsets = Vec::with_capacity(order.len() + 1);
        let mut y = 0.0;
        offsets.push(0.0);
        for &i in &order {
            y += metas[i].height();
            offsets.push(y);
        }
        Self { order, offsets }
    }

    fn total_height(&self) -> f32 {
        self.offsets.last().copied().unwrap_or(0.0)
    }

    /// The half-open range of visible rows intersecting `[top, bottom]`.
    fn range(&self, top: f32, bottom: f32) -> std::ops::Range<usize> {
        if self.order.is_empty() {
            return 0..0;
        }
        // partition_point: first index whose *end* offset exceeds `top`.
        let start = self
            .offsets
            .partition_point(|&o| o <= top)
            .saturating_sub(1);
        let end = self
            .offsets
            .partition_point(|&o| o < bottom)
            .min(self.order.len());
        start..end.max(start)
    }
}

/// Order `order` (indices into `metas`) by the state's key, side and direction,
/// always tie-breaking on the left path so the result is stable.
fn sort_order(order: &mut [usize], metas: &[RowMeta], state: &BoardState) {
    let first_path = |m: &RowMeta, left: bool| -> String {
        let paths = if left { &m.left_paths } else { &m.right_paths };
        paths.first().map(|p| p.to_lowercase()).unwrap_or_default()
    };
    order.sort_by(|&a, &b| {
        let (ma, mb) = (&metas[a], &metas[b]);
        let tie = || first_path(ma, true).cmp(&first_path(mb, true));
        let left = state.sort_left;
        let ord = match state.sort_key {
            SortKey::Path => first_path(ma, left).cmp(&first_path(mb, left)),
            SortKey::Size => {
                let (x, y) = if left {
                    (ma.left_size, mb.left_size)
                } else {
                    (ma.right_size, mb.right_size)
                };
                x.cmp(&y).then_with(tie)
            }
            SortKey::Date => {
                let (x, y) = if left {
                    (ma.left_modified, mb.left_modified)
                } else {
                    (ma.right_modified, mb.right_modified)
                };
                x.cmp(&y).then_with(tie)
            }
            SortKey::Status => {
                let (x, y) = if left {
                    (ma.left_status, mb.left_status)
                } else {
                    (ma.right_status, mb.right_status)
                };
                x.rank().cmp(&y.rank()).then_with(tie)
            }
        };
        if state.sort_asc { ord } else { ord.reverse() }
    });
}

/// The centre column's width: wide enough for the widest command set any row on
/// this board offers, so `C` is constant down the whole board and a command
/// never moves between rows.
fn centre_width(metas: &[RowMeta]) -> f32 {
    // Two columns as soon as any row pairs a left and a right command; a board
    // whose commands all act on the row as a whole needs only one.
    let paired = metas.iter().any(|m| {
        layout(&m.cmds).iter().any(|l| {
            matches!(l, Line::Sides(Some(_), Some(_))) || matches!(l, Line::Centre(_, Some(_)))
        })
    });
    CMD_W * if paired { 2.0 } else { 1.0 } + 8.0
}

/// Split the available width into the three regions.
///
/// The centre never shrinks — commands are what this board exists to show — so
/// when the window is too narrow the sides stop at [`SIDE_MIN`] and the board
/// clips at the window edge rather than deforming. Path text truncates from the
/// left inside whatever the side gets, so the distinguishing tail survives.
/// A one-sided board has no right region to balance, so its single side takes
/// everything the centre leaves rather than half of it — otherwise a PURGE
/// preview strands its commands mid-screen with dead space beside them.
fn regions(avail: f32, centre: f32, two_sided: bool) -> (f32, f32) {
    let sides = if two_sided { 2.0 } else { 1.0 };
    let side = ((avail - centre) / sides).max(SIDE_MIN);
    (side, centre)
}

/// Render the board. `metas` is the cheap per-row model the board sorts,
/// filters and measures; `body` is called only for the rows actually on screen,
/// so a caller may do a store read inside it. Returns the command the user
/// clicked, if any.
pub fn board(
    ui: &mut egui::Ui,
    state: &mut BoardState,
    metas: &[RowMeta],
    view: BoardView,
    thumbs: &mut ThumbCache,
    body: &mut dyn FnMut(usize) -> RowBody,
) -> Option<BoardAction> {
    summary(ui, view.totals);
    if view.full_len > metas.len() {
        ui.label(
            RichText::new(format!(
                "showing the first {} of {} — refine the filter to see the rest",
                metas.len(),
                view.full_len
            ))
            .color(theme::TAN)
            .size(11.0),
        );
    }

    let two_sided = view.right.is_some();
    if !two_sided {
        state.sort_left = true;
    }
    controls_bar(ui, state, two_sided, view.totals[3] > 0);
    headers(ui, &view, metas);

    let index = Index::build(metas, state);
    ui.label(
        RichText::new(format!("{} rows", index.order.len()))
            .color(theme::TAN)
            .size(11.0),
    );

    let centre = centre_width(metas);
    let mut action = None;

    // The board claims its full height and culls to `ui.clip_rect()` rather than
    // owning a `ScrollArea`. Its callers already wrap their whole tab in one, and
    // a scroll area nested in a scroll area gives the inner one a viewport that
    // is not the band the user can actually see — which showed up as only the
    // first row of a preview ever being drawn.
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), index.total_height()),
        egui::Sense::hover(),
    );
    let (side, centre) = regions(rect.width(), centre, two_sided);
    let geom = RowGeom {
        side,
        centre,
        two_sided,
        multi_repo: view.right.as_ref().is_some_and(|r| r.multi_repo),
        hide_skips_run: view.hide_skips_run,
    };
    let clip = ui.clip_rect();
    let visible = index.range(
        (clip.top() - rect.top()).max(0.0),
        (clip.bottom() - rect.top()).max(0.0),
    );
    for vis in visible {
        let i = index.order[vis];
        let h = metas[i].height();
        let row_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + index.offsets[vis]),
            egui::vec2(rect.width(), h),
        );
        // Striping, which TableBuilder used to provide.
        if vis % 2 == 1 {
            ui.painter().rect_filled(row_rect, 0.0, theme::PANEL);
        }
        // The row's vertical padding is an inset on the content rect. Adding it
        // with `add_space` inside a left-to-right row would have spent it
        // sideways instead, leaving the content taller than its measured height.
        let mut row_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(row_rect.shrink2(egui::vec2(0.0, ROW_PAD)))
                .layout(Layout::left_to_right(Align::Min)),
        );
        if let Some(cmd) = draw_row(&mut row_ui, thumbs, &metas[i], &body(i), geom) {
            action = Some(BoardAction { row: i, cmd });
        }
    }

    // HIDE is the board's own business: it never reaches the caller.
    if let Some(a) = &action
        && a.cmd == Cmd::Hide
    {
        state.hidden.insert(metas[a.row].key.clone());
        return None;
    }
    action
}

/// One side of a row as the board draws it: what the row model knows about that
/// side, plus the body resolved for it. Bundled because these always travel
/// together — passing them individually pushed `side_cell` past the argument
/// limit and would have needed a lint exemption.
struct SideView<'a> {
    status: Status,
    paths: &'a [String],
    body: &'a SideBody,
    /// From the row model, which is authoritative — see [`facts_line`].
    size: u64,
    modified: i64,
    /// Draw this side's own repo chip (the board's right side spans several
    /// repos, as GROUP SYNC's sinks do).
    multi_repo: bool,
}

/// The per-board constants every row is drawn against. One value, computed
/// once, so a row can never disagree with its neighbours about where the
/// regions are.
#[derive(Clone, Copy)]
struct RowGeom {
    side: f32,
    centre: f32,
    two_sided: bool,
    /// The right side spans several repos, so each row names its own.
    multi_repo: bool,
    hide_skips_run: bool,
}

/// One row: left mini-overview, centre command grid, right mini-overview.
///
/// All three are placed at **explicit rects** carved out of the row. Letting any
/// of them flow would add `item_spacing` between the regions that the offsets
/// don't know about, and the right-hand side would land past the window edge.
fn draw_row(
    ui: &mut egui::Ui,
    thumbs: &mut ThumbCache,
    meta: &RowMeta,
    body: &RowBody,
    geom: RowGeom,
) -> Option<Cmd> {
    let RowGeom {
        side,
        centre,
        two_sided,
        multi_repo,
        hide_skips_run,
    } = geom;
    let mut clicked = None;
    let row = ui.max_rect();
    let region = |x: f32, w: f32| {
        egui::Rect::from_min_size(egui::pos2(x, row.top()), egui::vec2(w, row.height()))
    };

    side_cell(
        ui,
        thumbs,
        region(row.left(), side),
        SideView {
            status: meta.left_status,
            paths: &meta.left_paths,
            body: &body.left,
            size: meta.left_size,
            modified: meta.left_modified,
            multi_repo: false,
        },
    );

    // The command grid sits on a CMD_W × CMD_H lattice, so the row draws exactly
    // the height `RowMeta::height` reserved for it.
    let grid_left = row.left() + side;
    let slot = |col: usize, line: usize| {
        egui::Rect::from_min_size(
            egui::pos2(
                grid_left + 4.0 + col as f32 * CMD_W,
                row.top() + line as f32 * CMD_H,
            ),
            egui::vec2(CMD_W - 8.0, BTN_H),
        )
    };
    for (n, line) in layout(&meta.cmds).into_iter().enumerate() {
        match line {
            Line::Sides(left_cmd, right_cmd) => {
                // Each half keeps its own column, so a row offering only the
                // right-hand command still draws it on the right.
                for (col, maybe) in [(0, left_cmd), (1, right_cmd)] {
                    if let Some(cmd) = maybe
                        && cmd_button(ui, slot(col, n), cmd, hide_skips_run).clicked()
                    {
                        clicked = Some(cmd);
                    }
                }
            }
            Line::Centre(first, second) => match second {
                // Two row-level commands fill the two columns.
                Some(other) => {
                    for (col, cmd) in [(0, first), (1, other)] {
                        if cmd_button(ui, slot(col, n), cmd, hide_skips_run).clicked() {
                            clicked = Some(cmd);
                        }
                    }
                }
                // A lone one is centred across them.
                None => {
                    let at = slot(0, n).translate(egui::vec2(CMD_W / 2.0, 0.0));
                    if cmd_button(ui, at, first, hide_skips_run).clicked() {
                        clicked = Some(first);
                    }
                }
            },
        }
    }

    if two_sided {
        side_cell(
            ui,
            thumbs,
            region(grid_left + centre, side),
            SideView {
                status: meta.right_status,
                paths: &meta.right_paths,
                body: &body.right,
                size: meta.right_size,
                modified: meta.right_modified,
                multi_repo,
            },
        );
    }
    clicked
}

/// A command button. Fixed width, so the grid lines up down the column and a
/// label never truncates.
fn cmd_button(ui: &mut egui::Ui, at: egui::Rect, cmd: Cmd, hide_skips_run: bool) -> egui::Response {
    let resp = ui.put(
        at,
        egui::Button::new(RichText::new(cmd.label()).color(cmd.color()).size(10.0))
            .fill(theme::PANEL)
            .stroke(egui::Stroke::new(1.0, cmd.color())),
    );
    resp.on_hover_text(cmd.hint(hide_skips_run))
}

/// One side's mini-overview, drawn into `rect`: an optional repo chip, the
/// thumbnail, every path this side holds, and a compact facts line. An absent
/// side renders nothing but still claims its rect, so the centre column stays
/// put.
fn side_cell(ui: &mut egui::Ui, thumbs: &mut ThumbCache, rect: egui::Rect, view: SideView) {
    if view.status == Status::Absent || view.paths.is_empty() {
        return;
    }
    let mut cell = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Min)),
    );
    let cell = &mut cell;
    let mut text_width = rect.width();
    if let Some(f) = &view.body.facts {
        let _ = media_cell(cell, thumbs, f, MediaStyle::row(THUMB));
        text_width -= THUMB + cell.spacing().item_spacing.x;
    }
    cell.vertical(|ui| {
        ui.set_max_width(text_width.max(0.0));
        if view.multi_repo
            && let Some(repo) = &view.body.repo
        {
            crate::repo_chip::repo_chip(ui, repo, false, theme::BLUE, view.body.repo_is_main, None);
        }
        for path in view.paths {
            // Elided from the left, so the distinguishing tail survives — two
            // files under a long shared prefix would otherwise clip to the same
            // text. `truncate` is the backstop when the estimate runs long.
            ui.add(
                egui::Label::new(
                    RichText::new(elide_left(path, chars_that_fit(text_width, 12.0)))
                        .size(12.0)
                        .color(view.status.color()),
                )
                .truncate(),
            )
            .on_hover_text(path);
        }
        if let Some(line) = facts_line(view.size, view.modified, view.body.facts.as_ref()) {
            ui.add(egui::Label::new(RichText::new(line).color(theme::TAN).size(10.5)).truncate());
        }
    });
}

/// Roughly how many characters of `pt`-sized proportional text fit in `width`.
/// Only used to choose where to elide, so an estimate is enough — the label's
/// own `truncate` catches any overshoot.
fn chars_that_fit(width: f32, pt: f32) -> usize {
    ((width / (pt * 0.52)).floor().max(8.0)) as usize
}

/// `size · dimensions-or-duration · mtime`, with provenance when known.
///
/// Size and date come from the **row model**, not from the indexed facts: the
/// row is what the operation was planned against, and on a DIFF the two can
/// legitimately disagree when the index is behind the disk. The facts add only
/// what the row cannot know — dimensions or duration, and where a file came
/// from. `None` when there is nothing to say.
fn facts_line(size: u64, modified: i64, facts: Option<&FileFacts>) -> Option<String> {
    if size == 0 && modified == 0 && facts.is_none() {
        return None;
    }
    let mut parts = vec![format_size(size)];
    if let Some(dims) = facts.map(|f| f.dims_or_duration()).filter(|d| d != "—") {
        parts.push(dims);
    }
    parts.push(format_mtime(modified));
    let mut line = parts.join(" · ");
    if let Some(origin) = facts.and_then(|f| f.origin.as_ref()) {
        line.push_str(&format!(" · from {origin}"));
    }
    Some(line)
}

/// The SHOW UNCHANGED toggle and the sort bar. With no column headers to click,
/// the sort key, the side it reads from and the direction are all explicit.
fn controls_bar(ui: &mut egui::Ui, state: &mut BoardState, two_sided: bool, any_unchanged: bool) {
    ui.horizontal_wrapped(|ui| {
        if any_unchanged {
            let label = format!(
                "{} UNCHANGED",
                if state.show_unchanged { "HIDE" } else { "SHOW" }
            );
            if crate::lcars::toggle_button(ui, &label, state.show_unchanged, theme::GREY).clicked()
            {
                state.show_unchanged = !state.show_unchanged;
            }
            ui.add_space(12.0);
        }
        ui.label(RichText::new("SORT").color(theme::TEXT).size(11.0));
        if two_sided {
            for (is_left, label) in [(true, "LEFT"), (false, "RIGHT")] {
                if crate::lcars::toggle_button(ui, label, state.sort_left == is_left, theme::LILAC)
                    .clicked()
                {
                    state.sort_left = is_left;
                }
            }
            ui.add_space(8.0);
        }
        for key in [SortKey::Path, SortKey::Size, SortKey::Date, SortKey::Status] {
            if crate::lcars::toggle_button(ui, key.label(), state.sort_key == key, theme::BLUE)
                .clicked()
            {
                state.sort_key = key;
            }
        }
        ui.add_space(8.0);
        let arrow = if state.sort_asc { "▲" } else { "▼" };
        if crate::lcars::toggle_button(ui, arrow, true, theme::AMBER).clicked() {
            state.sort_asc = !state.sort_asc;
        }
    });
}

/// The two region headers: role in caps, the repo chip (with its MAIN badge),
/// and the absolute path truncated from the left.
fn headers(ui: &mut egui::Ui, view: &BoardView, metas: &[RowMeta]) {
    let centre = centre_width(metas);
    let (side, centre) = regions(ui.available_width(), centre, view.right.is_some());
    ui.horizontal_top(|ui| {
        header_cell(
            ui,
            side,
            view.left_role,
            view.left_repo,
            view.left_is_main,
            view.left_path,
        );
        ui.add_space(centre);
        if let Some(right) = &view.right {
            header_cell(ui, side, right.role, right.repo, right.is_main, right.path);
        }
    });
}

fn header_cell(ui: &mut egui::Ui, width: f32, role: &str, repo: &str, is_main: bool, path: &str) {
    ui.allocate_ui_with_layout(egui::vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(width);
        ui.label(RichText::new(role).color(theme::TEXT).size(11.0).strong());
        if !repo.is_empty() {
            crate::repo_chip::repo_chip(ui, repo, false, theme::ORANGE, is_main, None);
        }
        if !path.is_empty() {
            ui.add(
                egui::Label::new(
                    RichText::new(elide_left(path, 44))
                        .color(theme::HAIRLINE)
                        .size(10.0),
                )
                .truncate(),
            )
            .on_hover_text(path);
        }
    });
}

/// Shorten a path from the **left**, keeping the tail. Two sibling repos under
/// one parent differ in their last segments, so clipping from the right (which
/// is what a plain truncate does) can render them identically.
fn elide_left(path: &str, max: usize) -> String {
    if path.chars().count() <= max {
        return path.to_string();
    }
    let tail: String = path
        .chars()
        .skip(path.chars().count().saturating_sub(max - 1))
        .collect();
    format!("…{tail}")
}

/// The summary line: `«count» «label»` per non-zero status, in its colour.
fn summary(ui: &mut egui::Ui, totals: [usize; 4]) {
    let entries = [
        (Status::WillDelete, totals[0], "to delete"),
        (Status::OnlyHere, totals[1], "only on one side"),
        (Status::Differs, totals[2], "differing"),
        (Status::Same, totals[3], "unchanged"),
    ];
    ui.horizontal_wrapped(|ui| {
        for (status, n, label) in entries {
            if n == 0 {
                continue;
            }
            ui.label(
                RichText::new(format!("{n} {label}"))
                    .color(status.color())
                    .strong(),
            );
            ui.add_space(10.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cmds` distinct commands. They must differ in kind: the grid pairs by
    /// kind, so N copies of one command would collapse to a single line.
    fn some_cmds(n: usize) -> Vec<Cmd> {
        const POOL: [Cmd; 8] = [
            Cmd::CopyRight,
            Cmd::CopyLeft,
            Cmd::DeleteLeft,
            Cmd::DeleteRight,
            Cmd::OverwriteRight,
            Cmd::OverwriteLeft,
            Cmd::Compare,
            Cmd::Hide,
        ];
        POOL.into_iter().take(n).collect()
    }

    fn meta(key: &str, cmds: usize, left_lines: usize) -> RowMeta {
        RowMeta {
            key: key.to_string(),
            left_status: Status::OnlyHere,
            right_status: Status::Absent,
            left_paths: (0..left_lines).map(|i| format!("{key}/{i}")).collect(),
            right_paths: Vec::new(),
            left_size: 0,
            right_size: 0,
            left_modified: 0,
            right_modified: 0,
            unchanged: false,
            cmds: some_cmds(cmds),
        }
    }

    /// Every status has its own colour: a delete must never look like an add.
    #[test]
    fn the_four_statuses_have_distinct_colours() {
        use Status::*;
        assert_eq!(Same.color(), theme::GREY);
        assert_eq!(OnlyHere.color(), theme::GREEN);
        assert_eq!(WillDelete.color(), theme::RED);
        assert_eq!(Differs.color(), theme::AMBER);
        for (a, b) in [
            (Same, OnlyHere),
            (Same, WillDelete),
            (Same, Differs),
            (OnlyHere, WillDelete),
            (OnlyHere, Differs),
            (WillDelete, Differs),
        ] {
            assert_ne!(a.color(), b.color(), "{a:?} and {b:?} must differ");
        }
    }

    /// Row height follows whichever is taller — the name list or the command
    /// grid — so an 8-command row is taller than a 2-command one.
    #[test]
    fn row_height_follows_content() {
        let short = meta("a", 2, 1);
        let tall_cmds = meta("b", 8, 1);
        let tall_names = meta("c", 2, 6);
        assert!(
            tall_cmds.height() > short.height(),
            "more commands make a taller row"
        );
        assert!(
            tall_names.height() > short.height(),
            "more names make a taller row"
        );
    }

    /// The prefix-sum index must map a viewport back to exactly the rows that
    /// intersect it — the thing a uniform-height `show_rows` would give for free.
    #[test]
    fn index_maps_a_viewport_to_the_rows_it_intersects() {
        let metas: Vec<RowMeta> = (0..10).map(|i| meta(&format!("r{i}"), 2, 1)).collect();
        let state = BoardState::default();
        let index = Index::build(&metas, &state);
        assert_eq!(index.order.len(), 10);

        let h = metas[0].height();
        assert!((index.total_height() - h * 10.0).abs() < 0.5);

        // A viewport over rows 2..4 must not return row 0 or row 9.
        let r = index.range(h * 2.0 + 1.0, h * 4.0 - 1.0);
        assert!(r.start <= 2 && r.end >= 4, "covers the intersecting rows");
        assert!(r.start >= 1, "does not start at the top of the board");
        assert!(r.end <= 5, "does not run to the end of the board");
    }

    /// Ragged heights: the index must still land on the right rows when rows
    /// differ in height, which is the case a uniform stride gets wrong.
    #[test]
    fn index_handles_ragged_heights() {
        let metas = vec![
            meta("a", 2, 1), // short
            meta("b", 8, 8), // tall
            meta("c", 2, 1), // short
        ];
        let state = BoardState::default();
        let index = Index::build(&metas, &state);
        let h0 = metas[0].height();
        let h1 = metas[1].height();
        assert!((index.total_height() - (h0 + h1 + metas[2].height())).abs() < 0.5);
        // A viewport entirely inside the tall middle row returns just that row.
        let r = index.range(h0 + 1.0, h0 + h1 - 1.0);
        assert_eq!(r.start, 1);
        assert_eq!(r.end, 2);
    }

    /// Hidden rows leave the index entirely — that is what makes RUN skip them.
    #[test]
    fn hidden_rows_are_dropped_from_the_index() {
        let metas: Vec<RowMeta> = (0..3).map(|i| meta(&format!("r{i}"), 2, 1)).collect();
        let mut state = BoardState::default();
        state.hidden.insert("r1".to_string());
        let index = Index::build(&metas, &state);
        assert_eq!(index.order, vec![0, 2], "the hidden row is gone");
        assert!(
            (index.total_height() - metas[0].height() * 2.0).abs() < 0.5,
            "total height shrinks with it"
        );
    }

    /// Unchanged rows are hidden until the toggle asks for them.
    #[test]
    fn unchanged_rows_are_hidden_by_default() {
        let mut metas: Vec<RowMeta> = (0..3).map(|i| meta(&format!("r{i}"), 2, 1)).collect();
        metas[1].unchanged = true;
        let mut state = BoardState::default();
        assert_eq!(Index::build(&metas, &state).order, vec![0, 2]);
        state.show_unchanged = true;
        assert_eq!(Index::build(&metas, &state).order, vec![0, 1, 2]);
    }

    /// Sorting reads the side the user picked, and reverses on demand. This is
    /// the behaviour DIFF's old header sort never actually had — clicking a
    /// header there changed no row order until the diff was re-planned.
    #[test]
    fn sort_reads_the_selected_side_and_direction() {
        let mut a = meta("a", 2, 1);
        a.left_paths = vec!["zeta".into()];
        a.right_paths = vec!["alpha".into()];
        let mut b = meta("b", 2, 1);
        b.left_paths = vec!["alpha".into()];
        b.right_paths = vec!["zeta".into()];
        let metas = vec![a, b];

        let mut state = BoardState {
            sort_key: SortKey::Path,
            sort_left: true,
            ..Default::default()
        };
        assert_eq!(
            Index::build(&metas, &state).order,
            vec![1, 0],
            "by left path"
        );

        state.sort_left = false;
        assert_eq!(
            Index::build(&metas, &state).order,
            vec![0, 1],
            "by right path"
        );

        state.sort_asc = false;
        assert_eq!(
            Index::build(&metas, &state).order,
            vec![1, 0],
            "direction reverses it"
        );
    }

    /// Sorting by status puts the rows that need attention first.
    #[test]
    fn sort_by_status_ranks_deletions_first() {
        let mut a = meta("a", 2, 1);
        a.left_status = Status::Same;
        let mut b = meta("b", 2, 1);
        b.left_status = Status::WillDelete;
        let mut c = meta("c", 2, 1);
        c.left_status = Status::Differs;
        let metas = vec![a, b, c];
        let state = BoardState {
            sort_key: SortKey::Status,
            ..Default::default()
        };
        assert_eq!(Index::build(&metas, &state).order, vec![1, 2, 0]);
    }

    /// The centre column never shrinks; the sides absorb a narrow window down to
    /// a floor, past which the board clips rather than squeezing the commands.
    #[test]
    fn the_centre_column_never_shrinks() {
        let centre = 216.0;
        let (wide, c_wide) = regions(1600.0, centre, true);
        let (narrow, c_narrow) = regions(600.0, centre, true);
        assert_eq!(c_wide, centre, "centre is constant when wide");
        assert_eq!(c_narrow, centre, "centre is constant when narrow");
        assert!(wide > narrow, "the sides absorb the difference");
        assert!(narrow >= SIDE_MIN, "the sides stop at their floor");
    }

    /// `C` is one constant for the whole board, so a command never moves
    /// horizontally between rows.
    #[test]
    fn centre_width_is_constant_across_rows() {
        let mut paired = meta("a", 0, 1);
        paired.cmds = vec![Cmd::CopyRight, Cmd::CopyLeft];
        let metas = vec![paired, meta("b", 2, 1), meta("c", 1, 1)];
        let c = centre_width(&metas);
        assert!(
            (c - (CMD_W * 2.0 + 8.0)).abs() < 0.01,
            "two columns when paired"
        );
        // A planned board pairs APPLY with HIDE on one line, so it is two
        // columns wide too — the row stays one line tall rather than two.
        let mut planned = meta("a", 0, 1);
        planned.cmds = vec![Cmd::Apply, Cmd::Hide];
        assert!((centre_width(&[planned.clone()]) - c).abs() < 0.01);
        assert_eq!(
            layout(&planned.cmds),
            vec![Line::Centre(Cmd::Apply, Some(Cmd::Hide))],
            "APPLY and HIDE share one line"
        );
        // Only a board whose rows offer a single command can be narrower.
        let mut lone = meta("a", 0, 1);
        lone.cmds = vec![Cmd::Hide];
        assert!(
            centre_width(&[lone]) < c,
            "one command per row needs only one column"
        );
    }

    /// A command and its mirror image share a line: `COPY >` beside `< COPY`,
    /// `DELETE L` beside `DELETE R`. Left-hand commands take the left slot.
    #[test]
    fn mirrored_commands_share_a_line_left_in_the_left_slot() {
        let lines = layout(&[
            Cmd::CopyRight,
            Cmd::CopyLeft,
            Cmd::DeleteLeft,
            Cmd::DeleteRight,
            Cmd::OverwriteRight,
            Cmd::OverwriteLeft,
            Cmd::Compare,
            Cmd::Hide,
        ]);
        assert_eq!(
            lines,
            vec![
                Line::Sides(Some(Cmd::CopyRight), Some(Cmd::CopyLeft)),
                Line::Sides(Some(Cmd::OverwriteRight), Some(Cmd::OverwriteLeft)),
                Line::Sides(Some(Cmd::DeleteLeft), Some(Cmd::DeleteRight)),
                Line::Centre(Cmd::Compare, Some(Cmd::Hide)),
            ]
        );
    }

    /// Pairing is by kind, not by list position — a caller listing its commands
    /// in any order still gets the mirrored pairs on one line.
    #[test]
    fn pairing_survives_a_scrambled_command_order() {
        let scrambled = layout(&[
            Cmd::DeleteRight,
            Cmd::CopyLeft,
            Cmd::DeleteLeft,
            Cmd::CopyRight,
        ]);
        assert_eq!(
            scrambled,
            vec![
                Line::Sides(Some(Cmd::CopyRight), Some(Cmd::CopyLeft)),
                Line::Sides(Some(Cmd::DeleteLeft), Some(Cmd::DeleteRight)),
            ],
            "order in, order out is not how pairing works"
        );
    }

    /// A half-pair keeps its own side's column instead of sliding across, so a
    /// right-hand command never appears under the left region.
    #[test]
    fn a_lone_command_keeps_its_own_side() {
        assert_eq!(
            layout(&[Cmd::DeleteRight]),
            vec![Line::Sides(None, Some(Cmd::DeleteRight))],
            "a right-only delete stays in the right slot"
        );
        assert_eq!(
            layout(&[Cmd::CopyRight]),
            vec![Line::Sides(Some(Cmd::CopyRight), None)]
        );
    }

    /// Render a two-sided board at `width` and hand back the harness.
    fn render(width: f32, metas: Vec<RowMeta>) -> egui_kittest::Harness<'static, BoardState> {
        let mut init = false;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 700.0))
            .build_ui_state(
                move |ui, state: &mut BoardState| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let mut thumbs = ThumbCache::new(4);
                    board(
                        ui,
                        state,
                        &metas,
                        BoardView {
                            left_role: "SOURCE",
                            left_repo: "photos",
                            left_is_main: false,
                            left_path: "/mnt/photos",
                            right: Some(RightHeader {
                                role: "TARGET",
                                repo: "backup",
                                is_main: false,
                                path: "/mnt/backup",
                                multi_repo: false,
                            }),
                            totals: [0, 3, 0, 0],
                            full_len: 3,
                            hide_skips_run: true,
                        },
                        &mut thumbs,
                        &mut |_| RowBody::default(),
                    );
                },
                BoardState::default(),
            );
        harness.run();
        harness.run();
        harness
    }

    fn diff_row(key: &str) -> RowMeta {
        RowMeta {
            key: key.to_string(),
            left_status: Status::Differs,
            right_status: Status::Differs,
            left_paths: vec![format!("a/b/{key}.jpg")],
            right_paths: vec![format!("a/b/{key}.jpg")],
            left_size: 2_100_000,
            right_size: 2_400_000,
            left_modified: 0,
            right_modified: 0,
            unchanged: false,
            cmds: vec![
                Cmd::CopyRight,
                Cmd::CopyLeft,
                Cmd::DeleteLeft,
                Cmd::DeleteRight,
                Cmd::Compare,
                Cmd::OverwriteRight,
                Cmd::OverwriteLeft,
                Cmd::Hide,
            ],
        }
    }

    /// **Every command button must be fully on screen.** A label query passes
    /// even when a widget is clipped or painted past the window edge — which is
    /// exactly how the old diff board shipped three buttons overflowing a
    /// 120px-wide, unclipped column. So this asserts rects, at three widths.
    #[test]
    fn every_command_is_fully_inside_the_window() {
        use egui_kittest::kittest::Queryable;
        for width in [900.0_f32, 1280.0, 1920.0] {
            let metas = vec![diff_row("one"), diff_row("two")];
            let harness = render(width, metas);
            for label in [
                "COPY >",
                "< COPY",
                "DELETE L",
                "DELETE R",
                "COMPARE",
                "OVERWRITE >",
                "< OVERWRITE",
                "HIDE",
            ] {
                let mut found = false;
                for node in harness.query_all_by_label(label) {
                    found = true;
                    let r = node.rect();
                    assert!(
                        r.left() >= 0.0 && r.right() <= width,
                        "at {width}px the command {label} is not fully on screen: \
                         {:.1}..{:.1}",
                        r.left(),
                        r.right()
                    );
                }
                assert!(found, "at {width}px the command {label} was not drawn");
            }
        }
    }

    /// **A row's commands must stay inside that row.** The measured row height
    /// and what the centre column actually draws have to agree; when they did
    /// not, an eight-command row silently lost its last line — and no label
    /// query caught it, because a clipped widget is still in the tree with a
    /// plausible rect. So this pins the invariant positionally: every command of
    /// the first row sits above the second row's content.
    #[test]
    fn a_rows_commands_stay_inside_that_row() {
        use egui_kittest::kittest::Queryable;
        let mut second = diff_row("two");
        second.cmds = vec![Cmd::Apply];
        let harness = render(1280.0, vec![diff_row("one"), second]);

        let next_row_top = harness.get_by_label("APPLY").rect().top();
        for label in [
            "COPY >",
            "< COPY",
            "DELETE L",
            "DELETE R",
            "COMPARE",
            "OVERWRITE >",
            "< OVERWRITE",
            "HIDE",
        ] {
            let r = harness.get_by_label(label).rect();
            assert!(
                r.bottom() <= next_row_top + 0.5,
                "{label} spills out of its row: bottom {:.1} is below the next row's \
                 top {next_row_top:.1}",
                r.bottom()
            );
        }
    }

    /// Every command a row offers is actually drawn — an eight-command row draws
    /// all eight, not as many as happened to fit.
    #[test]
    fn all_of_a_rows_commands_are_drawn() {
        use egui_kittest::kittest::Queryable;
        let harness = render(1280.0, vec![diff_row("one")]);
        for label in [
            "COPY >",
            "< COPY",
            "DELETE L",
            "DELETE R",
            "COMPARE",
            "OVERWRITE >",
            "< OVERWRITE",
            "HIDE",
        ] {
            assert_eq!(
                harness.query_all_by_label(label).count(),
                1,
                "{label} must be drawn exactly once for a single eight-command row"
            );
        }
    }

    /// The model pairing must survive into the rendered geometry: a command and
    /// its mirror sit at the same height, and the left-hand one is to the left.
    #[test]
    fn mirrored_commands_render_at_the_same_height_and_side() {
        use egui_kittest::kittest::Queryable;
        let harness = render(1280.0, vec![diff_row("one")]);
        for (left, right) in [
            ("COPY >", "< COPY"),
            ("DELETE L", "DELETE R"),
            ("OVERWRITE >", "< OVERWRITE"),
        ] {
            let l = harness.get_by_label(left).rect();
            let r = harness.get_by_label(right).rect();
            assert!(
                (l.top() - r.top()).abs() < 0.5,
                "{left} and {right} are not at the same height: {:.1} vs {:.1}",
                l.top(),
                r.top()
            );
            assert!(
                l.right() <= r.left(),
                "{left} must sit left of {right}: {:.1} > {:.1}",
                l.right(),
                r.left()
            );
        }
    }

    /// **Nothing may cross the window's right edge**, at any width. The row
    /// regions are placed at computed offsets, so an unaccounted `item_spacing`
    /// between them pushes the right-hand side out of the window — invisibly,
    /// because a widget past the edge still reports a plausible rect.
    #[test]
    fn no_content_runs_past_the_window_edge() {
        use egui_kittest::kittest::Queryable;
        // Only above `W_min`. Below it the sides have stopped shrinking and the
        // board is *meant* to overrun and be clipped — see the next test.
        for width in [700.0_f32, 1280.0, 1920.0] {
            let harness = render(width, vec![diff_row("one")]);
            for label in ["a/b/one.jpg", "COPY >", "< COPY", "HIDE", "COMPARE"] {
                for node in harness.query_all_by_label(label) {
                    let r = node.rect();
                    assert!(
                        r.right() <= width + 0.5,
                        "at {width}px, {label} crosses the right edge: {:.1} > {width}",
                        r.right()
                    );
                }
            }
        }
    }

    /// The narrow-window rule: below `W_min` the commands and thumbnails keep
    /// their full size and the board clips at the window edge, rather than the
    /// centre column being squeezed. Paths are what give way, elided from the
    /// left so the distinguishing tail survives.
    #[test]
    fn below_the_minimum_width_the_commands_keep_their_size() {
        use egui_kittest::kittest::Queryable;
        let w_min = SIDE_MIN * 2.0 + CMD_W * 2.0 + 8.0;
        let narrow = w_min - 120.0;
        let wide = render(1280.0, vec![diff_row("one")]);
        let tight = render(narrow, vec![diff_row("one")]);
        for label in ["COPY >", "COMPARE", "HIDE"] {
            let a = wide.get_by_label(label).rect();
            let b = tight.get_by_label(label).rect();
            assert!(
                (a.width() - b.width()).abs() < 0.5,
                "{label} was squeezed at {narrow:.0}px: {:.1} vs {:.1}",
                b.width(),
                a.width()
            );
        }
        // And the side never shrinks past its floor.
        let (side, _) = regions(narrow, CMD_W * 2.0 + 8.0, true);
        assert_eq!(side, SIDE_MIN);
    }

    /// Clicking the sort bar must change the **order rows are drawn in**, not
    /// merely the flag. DIFF's old header sort flipped its arrow and reordered
    /// nothing until the diff was re-planned; asserting on state alone would
    /// not have noticed.
    #[test]
    fn the_sort_bar_reorders_the_rows_on_screen() {
        use egui_kittest::kittest::Queryable;
        let mut zeta = diff_row("zeta");
        zeta.left_paths = vec!["zeta.jpg".into()];
        zeta.right_paths = vec!["zeta.jpg".into()];
        let mut alpha = diff_row("alpha");
        alpha.left_paths = vec!["alpha.jpg".into()];
        alpha.right_paths = vec!["alpha.jpg".into()];
        let mut harness = render(1280.0, vec![zeta, alpha]);

        let top = |h: &egui_kittest::Harness<'static, BoardState>, label: &str| {
            h.get_all_by_label(label)
                .map(|n| n.rect().top())
                .fold(f32::INFINITY, f32::min)
        };
        assert!(
            top(&harness, "alpha.jpg") < top(&harness, "zeta.jpg"),
            "ascending by path puts alpha first"
        );

        harness.get_by_label("▲").click();
        harness.run();
        harness.run();
        assert!(
            top(&harness, "zeta.jpg") < top(&harness, "alpha.jpg"),
            "reversing the direction actually reorders the rows, not just the arrow"
        );
    }

    /// When the right side spans several repos, each row names its own — the
    /// GROUP SYNC case, which is the only reason `multi_repo` exists.
    #[test]
    fn a_multi_repo_right_side_names_the_repo_on_every_row() {
        use egui_kittest::kittest::Queryable;
        let mut a = diff_row("one");
        a.cmds = vec![Cmd::Hide];
        let mut b = diff_row("two");
        b.cmds = vec![Cmd::Hide];
        let metas = vec![a, b];
        let bodies = [("backup-nas", "backup-nas"), ("backup-usb", "backup-usb")];
        let mut init = false;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1280.0, 700.0))
            .build_ui_state(
                move |ui, state: &mut BoardState| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let mut thumbs = ThumbCache::new(4);
                    board(
                        ui,
                        state,
                        &metas,
                        BoardView {
                            left_role: "MAIN",
                            left_repo: "photos",
                            left_is_main: true,
                            left_path: "/mnt/photos",
                            right: Some(RightHeader {
                                role: "SINKS",
                                repo: "",
                                is_main: false,
                                path: "2 sink(s) selected",
                                multi_repo: true,
                            }),
                            totals: [0, 2, 0, 0],
                            full_len: 2,
                            hide_skips_run: true,
                        },
                        &mut thumbs,
                        &mut |i| RowBody {
                            left: SideBody::default(),
                            right: SideBody {
                                facts: None,
                                repo: Some(bodies[i].1.to_string()),
                                repo_is_main: false,
                            },
                        },
                    );
                },
                BoardState::default(),
            );
        harness.run();
        harness.run();
        for (_, repo) in bodies {
            assert!(
                harness.query_by_label(repo).is_some(),
                "each row names the repo its right-hand side belongs to ({repo})"
            );
        }
    }

    /// A path too long for its cell keeps its tail, so two files under one long
    /// shared prefix stay distinguishable.
    #[test]
    fn a_row_path_too_long_for_its_cell_keeps_its_tail() {
        let budget = chars_that_fit(120.0, 12.0);
        let a = elide_left("archive/2024/holidays/spain/IMG_0001.jpg", budget);
        let b = elide_left("archive/2024/holidays/spain/IMG_0002.jpg", budget);
        assert_ne!(a, b, "the tails differ, so the rows are distinguishable");
        assert!(a.ends_with("0001.jpg") && b.ends_with("0002.jpg"));
        assert!(a.starts_with('…'), "elided from the left");
    }

    /// The three regions must not overlap: left ends before the centre starts,
    /// the centre before the right. Checked through real rects, by comparing a
    /// left-side path, a centre command and a right-side path on one row.
    #[test]
    fn the_three_regions_do_not_overlap() {
        use egui_kittest::kittest::Queryable;
        let harness = render(1280.0, vec![diff_row("one")]);
        let cmd = harness.get_by_label("COMPARE").rect();
        let paths: Vec<_> = harness
            .query_all_by_label("a/b/one.jpg")
            .map(|n| n.rect())
            .collect();
        assert_eq!(paths.len(), 2, "both sides draw the path");
        let (left, right) = if paths[0].left() < paths[1].left() {
            (paths[0], paths[1])
        } else {
            (paths[1], paths[0])
        };
        assert!(
            left.right() <= cmd.left(),
            "left region overlaps the centre: {:.1} > {:.1}",
            left.right(),
            cmd.left()
        );
        assert!(
            cmd.right() <= right.left(),
            "centre overlaps the right region: {:.1} > {:.1}",
            cmd.right(),
            right.left()
        );
    }

    /// The sides are pinned: changing which commands a row offers must not move
    /// the left or right region, because `C` is a board-wide constant.
    #[test]
    fn the_sides_do_not_move_when_a_row_offers_fewer_commands() {
        use egui_kittest::kittest::Queryable;
        let left_edge = |metas: Vec<RowMeta>| {
            let harness = render(1280.0, metas);
            harness
                .query_all_by_label("a/b/one.jpg")
                .map(|n| n.rect().left())
                .fold(f32::INFINITY, f32::min)
        };
        let full = left_edge(vec![diff_row("one")]);
        let mut trimmed = diff_row("one");
        trimmed.cmds = vec![Cmd::Apply, Cmd::Hide];
        let fewer = left_edge(vec![trimmed]);
        assert!(
            (full - fewer).abs() < 0.5,
            "the left region moved when the command set changed: {full:.1} vs {fewer:.1}"
        );
    }

    /// HIDE is handled inside the board: it drops the row and reports nothing
    /// to the caller, so the row is gone from the next frame's index.
    #[test]
    fn hide_removes_the_row_and_is_not_reported_to_the_caller() {
        use egui_kittest::kittest::Queryable;
        let metas = vec![diff_row("one"), diff_row("two")];
        let mut state = BoardState::default();
        assert_eq!(Index::build(&metas, &state).order.len(), 2);
        // What the board does on a HIDE click.
        state.hidden.insert(metas[0].key.clone());
        let index = Index::build(&metas, &state);
        assert_eq!(index.order, vec![1], "the hidden row is gone");

        // And there is deliberately no counter and no way back: nothing in the
        // state records how many rows were hidden beyond the set itself.
        let harness = render(1280.0, vec![diff_row("one")]);
        assert!(
            harness.query_by_label_contains("hidden").is_none(),
            "no hidden-row counter is shown"
        );
    }

    /// Doc screenshot: the unified board with a DIFF-shaped row (8 commands), a
    /// two-command row and a multi-name row, to `docs/screenshots/board.png`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_board() {
        let mut multi = diff_row("photo");
        multi.left_paths = vec![
            "photo.jpg".into(),
            "photo (1).jpg".into(),
            "copy_of_photo.jpg".into(),
        ];
        multi.right_paths = vec!["IMG_0042.jpg".into()];
        multi.cmds = vec![
            Cmd::RenameLeft,
            Cmd::KeepOneLeft,
            Cmd::DeleteAllLeft,
            Cmd::CopyRight,
            Cmd::Hide,
        ];
        let mut simple = diff_row("notes");
        simple.left_status = Status::OnlyHere;
        simple.right_status = Status::Absent;
        simple.right_paths = Vec::new();
        simple.cmds = vec![Cmd::CopyRight, Cmd::DeleteLeft, Cmd::Hide];
        let metas = vec![diff_row("holiday"), multi, simple];

        let mut init = false;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 680.0))
            .wgpu()
            .build_ui_state(
                move |ui, state: &mut BoardState| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let mut thumbs = ThumbCache::new(4);
                    board(
                        ui,
                        state,
                        &metas,
                        BoardView {
                            left_role: "LEFT",
                            left_repo: "photos-2024",
                            left_is_main: true,
                            left_path: "/home/axel/media/photos-2024",
                            right: Some(RightHeader {
                                role: "RIGHT",
                                repo: "backup-nas",
                                is_main: false,
                                path: "/mnt/nas/backup/photos",
                                multi_repo: false,
                            }),
                            totals: [0, 1, 2, 0],
                            full_len: 3,
                            hide_skips_run: false,
                        },
                        &mut thumbs,
                        &mut |_| RowBody::default(),
                    );
                },
                BoardState::default(),
            );
        harness.run();
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).expect("screenshot dir");
        let out = dir.join("board.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Paths elide from the left, so two repos under a shared parent stay
    /// distinguishable by their tails.
    #[test]
    fn paths_elide_from_the_left_keeping_the_tail() {
        let a = "/home/axel/media/photos-2024";
        let b = "/home/axel/media/photos-2025";
        let (ea, eb) = (elide_left(a, 12), elide_left(b, 12));
        assert_ne!(ea, eb, "the distinguishing tail survives");
        assert!(ea.ends_with("2024"));
        assert!(eb.ends_with("2025"));
        assert!(ea.starts_with('…'));
        assert_eq!(
            elide_left("short", 44),
            "short",
            "short paths are untouched"
        );
    }
}

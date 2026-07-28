# Unified review board + group visibility

Status: ready-for-agent

Spec produced by a grilling session on 2026-07-28. Every decision below was put to
the user and chosen by them; the rationale recorded here is the reason given at the
time, not a reconstruction.

## Context

Two problems, agreed as one effort because they share the repo-naming widget.

**1. "The review looks different everywhere."** There is no single review board. There
are three tables, and the main one changes shape per caller:

| Surface | Widget | Cols | Thumbnails | Actions |
| --- | --- | --- | --- | --- |
| Grooming DEDUPE / PURGE / PRUNE | `review::table` | 3 | source only | `✗` reject, `→` apply (icons, 76 px) |
| Grooming ORGANIZE | `review::table` | 5 | source only (target col blank) | same |
| Transfer COPY / MOVE / SYNC | `review::table` | 5 | one side or both | same |
| Transfer GROUP SYNC | `review::table` | **4 — no ACTIONS column** | source only | none (`ReadOnly`) |
| Transfer DIFF | **`diff_board::board`** | **8** | **none — plain text** | COPY / DELETE / RENAME / COMPARE / OVERWRITE / KEEP 1 / DELETE ALL, split across two 120 px columns at the far left and far right |
| Browse | its own table | 6 | none | none — out of scope, a different kind of view |

On top of the per-caller variation, `egui_extras` discards user-dragged column widths
whenever the column count changes (`egui_extras-0.35.0/src/table.rs:603`), so switching
command silently resets the layout.

**2. A sync group's main is invisible.** `SyncGroup { main: String, sinks: Vec<SyncSink> }`
(`dedup-core/src/store.rs:165-202`) has no per-repo flag, and no marker exists in the GUI.
`is_main` is used only *negatively* — `app.rs:1906` suppresses the MAKE MAIN button.
The de-facto signal that a repo is a main is that `group_section` draws a controls row
under its card. Groups themselves have no visual boundary.

Intended outcome: one board with identical geometry on every surface, and a repo tab
where groups and their mains are unmistakable.

## Bugs this fixes on the way

Found while surveying; all confirmed in source, none previously ticketed except where noted.

- **DIFF sorting does nothing.** `diff_board::sort` is only called when a diff is freshly
  planned (`transfer_view.rs:2505`), and `board()` takes `rows: &[RepoDiffRow]`. Clicking a
  header at `diff_board.rs:311-319` flips the arrow and changes no row order until REVIEW is
  pressed again. No test covers it.
- **DIFF conflict rows overflow.** A left-side `Conflict` lays out COMPARE + OVERWRITE +
  DELETE inside `Column::exact(120.0)` which has **no** `.clip(true)` (`diff_board.rs:262`),
  so the third button bleeds into the neighbouring column. The kittest `get_by_label("COMPARE")`
  calls at `transfer_view.rs:4458/4520/4588` do not catch this — label queries hit the widget
  regardless of clip rect.
- **Board headers never truncate.** `util::sort_header` (`util.rs:72-86`) builds its label with
  no `.truncate()`, and headers are absolute repo paths, so two sibling repos under one parent
  clip to identical-looking text.
- **`ReviewRow.target_path` bakes in the sink** as `format!("{sink}: {rel}")`
  (`transfer_view.rs:284,296`) — already logged at `ai/improvements.md:86-88`. Sorting by
  target path sorts by sink name, and a rel-path containing `": "` is ambiguous.
- **`repo_chip` identicons are corrupted in the SINKS row.** `transfer_view.rs:1609` passes
  `format!("{repo} · {mode}")` as the chip *name*; `identicon()` hashes whatever it is given,
  so the same repo draws a different glyph in SINKS than in SOURCE.
- **`review::RowKind` does not exist.** Two doc links reference it
  (`grooming_view.rs:219`, `transfer_view.rs:720`) and resolve to nothing.

---

## Slice 1 — repo chrome  [DONE 2026-07-28]

Landed as specified, with three deviations worth recording:

- **`Store::main_repo_names()`** was added to dedup-core (mirroring `sink_repo_names()`), because
  every view needed the set and each was open-coding `g.main == name`. Two integration tests in
  `crates/dedup-core/tests/sync_groups.rs`.
- **The filled/outline star distinction is not available.** The vendored Phosphor subset carries
  a single `STAR` codepoint, so the badge is distinguished from the MAKE MAIN action by its
  filled amber tile, not by a different glyph. Verified by rendering.
- **`expanded_mains` was deleted** from `DedupApp`: the LCARS section's own persisted collapse
  state replaces it, and the old `SINK(S) IN '…'` chevron is gone.

A fourth deviation, found in review: the Repositories **card** does not use `repo_chip` at all
(it draws a plain name label), so the badge there is a `main_pill` built from the file's existing
shared `pill` helper rather than the chip badge. Same glyph, same colour, same `"MAIN"` widget
label.

Rendered check: `docs/screenshots/repo_group_section.png`
(`app::ui_tests::doc_screenshot_repo_group_section`, `--ignored`) shows a framed group with a
badged main and a MIRROR sink, next to an ungrouped bare card. Embedded in
`docs/gui/repositories.md`.

### Code review findings, fixed

- **Real layout bug, caught by the Spec axis.** The `MODE: …` label was drawn *outside* the
  response `chip_row` measures, so its width was never budgeted and the SINKS row overran the
  window — 179 px past the edge at 900 px with four sinks. Chip and label are now wrapped
  together and the wrapper's response is returned. Pinned by
  `group_sync_sink_chips_and_modes_stay_inside_the_window`, a geometric assert, since the
  `query_by_label("MODE: MIRROR")` check I had written passes even while the row overflows —
  exactly the trap this spec's Verification section warns about.
- **Three stale doc comments** falsified by deleting `expanded_mains` and the chevron: an
  orphaned line that had re-attached itself to the `browse` field, the `groups` field's
  "collapses a group's sinks" description, and `group_section`'s doubled paragraph describing
  the removed chevron twice.
- **`main_pill` hand-rolled a second pill** five lines below the existing `pill` helper, against
  CLAUDE.md's "don't hand-roll egui widgets". Now built from `pill`.
- **README.md** was missed in the doc sweep; the Repositories bullet now mentions groups.

Two Standards-axis judgement calls **not** actioned, as they are better resolved in slice 2 when
the same structures are already being rewritten: `is_main` travelling beside `read_only` as a
data clump across four structs (`RepoSel`, `SideInfo`, `OverviewSide`, `ColumnHead`), and
`repo_chip`'s two adjacent unnamed booleans (`…, main, lock`) at 13 call sites.

**Both open questions resolved by the user (2026-07-28):**

1. **Section title casing — no change; question withdrawn.** It was posed on a false premise:
   nothing in `CLAUDE.md`, `AGENTS.md` or the docs mandates uppercase, and there is no
   `to_uppercase` anywhere in the GUI. The all-caps look is just what previous authors typed
   into literal strings. The group name renders verbatim, which is correct.
2. **Groups start folded.** Restored to the pre-existing default via
   `section_lcars_collapsible(ui, &name, theme::GREEN, false, …)` — the repo list stays about
   your originals. Three tests and the doc screenshot now expand the section before asserting
   on its contents.

### Original plan

### 1.1 MAIN badge on `repo_chip`

`repo_chip` (`repo_chip.rs:70`) gains a `main: bool` parameter alongside the existing
`lock: Option<bool>` — the same way the padlock was introduced. It draws a **filled**
`icon::STAR` badge as a third grouped item in the chip's horizontal.

The badge appears on **every** surface that names a repo: the Repositories card, Transfer
SOURCE / TARGET / SINKS, Grooming, Browse, Duplicates, the lightbox column heads, and the new
board's headers. All 13 `repo_chip` call sites are updated.

`icon::STAR` currently means the *action* MAKE MAIN (`app.rs:1956`) and appears on the SINK INTO
menu entries (`app.rs:1977`). Accepted: filled star = "is the main" (state), outline star =
"make this the main" (action). One glyph, two jobs, distinguished by fill and context.

While the badge slot exists, fix the SINKS-row identicon corruption: pass the bare repo name
and render the ADD ONLY / MIRROR mode as its own element rather than concatenating it into
the name.

`RepoRow` (`app.rs:51-63`) carries no group field today; group knowledge lives on the app
(`app.rs:209`). Either is fine — the badge needs a bool at the call site, not a new field.

### 1.2 LCARS elbow around groups

One `lcars::section_lcars` per group on the Repositories tab, titled with the group name,
whose rail brackets **the main's card and all its sink cards as a single unit**. Ungrouped
repos stay as bare cards, so a group reads as one framed block.

The section's own caret **replaces** the ad-hoc green `"N SINK(S) IN '{group}'"` chevron
(`app.rs:1316-1326`) — one collapse affordance, not two. ADD REPO / UPDATE ALL / UNGROUP move
into the section body under the main.

`section_lcars` persists open state under an id derived from the title; group names are unique
in the registry, so that is safe. Groups start open.

Touches: `repo_chip.rs`, `app.rs` (`repo_card`, `group_section`, the top-level loop at
`app.rs:1219-1226`), plus the call sites in `transfer_view.rs`, `grooming_view.rs`,
`browse_view.rs`, `dupes_view.rs`, `lightbox.rs`.

---

## Slice 2 — the unified board  [IN PROGRESS — widget built, no surface routed]

**Done (2026-07-28):** `crates/dedup-gui/src/board.rs` — the widget itself, complete against
every decision in §2.1–2.8 and covered by 20 tests including geometric asserts at 900 / 1280 /
1920 px and a rendered doc screenshot (`docs/screenshots/board.png`,
`board::tests::doc_screenshot_board`, `--ignored`).

Built: `Status` (the four-colour vocabulary), `Cmd` (15 commands with labels, colours and
end-user hints), `RowMeta` (the cheap sort/filter/measure model), `RowBody` (the deferred
per-visible-row content), `BoardState` (sort key + side + direction, show-unchanged, hidden
set), `Index` (the prefix-sum virtualisation), the LCARS sort bar, the role + chip + elided-path
headers, the three-region geometry with the narrow-window rule, and the multi-name row shape.

**Two layout bugs found by rendering, both of which the geometric tests had missed:**

1. An eight-command row drew only six commands. `RowMeta::height` and the flow-laid-out grid
   disagreed — measured against `BTN_H = 20` + `CMD_GAP = 5`, egui actually drew 24 px buttons
   with a 7 px gap. **The first geometric test did not catch this**: it asserted horizontal
   containment, and a clipped widget is still in the accessibility tree with a plausible rect.
   Fixed by placing the command grid at **explicit rects** on a `CMD_W × CMD_H` lattice, so
   measured and drawn are the same number by construction. Pinned by
   `a_rows_commands_stay_inside_that_row` (every command of row 0 sits above row 1) and
   `all_of_a_rows_commands_are_drawn`.
2. `ROW_PAD` was applied with `ui.add_space` inside a left-to-right row, which spent it
   sideways; the vertical padding it was supposed to provide never existed. Now an inset on the
   row's content rect.

**Not started — the whole of the routing work:**

- Grooming's two preview builders → `RowMeta` + `RowBody`, and its one `review::table` call site.
- Transfer's four preview builders, GROUP SYNC's lane, and its `review::table` call site.
- DIFF: `RepoDiffRow` → `RowMeta`, the lazy `open_facts`/`facts_for` body resolver (§2.7), the
  `BoardAction` → `Act::Board` mapping, `start_board_action`, `open_inspect`, and re-homing the
  three modals (`ConfirmDeleteAll` / `KeepOne` / `PickName`).
- Renaming `rejected` → `hidden` at the six sites that honour it when RUN builds its skip set
  (`grooming_view.rs:1127-1151,1354-1376` and Transfer's equivalents).
- Deleting `review.rs` and `diff_board.rs` and migrating their tests.
- Fixing the two dangling `review::RowKind` doc links.
- Docs: `README.md`, `CHANGELOG.md`, `ai/improvements.md`, `ai/roadmap.md`, `docs/gui/files.md`.

`board` is declared `pub mod` in `lib.rs` purely so the unrouted module is not dead code —
`AGENTS.md` forbids `#[allow(...)]`. **Make it private again as soon as a view renders it.**

### Original plan

One new widget replaces **both** `review::table` and `diff_board::board`, used by Grooming
(all commands), Transfer COPY / MOVE / SYNC, GROUP SYNC, and DIFF. Same column geometry in all
of them; only the command set differs per surface.

### 2.1 Mechanism — hand-rolled, not `TableBuilder`

`egui_extras` cannot deliver the requirement. It has **no horizontal scroll** at all
(`TableScrollOptions` has `vscroll`, no `hscroll` field), so a narrow window can only clip;
and `.resizable(true)` makes every boundary drag-movable, which is explicitly unwanted.

Layout is computed per frame:

```
|<-- left = (W - C)/2 -->|<-- C -->|<-- right = (W - C)/2 -->|
```

`C` is a constant for the whole board, sized once to fit the widest command set that surface
offers. The left and right regions are pinned to the screen edges and are **not** movable or
resizable.

**Narrow-window rule.** With no horizontal scroll and no resizing, something has to give when
`W` shrinks. The requirement is about *commands*, so the priority is: the centre column and the
row thumbnails never shrink and never clip; **path text is what truncates**, from the left, so
the distinguishing tail survives. Below `W_min = C + 2 × (thumbnail + minimum path stub)` the
sides stop shrinking and the board clips at the window edge rather than deforming. Pin this with
a test at a deliberately narrow width (see Verification 2).

Rows live in an `egui::ScrollArea`. Sorting, striping and virtualization are lost from
`TableBuilder` and must be re-implemented — see 2.3 and 2.6.

### 2.2 Row shape — height follows content

Only the commands that **apply to that row** are rendered, and the row height follows. A
2-command row is short; an 8-command row is tall.

**Command placement mirrors the regions (added 2026-07-28).** The centre grid has a left and a
right slot: a command acting on the left file sits in the left slot, one acting on the right in
the right slot, and a command and its mirror image share a line — `COPY >` beside `< COPY`,
`DELETE L` beside `DELETE R`. A command acting on the row as a whole (`COMPARE`, `APPLY`,
`HIDE`) is centred across both. Pairing is by *kind*, not by position in the caller's list, so a
caller listing its commands in any order still gets the pairs aligned. A half-pair keeps its own
side's column rather than sliding across, so a right-only command never appears under the left
region. Densest board, most rows on screen.

Consequence, accepted: rows are ragged, and virtualization needs a prefix-sum height index
rather than a uniform row height.

> **The one piece of slice 2 with no existing pattern in the repo to copy.** `PREVIEW_CAP` is
> 10 000 rows, and a row's height depends on both its command count and, in BY HASH, its name-list
> length. The prefix-sum index must be rebuilt whenever the sort order or the hidden set changes.
> `ScrollArea::show_rows` cannot be used (it requires uniform height) — this needs
> `show_viewport` plus a binary search into the index. Budget for it explicitly; do not discover
> it mid-pass.

```
| [thumb] a/b/photo.jpg  | [COPY >][< COPY]  | [thumb] a/b/photo.jpg |
|  2.1 MB   2026-03-04   | [DEL L ][DEL R ]  |  2.4 MB   2026-05-11  |
|                        | [COMPARE][OVERWR>]|                       |
|                        | [<OVERWR][HIDE  ] |                       |
| [thumb] a/c/only.png   | [COPY >][DEL L]   | (absent)              |
|  0.9 MB   2026-01-02   | [HIDE  ]          |                       |
```

Per-surface command sets (from `diff_board.rs:503-634`; relations are mutually exclusive by
mode — BY HASH never yields `Conflict`, BY PATH never yields `Renamed`):

- **Grooming / Transfer previews**: `APPLY`, `HIDE`
- **DIFF BY PATH**: `COPY →`, `← COPY`, `DELETE L`, `DELETE R`, `COMPARE`, `OVERWRITE →`, `← OVERWRITE`, `HIDE`
- **DIFF BY HASH**: `COPY →`, `← COPY`, `DELETE L`, `DELETE R`, `RENAME`, `KEEP 1`, `DELETE ALL`, `HIDE`

### 2.3 Sort bar

No column headers means no click-to-sort. A bar sits above the board, built from the existing
`lcars::toggle_button` vocabulary, next to the SHOW/HIDE UNCHANGED toggle and the paging strip
already there:

```
SORT  [LEFT|RIGHT]   [PATH][SIZE][DATE][STATUS]   [^]
```

SIDE is hidden when the surface is one-sided. Keys a surface does not have are not offered.
This bar is also what fixes DIFF sorting, which is broken today.

### 2.4 Headers

Each side is headed by its **role** in LCARS caps, then a `repo_chip` (identicon + name +
MAIN badge from slice 1), then the absolute path in small dim text **truncated from the left**
so the distinguishing tail survives rather than the shared parent.

```
  SOURCE                          TARGET
  (o) photos-2024                 (o) backup-nas
  ...ers/axel/media/photos-2024   ...mnt/nas/backup/photos

  MAIN                            SINKS
  (o) photos-2024 [*]             3 selected
```

Roles: SOURCE / MAIN / TARGET / SINKS / LEFT / RIGHT depending on surface.

### 2.5 Colour vocabulary

One vocabulary spanning both meanings the board now serves — prescriptive
("RUN will delete this") on Grooming/Transfer, descriptive ("these two differ") on DIFF:

| Colour | Meaning |
| --- | --- |
| GREY | same on both sides / unchanged / equal |
| GREEN | exists only on this side; in a plan, will be added |
| RED | will be deleted by RUN |
| AMBER (TAN) | same path different content (conflict), or same content different name (renamed) |

Mapping: `review::SideStatus` Added→GREEN, Removed→RED, Unchanged→GREY, Absent→(cell empty).
`DiffRelation` OnlyLeft/OnlyRight→GREEN, Conflict/Renamed→AMBER, Equal→GREY. A surface that
cannot produce a state simply never shows it. Keeps DELETE visually distinct from ADD.

### 2.6 Mini-overview cells

`media_cell` at `MediaStyle::row(48.0)` plus path, size and date — as `review::side_cell` does
today (`review.rs:513-516`), which is already the shape wanted.

**Multi-name rows (BY HASH).** One row per content pair. Each side draws a **single**
thumbnail — every name on a side shares the same `(size, hash)`, so the content is identical
and one thumbnail is honest — with all its names listed beside it and the size/date line once.
The row grows with the longer name list. This keeps `KEEP 1` and `DELETE ALL` meaningful:
they act on the side's whole name list, which is visible in the same row.

```
| [thumb] photo.jpg          | [RENAME ][KEEP 1 ] | [thumb] IMG_0042.jpg |
|         photo (1).jpg      | [DEL ALL][COPY > ] |         2.1 MB       |
|         copy_of_photo.jpg  | [< COPY ][DEL R  ] |         2026-05-11   |
|         2.1 MB  2026-03-04 | [HIDE   ]          |                      |
```

**Per-row repo chip.** When the right side is multi-repo (only GROUP SYNC today), a
`repo_chip` sits at the top of the right mini-overview. No fourth column — the three-region
geometry is unchanged. `ReviewRow` gains a real repo field and `target_path` becomes a clean
rel_path, which lands the `ai/improvements.md:86-88` backlog item.

### 2.7 DIFF facts — lazy lookup, no dedup-core change

`ai/roadmap.md:198-203` claims thumbnails in DIFF require `RepoDiffRow`/`DiffFile` to carry
`FileFacts`, "a separate, larger change" touching `plan_repo_diff` and its dedup-core tests.
**That is not necessary.** `DiffFile` stays `{rel_path, size, modified_ms}`.

```rust
let (ldb, lbase) = media_cell::open_facts(store, left_repo);   // once per frame
let (rdb, rbase) = media_cell::open_facts(store, right_repo);
// per visible row:
let lf = media_cell::facts_for(ldb.as_deref(), lbase.as_deref(), &f.rel_path);
```

This is the pattern the review callers already use, and the one DIFF's own `open_inspect`
already uses to build the COMPARE lightbox (`transfer_view.rs:2527-2536`). Rows are
virtualized, so it is bounded to roughly 20 lookups per frame — cacheable per rel_path if it
ever appears in a profile. `dedup-core` is untouched. Update `ai/roadmap.md` to record this.

### 2.8 HIDE

**One command, meaning varies by surface.** On planned surfaces (Grooming, Transfer) HIDE both
removes the row from the board and excludes it from RUN. On DIFF, which executes immediately
and has no RUN, it only removes the row from view.

`ReviewState.rejected` becomes a `hidden` set; the callers that honour it when RUNning follow
the same field. `RowControls::{Enabled,ReadOnly}` and the `→` apply / `✗` reject icon pair are
replaced by the named APPLY and HIDE commands in the centre column.

**This deliberately removes one capability.** Today `✗` excludes a row from RUN *while leaving it
on the board*, struck through, so you can see what you excluded and toggle it back. HIDE does not
have that middle state — a row is either present and included, or gone. APPLY (run this one row
now) survives unchanged. This is the direct consequence of choosing one command over two and is
recorded, not overlooked.

> **Accepted trade-off, raised twice and confirmed by the user.** A hidden row is gone: no
> counter in the summary line, no SHOW HIDDEN toggle, no un-hide. The board can therefore show
> N rows while RUN acts on fewer, with nothing on screen indicating the difference; recovering
> from a misclick means pressing REVIEW and rebuilding the preview, losing every other hide.
> Implement exactly this — do not add a counter or recovery affordance on your own initiative.

### 2.9 Retirement

Both old widgets go in this same pass: `review::table` and `diff_board::board` are deleted once
nothing calls them, and their tests are migrated to the new board. `browse_view`'s table is a
different kind of view and is **out of scope**.

Fix the two dangling `review::RowKind` doc links while in the file.

---

## Sequencing

Two slices, by the user's choice.

1. **Chrome** — `repo_chip` MAIN badge + LCARS group elbow. Small, self-contained.
2. **Board** — the whole rework in one pass: new widget, all five surfaces routed, both old
   widgets deleted, tests migrated. Roughly 2000 lines across `review.rs`, `diff_board.rs`,
   `transfer_view.rs`, `grooming_view.rs`.

Slice 2 is deliberately a single large change; there is no intermediate state where two boards
coexist.

## Verification

Per `AGENTS.md` and the `ai/roadmap.md` §1 lesson — *"a label-query-only kittest test does not
catch layout/overlap bugs — render it"*:

1. `cargo fmt --check` clean; `cargo clippy -- -D warnings` clean; `cargo test` green.
2. **Geometric kittest asserts** (not label queries) on the new board: the three regions do not
   overlap; every command button's rect is fully inside the centre region at 900 px, 1280 px and
   1920 px window widths; the left and right region rects are unchanged when the surface changes
   command. A label query would pass even when a button is clipped — that is exactly the
   `COMPARE`/`OVERWRITE`/`DELETE` overflow bug that shipped. Add a deliberately narrow width
   (below `W_min`) asserting the narrow-window rule of §2.1: commands and thumbnails still fully
   visible, path text truncated.
3. **Render tests** (`#[ignore]`d snapshots, `UPDATE_SNAPSHOTS=1` to regenerate) for: a DIFF
   BY PATH conflict row, a DIFF BY HASH multi-name row, a Grooming one-sided preview, and a
   GROUP SYNC preview with per-row sink chips.
4. **Sort bar test** proving row order actually changes on click — the thing DIFF's header sort
   never did.
5. **A doc screenshot of the Repositories tab** showing an ungrouped repo, a group inside its
   elbow, and the MAIN badge, checked visually.
6. Docs in the same change, not a follow-up: `README.md`, `CHANGELOG.md`, `ai/improvements.md`
   (strike the now-wrong `diff_board`/`FileFacts` note), `ai/roadmap.md` (§2 checkbox state is
   stale in both directions — reconcile it), `docs/gui/files.md`.

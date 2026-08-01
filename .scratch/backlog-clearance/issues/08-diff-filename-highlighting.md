# 08 — Highlight the differing characters between two near-identical filenames

Status: resolved
Spec: ../spec.md

## Problem

Reported from real use: "the diff in the name could be more highlighted, like with a blue
background for characters that differ from the other side. I assume there is some smart
algorithm that prevents a 1:1 comparison and highlighting everything after an additional
character?"

In a DIFF BY HASH row, two names for identical content sit side by side with no indication of
where they actually differ. The reporter anticipated the trap themselves: a naive
position-by-position comparison marks everything after an inserted character as different,
which is noise rather than signal.

## Approach

Compute a **common-subsequence-based** difference between the two names — not a positional
comparison — so that inserting one character highlights that character, not the entire
remainder. Highlight the differing runs on each side.

Constraints:

- **Names are the unit, not paths.** Highlight within the filename; a shared parent directory
  is not a difference worth painting.
- **Multi-name rows.** A BY HASH row can carry several names on a side. Define and document
  what is compared when either side has more than one name — comparing each side's *first*
  name is a defensible rule; whatever is chosen, state it and test it.
- **Colour comes from the theme**, consistent with the board's existing vocabulary. Do not
  introduce a new colour constant if an existing one carries the right meaning.
- Highlighting must not change row height or push the row's commands out of the centre region.

## Seam and tests

GUI seam — inline `ui_tests` in the board or the Transfer view, prior art the existing board
tests:

- an inserted character highlights that character only, not the rest of the name — this is the
  specific failure the reporter predicted, so assert it directly
- a changed extension highlights the extension
- two identical names highlight nothing
- the documented multi-name rule holds
- **geometric assertion**: a highlighted row's commands stay inside the centre region, and row
  height is unchanged versus the same row unhighlighted

Render check: a `doc_screenshot_` for a BY HASH row with highlighting, looked at.

## Done

Standing gate green. `CHANGELOG.md`, `ai/improvements.md`, and the GUI documentation page for
the Transfer tab updated.

## Comments

**Implemented 2026-07-31.** Gate green: fmt clean, clippy 0 warnings, `cargo test --workspace`
24 suites / 0 failures.

`name_diff_ranges(a, b)` returns the byte ranges of `a` absent from the longest common
subsequence with `b`; adjacent ranges merge so a run is one highlight rather than several.
`highlight_job` turns those into a `LayoutJob` with a highlighted background per run.

Decisions the ticket left open:

- **Names, not paths.** `file_name_at` splits the basename off first, so a differing parent
  directory is never painted. Pinned by `diffing_is_over_the_file_name_not_the_directory`.
- **Multi-name rows**: a side's names are paired against the other side's **in order**, and a
  name with no counterpart is rendered plain rather than compared against an unrelated one.
- **Colour** comes from `visuals().selection.bg_fill`, not a new constant.
- **Elision interaction**, which the ticket did not anticipate: ranges are computed against the
  real name, but the label may be elided from the left, which shifts every offset. Painting
  then would highlight the wrong characters, so an elided label falls back to plain text.
- A 512-character guard keeps the quadratic LCS table off pathological paths.

Six algorithm tests plus `highlighting_does_not_change_row_height` (paint must not desync the
prefix-sum index) and `a_name_without_a_counterpart_is_not_painted`.

**Verified by rendering, not just asserting** — the standing lesson in this repo. The doc
screenshot gained a rename pair so the feature is actually visible, and
`docs/screenshots/board.png` was regenerated and looked at: `a/b/holiday_v2.jpg` highlights
`_v2` only (the shared stem and `.jpg` stay plain — exactly the insertion trap the reporter
predicted), `photo.jpg` vs `IMG_0042.jpg` highlights both differing stems, and the counterpart
row shows no highlight because the insertion exists on one side only.

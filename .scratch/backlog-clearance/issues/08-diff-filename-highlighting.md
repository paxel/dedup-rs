# 08 — Highlight the differing characters between two near-identical filenames

Status: ready-for-agent
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

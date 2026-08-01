# 01 — The shell: permanent tab row, per-side titles, per-side switchers

Status: resolved
Spec: ../spec.md

## Problem

Comparing steps out of the tabbed screen onto a different one with a different set of controls.
That single fact produces three separate complaints: metadata becomes unreachable mid-comparison,
the CLOSE button changes colour and the tab button shifts and loses its icon, and the repository
badges migrate from beside their image into a distant bar.

Separately, the switcher walks one side through every candidate *including the file the other
side shows*, so a two-file group offers positions 1/2 and 2/2 and the second compares a file
with itself.

## Approach

Build the surviving viewer's shell, before any capability moves into it.

- **The tab row is always present.** Comparing is a mode of the current tab, not a departure
  from the tabs. Nothing about the row's shape may depend on whether two files are shown.
- **Each side gets a title** carrying path (truncatable), size, dimensions, date and repository,
  plus that side's delete control. The bottom legend goes; this reuses its space.
- **A switcher per side**, over a caller-supplied pool. A switcher **skips** the file the other
  side shows, so a side's positions have a hole in them; the counter is a position in the pool.
  With a pool of two or fewer, **no switcher is rendered at all**.
- **Hiding the second side** gives the first the whole screen.
- Fix while here: the overlay must fully cover what is behind it; controls must share a
  baseline rather than drifting downward toward the right; nothing may clip at narrow widths.

Actions stay caller-supplied — the viewer offers the controls it is given and reports what was
chosen.

## Seam and tests

The viewer's own inline tests, driven with a pair and a pool — the one seam that serves all four
callers:

- the tab row is present and identical whether one or two files are shown
- a side's title carries its own path, size, dimensions, date and repository
- a pool of two renders **no** switcher — the defect that started this, made structurally
  impossible
- a pool of five renders a switcher per side, and stepping one side never lands on the file the
  other shows
- hiding the second side leaves the first occupying the full area

Geometric assertions, because a label query passes on a clipped widget: every control's
rectangle sits inside the window at narrow and wide widths, and the controls in a bar share a
baseline.

Render check: a screenshot showing the overlay fully covering the tab beneath, looked at.

## Done

Standing gate green. No user-visible change yet beyond the shell — capabilities arrive in later
tickets — so `CHANGELOG.md` waits.

## Comments

**Partly implemented 2026-08-01, TDD. Status stays `ready-for-agent` — the switcher is done,
the rest of the shell is not.** Gate green: fmt clean, clippy 0 warnings, `cargo test
--workspace` 24 suites / 0 failures.

**Done: the switcher model, which is the defect that started the redesign.**

Five cycles, all RED→GREEN:

1. Stepping a side moves it to the next candidate — introduces the pool and `new_with_pool`.
2. Stepping **skips** the file the other side shows — screenshot 131750's bug, asserted
   directly, plus `assert_ne!` that the two sides can never be the same file.
3. A pair offers no stepping (`can_step_*` false); three candidates offer it on both sides.
4. Three candidates render a switcher per side.
5. Clicking NEXT actually moves that side, and only that side.

**Two discipline notes, both mine:**

- Cycle 1's first test asserted *two* behaviours — stepping and skipping — so it failed on the
  second while the first was correct. Corrected the test to cover one behaviour and left
  skipping to cycle 2, rather than implementing ahead of the test.
- Cycle 4 passed while the control was **inert**: it asserted the switcher was *present*, not
  that it did anything. `step` was collected and never applied. Cycle 5 caught it. Presence
  assertions are worth little on their own — the same trap as a label query on a clipped
  widget.

`DiffSide` gained `Clone` (the pool holds candidates); the deferred `step` is applied after the
drawing closure, since the closures borrow both sides.

**First caller adopted:** the DIFF row now opens with the pair as its pool, so neither side
renders a switcher — the spec's "a review row offers one or two, so switching is never
necessary", demonstrated rather than asserted. This also keeps clippy clean without an
`#[allow]`, which `AGENTS.md` forbids.

**Completed in a second pass**, cycles 6–8:

6. Each side's facts moved into its **own title above its image** — `side_strip` was already
   rendering repo, path, size, dimensions and date; it just sat at the bottom as a shared
   legend. Moving it above the panes also gave the images the 150px the legend occupied.
   Asserted geometrically: A's title is left of B's and both sit in the top half.
7. **Hiding the second side**, with a HIDE B / SHOW B control. Asserted that the second side is
   gone entirely rather than merely narrowed.
8. **Layout fixes.** The overlay was `from_black_alpha(252)` and let the Duplicates tab read
   through the photographs; it is now fully opaque. A geometric test pins that tab-row controls
   share a baseline and stay inside a 900px window.

**A third discipline note.** The baseline test first failed claiming "Image drifts off CLOSE's
baseline" — but CLOSE sits in the title row and the tabs in the row below, so they are
different rows *by design*. The assertion was wrong, not the layout. Corrected to compare tabs
against each other. A geometric test that encodes a wrong expectation is worse than none: it
would have driven a "fix" that broke a correct layout.

Also found: an image pair offers no Text tab yet, because `tab_kinds` still restricts text to
non-media files. Ticket `02` changes that; the baseline test uses the tabs that exist today.

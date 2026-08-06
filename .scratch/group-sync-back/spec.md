# GROUP SYNC BACK — pull a sink's changes back to its main

Status: ready-for-agent

Spec produced by a grilling session on 2026-08-06. Every decision below was put to
the user and chosen by them; the rationale is the reason given at the time.

## Problem Statement

I keep my main repository backed up to one or more sinks with GROUP SYNC — but
that only ever pushes the **main → sinks**. Two things then have no home:

- I sometimes drop new files **straight onto a backup drive** (a sink), and there
  is no way to promote them back into the main.
- I sometimes **delete files from the main by mistake** and want to recreate them
  from a backup that still has them.

Today a sink is not even selectable in the Transfer tab — sinks are deliberately
hidden ("managed through their main"), so I can't DIFF a sink against its main or
copy anything out of it. My only recourse is to leave the app and copy files by
hand, guessing which sink files are genuinely new and which I actually deleted on
purpose.

## Solution

A new **GROUP SYNC BACK** command in Transfer — the mirror image of GROUP SYNC —
offered exactly when the **source is a group's main**. I never select a sink
directly; I select the main and choose which of its sink(s) to pull from. It lands
me in the **normal review board**, showing the sink's files classified against the
main:

- **New** — content the main never had (my direct edits): **pre-selected to
  promote** into the main.
- **Resurrection** — content the main *deleted* (it holds a tombstone) that the
  sink still has: shown **marked blue and left unselected**, so I can tick the ones
  I deleted by mistake and recreate them, while the ones I deleted on purpose stay
  untouched.
- **Equal** — the main already has it: hidden by default.

I review and run it like any other board. Nothing is resurrected automatically
(that would silently undo a dedup pass) and nothing is silently skipped (that would
block mistake-recovery) — the board reveals both buckets and I decide.

## User Stories

1. As someone who backs up a main to sinks, I want to pull a sink's changes back
   into its main, so that files I only have on a backup are not stranded there.
2. As someone who dropped new photos straight onto a backup drive, I want those
   promoted into the main, so that the main becomes complete without me copying by
   hand.
3. As someone who deleted files from the main by mistake, I want to recreate them
   from a sink that still has them, so that I can recover without a full restore.
4. As a user, I want GROUP SYNC BACK offered right where GROUP SYNC is — when the
   source is a group's main — so that the reverse of a push is discoverable beside
   the push.
5. As a user, I want to pick which sink(s) to pull from, so that I can reconcile
   the one drive I actually edited.
6. As a user, I never want to select a sink as a raw source/target, so that the
   "sinks are managed through their main" model stays intact.
7. As a user, I want the pull to land in the same review board I already know, so
   that I don't learn a new surface for it.
8. As a user, I want files that are genuinely new on the sink pre-selected to
   promote, so that the common case is one glance and RUN.
9. As a user, I want files the main deleted but the sink still holds shown but
   **not** pre-selected, so that a pull never silently reverses a deletion.
10. As a user, I want those resurrection candidates **marked in a distinct colour
    (blue)**, so that I can see at a glance which rows would bring back deleted
    content.
11. As a user, I want to tick individual resurrection rows, so that I can recover
    exactly the files I deleted by mistake and leave deliberate deletions alone.
12. As a user, I want equal files hidden by default, so that the board shows only
    what would actually change.
13. As a user, I want to **filter the board by resurrection**, so that I can
    isolate and scan just the "would bring back deleted content" rows.
14. As a user, I want the pull to compare by **content**, not path, so that a file
    renamed on the sink is still recognised as the same content the main has.
15. As a user, I want a promoted file whose path collides with a *different* main
    file to offer OVERWRITE / RENAME, so that a name clash is a visible choice, not
    a silent overwrite.
16. As a user, I want running the pull to copy the checked content into the main
    and index it, so that the main immediately knows the promoted files.
17. As a user, I want the run to be best-effort and report successes and failures
    separately, so that one unreadable file does not abort the whole pull.
18. As a careful user, I understand that a **Mirror** sink deletes my direct-edits
    on the next push, so I want to run GROUP SYNC BACK *before* pushing — and I do
    not want the app to second-guess that sequencing.
19. As a user, I never want the machine to decide which deletions were mistakes,
    because only I know that — so resurrection must always be my explicit choice.
20. As a developer, I want the new/equal/resurrection classification to be pure and
    unit-testable in the core, so that the semantics are pinned without a GUI.
21. As a developer, I want the five review-board colours to stay pairwise-distinct
    in both palettes, so that "resurrection" can never be mistaken for "will-delete"
    or "only-here".

## Implementation Decisions

- **A new Transfer command, `GROUP SYNC BACK`,** offered under the same condition as
  GROUP SYNC — when the selected source repo is a sync group's **main** (the group
  whose `main` equals the source). Its sink selection mirrors GROUP SYNC's: the
  group's sinks, chosen by the user. Sinks remain **not** selectable as a top-level
  source or target; you always start from the main and act on its group.
- **The classification reuses the existing core diff.** `diff_print(source = sink,
  reference = main)` already emits `New / Equal / DeletedInReference`.
  **Resurrection is exactly `DeletedInReference`** — the main holds a tombstone (a
  `missing = true` index entry) for content the sink still has. No new
  classification is invented; the reverse plan runs the existing one with the sink
  as source and the main as reference. Content identity is size + BLAKE3, as
  everywhere — paths never decide equality.
- **The review board gains a fifth row semantic, `resurrection`, coloured blue.**
  New rows are **pre-checked** to promote (copy sink → main); resurrection rows are
  shown **unchecked and blue**; equal rows are hidden by default. The board's row
  model carries the resurrection marking, derived from the core classification's
  `DeletedInReference` — a "sink-only" row is *new* (promote/green) when the main
  never saw the content and *resurrection* (blue) when the main tombstoned it.
- **Path collisions reuse the board.** A promoted file whose relative path collides
  with a *different* main file uses the board's existing per-row **OVERWRITE /
  RENAME** — no new conflict/merge logic is added.
- **The board can filter by resurrection** — a board-level facet that isolates the
  blue rows, alongside the existing show/hide of equal and unchanged rows.
- **`theme` gains blue as a review semantic.** The invariant that the review-board
  semantics stay **pairwise-distinct in both palettes** — and the perceptual-distance
  test that enforces it — extend from **four** (grey unchanged, green only-here, red
  will-delete, amber differs) to **five** (adding blue resurrection), tuned distinct
  in light *and* dark. `theme::blue()` already carries both-palette values; this is a
  distinctness/tuning check, not new machinery. Note blue is currently used as a
  non-review *accent* elsewhere (compare A-side, filter labels, an "identical"
  label) — different surfaces from the review board.
- **No auto-resurrect, no auto-skip.** The machine reveals the buckets and marks
  resurrections; the user decides. This is forced by the domain: a tombstone cannot
  distinguish a *mistaken* deletion from a *deliberate* dedup deletion, so resolving
  it automatically in either direction would be wrong for one of the two real use
  cases.
- **The run** promotes the checked rows — copies the content into the main and
  indexes it — best-effort, counting successes and failures, behind a confirmation,
  mirroring GROUP SYNC's batch-run shape (not a silent apply).
- **The Mirror interaction is intended and unguarded.** A Mirror sink deletes
  direct-edits on the next main→sink push (the main lacks them); GROUP SYNC BACK
  must be run first. The app does **not** warn or reorder — the user sequences it.

## Testing Decisions

A good test asserts **external behaviour** — which bucket each file lands in, what
the board pre-checks, and the **resulting main index state** after a run — not the
internal plan shape. Seams, highest first; all already exist in the codebase:

1. **Core `diff` classification (the one seam where the semantics live).** With
   temp repos, assert `diff_print(sink, main)` classifies: content the main never
   saw as `New`; content present on both as `Equal`; and content the main tombstoned
   (`missing = true`) that the sink still holds as `DeletedInReference`
   (resurrection). This is pure and GUI-free. Prior art:
   `crates/dedup-core/tests/repo_diff.rs`, `diff_ops.rs`, `sync_groups.rs`.
2. **GUI command + board classification/marking/filter (transfer_view kittest).**
   Assert GROUP SYNC BACK is offered only when the source is a group's main; the
   board pre-checks `New` rows and leaves `resurrection` rows unchecked; the
   resurrection filter isolates the blue rows; and a run promotes exactly the checked
   rows and the main index then knows them. Follow the repo's rule — a
   label-query-only test does not catch a colour/overlap bug — with a geometric
   assert and an `#[ignore]`d render test showing a blue resurrection row distinct
   from green/red/amber. Prior art: the transfer_view DIFF / GROUP SYNC kittest
   tests and the `doc_screenshot_*` render tests.
3. **`theme` distinctness.** Extend the existing pairwise-distinct perceptual-distance
   test from four semantics to five (blue included), in both palettes. Prior art:
   the four-semantic distinctness test already in `theme`.

## Out of Scope

- **Whole-repo recovery** — rebuilding a lost or empty main from a sink. This is
  file-level reconcile (additive promote + opt-in resurrect), not disaster recovery.
- **Two-way / continuous sync.** The pull is a deliberate, reviewed, one-direction
  operation, not an ongoing bidirectional reconcile.
- **Automatic conflict resolution / merge.** Path collisions reuse the board's
  existing OVERWRITE / RENAME; no content-merge is added.
- **A Mirror-trap warning.** The push-deletes-edits behaviour is intended; the user
  sequences GROUP SYNC BACK before the next push.
- **Making sinks generally selectable in Transfer.** They remain managed through
  their main; GROUP SYNC BACK is the only new sink→main path.

## Further Notes

- This is the first **sink → main** flow. It relaxes "sinks are operated on only
  through their main" only in the controlled sense that you still *start from the
  main* and act on its group — a sink never becomes a free-standing source.
- The governing safety insight, and why the review is non-negotiable: a tombstone
  cannot tell a mistaken deletion from a deliberate one, so resurrection is never
  automatic and never suppressed — it is revealed, marked blue, and chosen by the
  user. This is the same principle the compare viewer follows: reveal what is not
  equal; the machine does not decide.
- **"Resurrection"** is the new domain term — restoring, from a sink, content the
  main deleted (tombstoned) but the sink still holds.

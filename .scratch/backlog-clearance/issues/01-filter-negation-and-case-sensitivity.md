# 01 — Filter negation (`!pattern`) and case-sensitivity toggle

Status: resolved
Spec: ../spec.md

## Problem

A filter can only express what to include. There is no way to say "everything that is *not*
an MP3" — the user must enumerate every format they do want. Separately, matching is
implicitly case-sensitive with no way to change it, which misbehaves on drives that mix
`.JPG` and `.jpg`.

Verified: the core filter module contains no negation handling and no case-sensitivity
option; the GUI filter builder offers neither control.

## Approach

Extend the core file filter with:

- **Negation.** A leading `!` on a pattern inverts that pattern's match — `!*.mp3`,
  `!/cache/`. Decide and document the semantics when negated and plain patterns are mixed in
  one filter; the natural reading is that a file must match at least one plain pattern (if any
  are present) and no negated pattern. State the chosen rule in the module documentation.
- **Case-sensitivity.** A `case_sensitive` option on the filter, defaulting to the current
  behaviour so existing callers do not change meaning.

Then surface both in the GUI filter builder: a negation affordance and an `Aa` toggle. The
builder is the shared filter widget used by every tab — extend it there, never build a
per-tab variant.

Escaping matters: a filename can legitimately begin with `!`. Provide a way to express a
literal leading `!` and cover it with a test.

## Seam and tests

Core seam — `crates/dedup-core/tests/`, prior art `diff_ops.rs` / `update_repo.rs`:

- a negated pattern excludes matching files and admits everything else
- a plain and a negated pattern combined behave per the documented rule
- case-insensitive matching admits `.JPG` for `*.jpg`; case-sensitive does not
- a literal leading `!` can be expressed and matches a file whose name starts with `!`
- the default remains exactly the pre-change behaviour

GUI seam — inline `ui_tests` in the filter builder: toggling each control produces the
expected filter value.

## Done

Standing gate green (`cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`).
Documentation updated in the same change: `README.md` filter syntax, `CHANGELOG.md`,
`ai/improvements.md` item struck.

## Comments

**Implemented 2026-07-31.** Gate green: `cargo fmt --check` clean, `cargo clippy --workspace
--all-targets` clean, `cargo test --workspace` 24 suites / 0 failures.

Two decisions the ticket left open, resolved during implementation:

1. **Negation is per *condition*, not per pattern.** The ticket's examples (`!*.mp3`) had no
   facet prefix, but the grammar has no bare patterns — every value follows `name:` / `mime:`
   / etc. So `!` attaches to the whole condition: `!name:*.mp3`, `!mime:image`. This is
   uniform across all seven facets instead of needing per-facet value parsing, and it made
   the escaping question disappear: `!` is only recognised at a condition boundary, so
   `name:!important` is literal text and no escape syntax was needed. `split_groups` had to
   learn that `!` opens a group, otherwise `mime:image !name:x` collapsed into a single
   `Mime("image !name:x")`.
2. **Mixing rule** — conditions still combine with AND, so plain-and-negated reads as "matches
   every plain condition and none of the negated ones". Documented in the module header and
   pinned by `negated_and_plain_conditions_combine_with_and`.

**Case mode is a whole-expression modifier**, not a per-condition flag: `case:insensitive`
parses to a `NoCase` wrapper and the flag rides the match traversal (`matches_inner`'s `ci`
parameter) rather than living in every leaf's data. That keeps `FileFilter`'s existing
variants and their public signatures unchanged — no caller outside the module needed touching
— and means it correctly reaches inside a negation. Size and date facets ignore it by
construction.

New variants: `FileFilter::Not(Box<_>)` and `FileFilter::NoCase(Box<_>)`. `uses_annotations`
recurses through both, so a negated `tag:` condition still declares that it reads the
annotations table — a caller skipping that load would have silently mismatched.

Tests: 9 new core tests (22 total in the module), 8 new GUI tests (11 total), including an
expression round-trip through `filter_string` → `set_expression` covering all four new syntax
forms. `SavedCond.negated` is `#[serde(default)]`, so presets saved before this change still
load.

**Correction, same day.** The first pass claimed this ticket done while missing its own GUI
seam requirement: the five GUI tests all set `fb.filters` / `fb.case_insensitive` directly, so
they proved the *grammar* round-trips but never rendered or clicked the two controls actually
added. Nothing dispatched `Act::ToggleNegate` or `Act::ToggleCase` — the buttons could have
been unwired and every test would still have passed. Three tests added that drive a real
`FilterBuilder` through `ui()` in a headless kittest harness:

- `clicking_not_negates_that_condition` — clicks NOT on an editing condition, asserts the
  expression becomes `!name:*.mp3`.
- `clicking_aa_switches_to_case_insensitive` — clicks `Aa`, asserts the `case:insensitive`
  prefix appears.
- `the_filter_bar_stays_inside_a_narrow_window` — geometric assert at 560 px, because `Aa`
  was added into the same `horizontal_wrapped` row as the condition chips and a label query
  passes even when a widget is clipped (this repo's standing lesson).

Two smaller corrections in the same pass: the `#[allow(clippy::too_many_arguments)]` initially
put on `diff_sync_from` (ticket `02`) was unnecessary — 7 parameters is at clippy's threshold,
not over it — and AGENTS.md forbids unnecessary allows, so it was removed and clippy re-run
clean without it. `Status:` now uses `resolved` from `docs/agents/triage-labels.md` rather than
the off-vocabulary `done`. `docs/cli.md`'s filter-syntax table also needed the new grammar; it
had a full field table that README's summary does not replace.

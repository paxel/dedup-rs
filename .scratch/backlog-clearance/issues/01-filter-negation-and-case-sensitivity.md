# 01 — Filter negation (`!pattern`) and case-sensitivity toggle

Status: ready-for-agent
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

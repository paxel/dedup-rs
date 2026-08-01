# 06 — Inline DELETE A / DELETE B pills in the native audio compare header

Status: resolved
Spec: ../spec.md

## Problem

Reported from real use: "when comparing two MP3s, no mark buttons exist."

The image compare header carries inline `DELETE A` / `DELETE B` pills. The native audio
compare header does not, so marking a copy means leaving the comparison and going back to the
Overview screen. Behaviour differs by media type for no reason the user can see.

**Decided in session:** add the inline pills. Overview-based marking stays; this is in
addition to it, chosen so that marking does not depend on which media type is being compared.

## Approach

Add the two pills to the native audio compare header, built from the **existing shared mark
pill helper** in the Duplicates view — the one the image compare header already uses. Do not
hand-roll a second pill; the helper already encodes the agreed vocabulary:

- labels are exactly `DELETE`, `DELETE A`, `DELETE B`
- a marked pill is filled
- a file in a read-only repository is *protected*: the pill reads `… (Protected)`, is
  disabled, and is struck through

Marking A must affect A only and marking B must affect B only — an earlier report of a single
click marking both sides is exactly the failure to avoid.

Header space is already tight with transport controls; the pills must not push the existing
controls out of the window.

## Seam and tests

GUI seam — inline `ui_tests` in the Duplicates view:

- while comparing two audio files, both pills are present
- clicking `DELETE A` marks A and leaves B unmarked; likewise for B
- a file in a read-only repository shows a disabled, struck-through pill and cannot be marked
- **geometric assertion**: with both pills present, every header control's rectangle stays
  inside the window at a narrow width — a label query passes even when a control is clipped

Render check: extend or add a `doc_screenshot_` test covering the audio compare header and
look at the PNG.

## Done

Standing gate green. `CHANGELOG.md`, `ai/improvements.md`, and the GUI documentation page for
the Duplicates tab updated.

## Comments

**Implemented 2026-07-31.** Gate green: fmt clean, clippy 0 warnings, `cargo test --workspace`
24 suites / 0 failures.

Built from the shared `mark_pill` helper, so `DELETE` / `DELETE A` / `DELETE B`, the filled
marked state and the disabled struck-through `… (Protected)` state are the image header's
exactly. While comparing there is an independent pill per copy; otherwise a single `DELETE`
for the copy on screen.

The source NOTE claimed audio "needs audio-scope mark bindings + a deferred toggle, unlike the
image lightbox's `acts`". That was accurate: `audio_lightbox` has no `acts` vec, and its
drawing closure cannot borrow `self` again. Resolved the same way the file already resolves
B's facts — compute the keys, `marked` and `markable` *before* the closure, collect clicks into
a local `toggle_marks`, and apply after drawing. The NOTE is gone.

**A false alarm worth recording, because it nearly became a bogus bug report.** The first test
asserted that clicking `DELETE A` leaves B unmarked, and it failed — looking exactly like the
dual-marking defect this ticket warns about. Instrumenting the real code path showed only one
toggle ever fired (`track0`). The cause was the test's premise: the Duplicates view **auto-marks
the copies it did not pick as best**, so B already carried a mark before any click. Clicking
`DELETE B` alone therefore *un*marked it, which is why an earlier probe showed an empty set.
The test now asserts the *transition* (each pill flips its own copy, leaving the other's mark
exactly as it was) rather than absolute membership. No product bug existed.

Two tests, both confirmed to fail when the new pills are removed and pass when restored:
`audio_compare_header_marks_each_copy_independently`, and
`audio_compare_header_controls_stay_inside_a_narrow_window`, a geometric assert at 900 px —
the header was already crowded with transport controls, and a label query passes even when a
widget is clipped.

Note on querying: the pills match `query_all_by_label_contains` rather than an exact label,
which is the idiom the rest of this file already uses.

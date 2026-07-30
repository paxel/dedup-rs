# 06 — Inline DELETE A / DELETE B pills in the native audio compare header

Status: ready-for-agent
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

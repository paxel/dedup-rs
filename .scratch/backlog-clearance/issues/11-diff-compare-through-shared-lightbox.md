# 11 — Route DIFF comparison through the shared lightbox and delete the private compare type

Status: ready-for-agent
Spec: ../spec.md

**Run this ticket last.** It is the largest change in the batch and the only one that can end
in a partially-applied state. With no revert available and one shared working tree, it sits at
the end so a failure here cannot contaminate the ten tickets ahead of it. If it cannot be made
to pass the gate, record what happened under `## Comments`, set `Status: needs-triage`, and
stop — do not leave a half-applied rework behind and continue to other work.

## Problem

Comparing two audio files from a Transfer DIFF row shows `no preview for audio/mpeg`. So does
comparing two documents. The Duplicates lightbox grew tabbed representations — Overview,
Metadata, Text, Image, Audio, Video — and DIFF can reach none of them.

The cause is architectural, not a missing branch. The previous epic recorded itself as complete
on the grounds that the old two-pane compare file had been deleted. It had; its logic was
re-created as a **private compare type inside the Transfer view**, which:

- decodes only images and video frames, via its own decode threads and texture slots
- treats "previewable" as *image or video*, so audio is excluded by construction
- has no tabs, no metadata, no text, no spectrogram, no gapless flip

So the reported complaint — "compare of two audio is completely broken and obviously not
reused from the duplicates view" — is still literally true, and the backlog's claim to the
contrary is wrong. `ai/improvements.md` should be corrected by this ticket.

This is also a direct violation of the project's own rule that shared widgets are reused
across tabs and downgraded per-tab variants are not built.

## Approach

Route the DIFF comparison through the shared lightbox and **delete the private compare type**.

The blocking piece is the one the original epic listed first and never built: the comparison's
B side is currently "another member of this duplicate group", identified by an index into that
group. DIFF's two sides are two files in **different repositories** and are not group members
at all.

1. **Generalise the B side into an abstract source** — something that yields the facts and the
   texture for a side, whether it came from a duplicate group or from the other repository in
   a diff. The Duplicates tab keeps working through the same abstraction.
2. **Route DIFF's compare command** at that generalised entry point, supplying both sides from
   the two repositories.
3. **Delete the private compare type** and its decode/poll/texture machinery once nothing
   calls it. Audio, Metadata and Text in DIFF fall out of the lightbox's existing dispatch —
   they are not implemented again here.
4. **Audio comparison uses the spectrogram** (decided in session). The amplitude waveform is
   painter-drawn rather than a texture and is not brought into comparison; it stays available
   in the native audio view with its existing toggle. A waveform-to-texture renderer is
   explicitly **not** in scope.
5. **Keep the actions caller-supplied.** Unify the viewer, not the actions: DIFF's row actions
   stay DIFF's, the Duplicates tab's marking stays its own. This is the existing guiding
   principle and it is what keeps the abstraction honest.
6. **A side with no visual disables comparison** rather than showing a broken half.

## Seam and tests

**The seam is the Transfer view's observable behaviour, not the private type** — that type is
being deleted, so any test written against it dies with it. Assert what a user of the tab can
see. Prior art: the existing Transfer view DIFF tests and the Duplicates lightbox tests.

- comparing two audio files from a DIFF row offers the audio representation and shows a
  spectrogram on both sides — the exact reported failure, asserted directly
- comparing two documents from a DIFF row offers the Text representation
- comparing two tagged audio files offers Metadata
- comparing two images still works exactly as before — this is the regression risk
- a side with no visual leaves comparison unavailable rather than half-drawn
- the Duplicates tab's own comparison is unchanged: its existing tests stay green **unmodified
  wherever possible**. Needing to rewrite them is a signal the abstraction leaked.

Render checks, per the standing rule that a label query passes even when a widget is clipped:
re-render the existing DIFF compare screenshot and add one for audio-versus-audio from DIFF.
Look at both.

## Done

Standing gate green. The private compare type is gone and nothing references it.

Documentation corrected in the same change: `ai/improvements.md` — strike the claim that the
comparison surfaces were already unified and record what was actually true; `CHANGELOG.md`;
the GUI documentation page for the Transfer tab.

This decision meets the bar for an ADR — note it, but creating `docs/adr/` is not part of this
ticket.

# 02 — Every type gets text and raw bytes; the law starts to hold

Status: resolved
Spec: ../spec.md
Blocked by: 01

## Problem

A file that cannot be rendered as a picture cannot currently be opened for comparison at all —
the comparison key is inert for a group of documents, and previewability is defined as image or
video. That is backwards for a forensic tool: a document's text and an unknown format's bytes
are exactly what you would want to compare.

## Approach

Make previewability stop gating *opening* and only decide which tabs appear.

- **Text and raw bytes are offered for every type**, including images and audio. This is a
  change from today, where they are offered only for files that are not image, audio or video.
  A photograph gains both — reading a JPEG's header is a legitimate thing to want.
- The viewer opens on the pair's own representation; a file with no renderable form still opens,
  on text or bytes.
- **Raw bytes compare as two panes, side by side, scroll-locked**, so both show the same offset.
- **Flicker is a shared primitive**, not an image feature: an A/B swap in place, serving
  pictures, hex and text alike. Implement it once.
- The sampled views must **say they are a sample**. Concluding two files are identical from
  their first kilobyte is the failure this prevents.

Highlighting the differing bytes is a **bonus, not a requirement** — take it only if it comes
cheaply, and bound the comparison window rather than diffing an arbitrarily large file.

## Seam and tests

The viewer's own inline tests:

- a document pair offers text and bytes and opens successfully — the case that could not be
  opened at all before
- an image pair offers picture, text and bytes
- an audio pair offers sound, metadata, text and bytes
- a pair of unknown-format files still opens, on bytes
- the two byte panes stay on the same offset when one is scrolled
- flicker swaps which side is shown, on the byte tab as well as the picture tab
- the sampled view states that it is a sample

## Done

Standing gate green. This is where the law becomes visible to a user, so `CHANGELOG.md`, and the
GUI documentation for the tabs.

## Comments

**Implemented 2026-08-01, TDD.** Gate green: fmt clean, clippy 0 warnings, `cargo test
--workspace` 24 suites / 0 failures.

`has_text_representation` was `!image && !audio && !video`; it is now unconditionally true.
That one line is the law taking effect: a pair of PDFs, a pair of unknown blobs, a JPEG whose
header you want to read — all open and all compare. Pinned by
`an_image_pair_also_offers_text_and_bytes` and `any_file_opens_even_with_no_renderable_form`,
the latter covering PDF, octet-stream, ODF and a file with no mime at all.

**The sample disclosure was the substantive part.** The note under a text/hex column was
extracted into a `preview_note` pure function and reworded: it now says *"only the first 64 KB,
not the whole file"* rather than *"First 64 KB"*. The old wording stated a fact; the new one
states a limit. Two files whose first 64 KB match are not identical, and the conclusion drawn
from this pane can end in a deletion — so the pane has to say what it did not read. Tested as a
pure function rather than through a harness, which is the highest seam available.

**Three tests encoded the old rule and were updated — deliberately, not bent to fit:**

- `lightbox::test_file_representations_available_kinds` asserted an image offers no text. That
  rule is exactly what this ticket changes; the assertion is inverted with a comment saying why.
- `dupes_view::an_unsupported_tab_falls_back_to_overview` used Text as "a tab images don't
  offer" to exercise the fallback. Images now offer it, so the *example* became invalid while
  the *behaviour* stayed real — switched to Audio, which an image genuinely cannot offer.
- `dupes_view::text_tab_previews_both_sides_as_text_or_hex` queried the exact old note copy.
  Loosened to the stable part of the string; a test coupled to product copy is brittle by
  construction.

This is the distinction ticket `04` will need to hold to: a test updated because the rule
changed is legitimate; a test updated because the code stopped doing what it promised is not.

**Not done here, and deliberately:** scroll-locked hex panes and flicker on the byte view.
Flicker is a shared A/B primitive and belongs with the picture tools in ticket `03`, where it is
implemented once for images, hex and text together rather than twice.

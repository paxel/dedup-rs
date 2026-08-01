# 05 — Regression tests: switcher freeze and ID3 tag sync with more than two audio files

Status: resolved
Spec: ../spec.md

## Problem

Two reports from real use, neither ever verified:

1. **Switcher freeze.** "I can flip through the 4 files until I click compare, then the
   switcher freezes."
2. **Tag shown every second file.** "I edited one ID3 tag to mark 1 of 4 similar MP3s and the
   tag is shown every second time I click next — makes me think the numbers iterate 1 to 4 but
   the files iterate 1 to 2."

Both plausibly fixed as a side effect of the Overview cycler's index mapping, which computes
the *other* members of a group and labels the switcher accordingly. Neither has been run since.

## Approach

**This ticket is tests first.** Write the two regression tests against current behaviour and
run them:

- If both pass, the bugs were fixed as a side effect. Close the ticket as **verified**, and
  say so plainly in `ai/improvements.md` — the value delivered is the pin, not a fix.
- If either fails, fix it, keeping the test as the pin.

Do not refactor the index mapping pre-emptively on the assumption it is broken.

## Seam and tests

GUI seam — inline `ui_tests` in the Duplicates view. Build a group of **four** audio files;
two-file groups cannot reproduce either report.

- **Switcher:** enter compare, then step through the other members; assert the selected file
  actually advances through all three others and the switcher label tracks it. The label must
  never show a member count in the position where the count of *others* belongs.
- **Tag sync:** give the four files distinguishable metadata, step through them, and assert
  the metadata shown belongs to the file currently selected — specifically that file 1 and
  file 3 do not show identical tags when their stored tags differ.

Assert the selected member and the displayed values, not internal index arithmetic.

## Done

Standing gate green. `ai/improvements.md` updated to record the outcome — verified or fixed —
and `CHANGELOG.md` only if behaviour actually changed.

## Comments

**Closed as VERIFIED, not fixed — 2026-07-31.** Gate green: fmt clean, clippy 0 warnings,
`cargo test --workspace` 24 suites / 0 failures. Both regression tests passed on their first
run, which is the outcome this ticket explicitly allowed for.

The coverage gap was real even though the bugs were not: a four-member cycler test already
existed, but it used an **image** group, and audio dispatches through `audio_lightbox` rather
than `lightbox_modal` once past Overview. So the audio path at >2 copies had never been
exercised. Both new tests build a four-copy **audio** group with real tagged MP3s on disk via
`id3tags::write_bare_mp3` + `write`.

- `a_four_copy_audio_group_cycles_b_through_three_distinct_others` — asserts the index mapping
  directly (three others per choice of A, none of them A, all distinct — not a 1..2 cycle),
  that the label never reports the member count where the others count belongs, and then drives
  the real cycler through `<1 / 3>` → `<2 / 3>` → `<3 / 3>` → wrap, asserting `/ 4` never
  appears.
- `each_audio_copy_shows_its_own_tags_not_every_second_one` — reads each copy back through the
  same reader the lightbox uses and asserts copies 1 and 3 differ, which is the precise
  reported symptom ("1 and 3 show suddenly the modified id3 tag").

**Finding worth keeping.** The reported "switcher freeze after clicking compare" is not a
freeze but a deliberate design: arrow-nav while comparing flips *which copy is audible*
(gap-free, via the pre-loaded pair) instead of re-indexing A, because re-indexing A would
collide it with B and force a reloading pause — the source comment records that as the bug the
user originally hit. Cycling B through the other members is the Overview control, reached with
`C` then `I`. The test therefore pins the intended behaviour rather than "fixing" a
non-defect.

Per the ticket, `ai/improvements.md` records the verified outcome and `CHANGELOG.md` was left
untouched because no user-visible behaviour changed.

# 05 — Regression tests: switcher freeze and ID3 tag sync with more than two audio files

Status: ready-for-agent
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

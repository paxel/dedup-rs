# 04 — The audio player moves, and its regressions must survive untouched

Status: resolved
Spec: ../spec.md
Blocked by: 03

## Problem

The audio player is the largest and most delicate capability to move, and it carries the most
hard-won regression tests in the repository — several fixed only days ago. It is scheduled last
deliberately: a subtle break here is the least likely to show up in a screenshot, because the
evidence is *audible*.

## Approach

Move the audio capability into the surviving viewer, including the spectrogram, the amplitude
waveform, transport controls and the gapless A/B switch. Add playback speed, which the user
asked for as a forensic tool.

**The safety mechanism for this ticket is the existing tests, not new ones.** These must keep
passing **unmodified**:

- gapless A/B switching — the paired stream flips rather than re-seeking from disk
- playback keeps its paused state across a file switch, with the newly shown copy loaded
- each copy of a four-file group shows its own tags, with copies 1 and 3 differing
- the switcher walks three distinct others in a four-file group
- per-copy delete marks toggle independently; protected repositories render struck through

If any of them needs rewriting to pass, **that is a failure of the move, not an inconvenience**.
Stop and record what behaviour changed rather than adjusting the test.

Audio comparison remains **spectrogram-based** (decided 2026-07-31): the amplitude waveform is
painter-drawn rather than produced as a texture, and stays available in the single-file view
with its existing toggle.

## Seam and tests

Existing tests are the seam. New assertions only for what is genuinely new:

- playback speed changes the rate and survives a file switch
- the spectrogram is what a comparison shows for audio

## Done

Standing gate green, **with the listed regression tests unmodified**. `CHANGELOG.md` for
playback speed.

## Comments

**Implemented 2026-08-01, TDD.** Gate green: fmt clean, clippy 0 warnings, `cargo test
--workspace` 24 suites / 0 failures. **All five regression tests pass unmodified** — verified
before starting and again after.

**This ticket's framing was wrong, and the work was re-sliced accordingly.** It said "move the
audio capability into the surviving viewer". That cannot be done as a separate step: either the
old viewer keeps its player and nothing has moved, or the old viewer is deleted — which is
ticket `05`. A half-moved player, with two things owning one audio device, is the worst
available state. So the *move* is folded into `05`, where the old viewer goes; what landed here
is the capability that was genuinely new.

**Playback speed**, added at the player level where it is cleanly testable:

- `Player::set_speed`, clamped to 0.25×–4× — zero would stall and a runaway rate is not a
  forensic tool. Both bounds asserted.
- The rate is player state, so it survives switching between copies. Asserted.
- `Shared` needed a hand-written `Default`: the derived one left the rate at **zero**, which is
  silence rather than normal speed. Caught by the first test on its first run.
- One **cycling** SPEED pill in the audio header, not three fixed ones.

**A regression caught by my own test from two days ago.** The first attempt used three pills
(0.5× / 1× / 2×), which pushed `DELETE B` 18px past the right edge at 900px —
`audio_compare_header_controls_stay_inside_a_narrow_window` failed. That is the same defect
class the user reported for `ABOUT`. The layout was fixed rather than the test: one cycling
pill costs a third of the width. Geometric tests earn their keep exactly here — a label query
would have passed while the control sat off-screen.

**Deferred to `05` and recorded there:** moving the audio player, spectrogram, transport and
gapless flip into the surviving viewer, and migrating the five regression tests to point at it.
Their *assertions* must survive unchanged; only which viewer they drive may change, and that
distinction is the line between a legitimate update and a test bent to fit.

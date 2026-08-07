# 04 — Click-to-scrub (proportional shared playhead)

**What to build:** Clicking a region of the video filmstrip drops a **shared
playhead** that lets you inspect a specific moment more finely than the sampled
frames allow. Both sides decode the frame at that moment on demand and show it
enlarged, **A@t | B@t** side by side, so you compare the exact same instant of both
clips. The playhead is **proportional**: its position is a fraction (0–100%) of each
side's *own* duration, so two copies of different length — a trim, a re-encode — stay
aligned at the same relative moment instead of drifting apart.

**Blocked by:** 03 — Video filmstrip (there must be a filmstrip to click into).

**Status:** resolved

- [x] Clicking a filmstrip region sets a shared fraction (0.0–1.0); each side maps it
      to a timestamp against **its own** duration and decodes that arbitrary frame via
      the existing `fingerprint::video_frame(source, at)`.
- [x] The enlarged result is shown A@t | B@t; decode is on demand and off the UI
      thread (consistent with the existing `spawn_decode` pattern).
- [x] The fraction→per-side-timestamp mapping is a **pure function** (no egui, no I/O).
- [x] Unit test the mapping directly: a 50% playhead maps to 1:00 on a 2:00 clip and
      to 0:52 on a 1:45 clip (proportional alignment holds across differing lengths).

## Comments

- 2026-08-07 — Implemented and verified (`cargo test`, clippy clean). See CHANGELOG "[Unreleased]" and `docs/screenshots/video-diff.png`.

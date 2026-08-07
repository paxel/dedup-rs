# 03 — Video filmstrip

**What to build:** The Video tab in the shared viewer stops showing a single frame
per side and instead shows an **aligned filmstrip** — a fixed row of frames sampled
evenly across each clip — as the always-visible overview. Comparing two videos, you
see each clip's whole shape and can spot at a glance where they diverge, instead of
guessing from one still (which is often identical black/slate/logo on both). Frames
come from the existing cached-JPEG grid so the strip is instant after first build and
stepping through a group stays fluid.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] The Video representation renders **N evenly-spaced stills per side** (start
      N = 8, tunable), replacing the current single `(0, 1)` still, via the existing
      `thumbnail::video_frame_rgba(source, hash, idx, count)` grid.
- [x] Frames are cache-backed (existing `<hash>-v<idx>of<count>.jpg` pattern); the
      strip appears immediately when cached and decodes off the UI thread otherwise.
- [x] A clip with no fingerprint / no ffmpeg falls back to a placeholder, not a crash.
- [x] `compare_view` kittest: assert the filmstrip's **rects** — N frames laid out
      per side — not merely that a label exists (a label passes even when clipped).

## Comments

- 2026-08-07 — Implemented and verified (`cargo test`, clippy clean). See CHANGELOG "[Unreleased]" and `docs/screenshots/video-diff.png`.

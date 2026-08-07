# 01 — Audio tab on video (soundtrack diff)

**What to build:** When a video is opened in the shared viewer, it offers the
**Audio** representation — but only when the clip actually carries an audio track.
Selecting it shows the soundtrack exactly as a bare audio file would: the spectrogram
(from `waveform`), playback through the audio `Player`, and — when comparing two
videos — the paired A/B flicker where both play in sync but only one side is audible,
so you never hear two soundtracks at once. The track is pulled out of the container
once with ffmpeg into a cached WAV keyed by content hash, so reopening is instant.
If ffmpeg is unavailable, the Audio tab simply isn't offered (same graceful
degradation as video fingerprints); a silent video offers no Audio tab either.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] A new `dedup-core` function ensures a video's audio track exists as a cached
      `<hash>.wav` in `thumbnail::cache_dir()`, extracting via ffmpeg, idempotent
      (existing file reused, not re-extracted), mirroring `ensure_video_frame`.
- [x] "Has an audio track" is detected via the existing ffprobe-based media
      inspection; a video with no audio stream does not offer the Audio tab.
- [x] A video's representation set includes **Audio** when a track is present, with
      Video remaining the landing tab and Audio beside it.
- [x] The spectrogram (`waveform::spec_rgba`) and the `Player` are driven from the
      extracted WAV path, unchanged — including paired play (one sink audible).
- [x] Missing ffmpeg / extraction failure omits the Audio tab rather than erroring.
- [x] Core test (ffmpeg-gated, like the video-frame tests): the extractor yields a
      decodable WAV for a clip with sound and no track for a silent clip.
- [x] `compare_view` kittest: a video with audio offers the Audio representation
      (assert the tab is offered, not merely that a label exists).

## Comments

- 2026-08-07 — Implemented and verified (`cargo test`, clippy clean). See CHANGELOG "[Unreleased]" and `docs/screenshots/video-diff.png`.

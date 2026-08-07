# 02 — Pitch-preserving play-rate stops (0.25–2×)

**What to build:** The audio transport in the shared viewer gains discrete speed
stops — **0.25 / 0.5 / 0.75 / 1 / 1.5 / 2×**, 1× default — so a passage can be
slowed to confirm two tracks are the same recording, or a long one skimmed. Crucially
the speed change **preserves pitch**: a slowed track still sounds like itself instead
of dropping an octave, so it stays recognizable. The chosen rate applies to both
single playback and the paired A/B flicker, keeping the two soundtracks aligned. This
enhances the audio transport generally (bare audio and video-extracted audio alike),
so it depends on neither video nor ticket 01.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] The audio transport exposes the six rate stops with 1× as default and applies
      the selection to single and paired playback.
- [x] Non-1× rates are pitch-preserving via an ffmpeg **`atempo`** pre-render into
      the cache (`<hash>-r<NNN>.wav`), which the `Player` plays at 1× — not rodio's
      pitch-shifting `set_speed`. No new dependency.
- [x] `atempo` covers 0.5–2.0 in one pass; **0.25× chains** it (`atempo=0.5,atempo=0.5`).
- [x] First selection of a rate renders once and is cached; later selections are
      instant. In paired flicker both sides render at the chosen rate.
- [x] Pure rate-step cycle logic (the six stops, default, wrap/clamp) is a testable
      function with no UI dependency, unit-tested directly.
- [x] Rate render test (ffmpeg-gated): the `atempo` extractor yields a decodable WAV
      for a representative rate and for the chained 0.25× case.

## Comments

- 2026-08-07 — Implemented and verified (`cargo test`, clippy clean). See CHANGELOG "[Unreleased]" and `docs/screenshots/video-diff.png`.

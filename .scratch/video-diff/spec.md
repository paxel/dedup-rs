# VIDEO DIFF — soundtrack diffing, frame filmstrip + scrub, audio play-rates

Status: resolved

Spec produced by a grilling session on 2026-08-07. Every decision below was put to
the user and chosen by them; the rationale is the reason given at the time. Where a
choice was deliberately deferred ("built later") it is called out in **Out of
Scope**, not silently dropped.

## Problem Statement

In the shared viewer (`compare_view::DiffCompare`), video is the poor cousin of
every other media kind. When I compare two videos the app shows me **one static
still per side** — the frame at position 0 — and nothing else. That is not enough
to triage an inherited pile of clips:

- I cannot compare the **soundtracks**. Two clips grouped as "similar" might be the
  same footage with a different audio track, or the same audio over re-encoded
  video — but a video never offers the Audio tab, so I can never see or hear its
  sound the way I can for a bare audio file.
- I cannot inspect **across the timeline**. One frame at 0% tells me almost
  nothing; two different videos often share an identical first frame (black,
  a slate, a logo). I need to see the whole clip's shape and drill into any moment.
- Everything the audio viewer already gives me — spectrogram compare, an A/B
  flicker where only one side is audible, playback — is unavailable to a video even
  though the video *has* an audio track sitting right there.

## Solution

Bring video up to parity with the other media kinds in the shared viewer, in three
parts, plus a feasibility investigation:

1. **Audio tab on videos.** A video file offers the **Audio** representation
   whenever it has an audio track. The track is extracted once with ffmpeg to a
   cached WAV, and from there the *existing* spectrogram (`waveform`) and audio
   `Player` treat it exactly like a bare audio file — including the paired A/B
   flicker, where both sides play in sync but only one is audible, so I never hear
   two soundtracks at once. If ffmpeg is unavailable the Audio tab simply isn't
   offered (the same graceful degradation as video fingerprints).

2. **Playback rates on the audio transport.** The audio transport in the viewer
   gains discrete speed stops — **0.25 / 0.5 / 0.75 / 1 / 1.5 / 2×** (1× default) —
   so I can slow a passage down to confirm two tracks are the same recording, or
   speed through a long one. Speed changes are **pitch-preserving**: a slowed track
   still sounds like itself, not an octave lower, so I can actually recognize it.

3. **Video tab: filmstrip + click-to-scrub.** The Video representation shows an
   **aligned filmstrip** — a fixed row of frames sampled evenly across each clip —
   as the always-visible overview, from cache, instantly. Clicking into a region
   drops a **shared playhead** that decodes both sides at that exact moment for fine
   inspection. The playhead is **proportional**: its position is a fraction
   (0–100%) of *each side's own duration*, so two copies of different length (a trim,
   a re-encode) stay visually aligned and I compare the same relative moment.

4. **Feasibility investigation: in-app synced video playback.** Actual video
   *watching* today happens in the external OS player (`external::open`). Whether the
   viewer should ever play video **in-app** — two clips playing synced with an A/B
   flicker, muted by default — is an open question with real cost (a decode/upload
   pipeline that does not exist). This spec commissions a **written feasibility
   report**, not an implementation, so the decision can be made on evidence.

## User Stories

1. As someone triaging inherited clips, I want a video to offer the Audio tab, so
   that I can examine its soundtrack the same way I examine a bare audio file.
2. As a triager, I want a video's audio track shown as a spectrogram, so that I can
   see at a glance whether two clips carry the same sound.
3. As a triager, I want to play a video's audio inside the viewer, so that I can
   confirm by ear that two clips are the same recording without leaving the app.
4. As a triager comparing two videos, I want their soundtracks in an A/B flicker
   with only one side audible, so that I can switch between them instantly and never
   hear both at once.
5. As a triager, I want the Audio tab to appear only when a video actually has an
   audio track, so that silent clips don't offer an empty, useless tab.
6. As a user without ffmpeg installed, I want the Audio tab to be silently absent on
   videos, so that the app degrades gracefully instead of erroring.
7. As a triager, I want the audio extracted once and cached, so that reopening the
   same clip's Audio tab is instant and doesn't re-run ffmpeg.
8. As a triager, I want playback-rate stops of 0.25/0.5/0.75/1/1.5/2×, so that I can
   slow a passage to compare detail or speed through a long track.
9. As a triager, I want a slowed track to keep its original pitch, so that it still
   sounds like the recording I'm trying to identify instead of dropping an octave.
10. As a triager, I want the chosen rate to apply to the synced A/B pair, so that
    both soundtracks stay aligned while I flicker between them at any speed.
11. As a triager comparing two videos, I want an aligned filmstrip per side, so that
    I can see each clip's whole shape and spot where they diverge at a glance.
12. As a triager, I want the filmstrip to come from cache instantly, so that
    stepping through a group of clips is fluid and doesn't stall on decode.
13. As a triager, I want to click a region of the filmstrip to drop a shared
    playhead, so that I can inspect a specific moment more finely than the sampled
    frames allow.
14. As a triager, I want the shared playhead to decode *both* sides at the same
    relative position, so that I am always comparing the same moment of the two
    clips.
15. As a triager comparing clips of different lengths, I want the playhead measured
    as a fraction of each clip's own duration, so that a trimmed or re-encoded copy
    stays aligned with its longer counterpart instead of drifting.
16. As a triager, I want the enlarged frame shown as A@t | B@t side by side, so that
    I can compare the exact same instant of both clips directly.
17. As a triager, I want a clip with no fingerprint or no ffmpeg to fall back to a
    placeholder rather than a crash, so that the viewer stays robust on odd files.
18. As a maintainer, I want a written feasibility report on in-app synced video
    playback, so that we can decide whether to build it on evidence rather than
    guesswork.
19. As a triager, I want to keep watching a clip full-speed in my external player,
    so that the in-app tools augment rather than replace the OS video player.
20. As a triager, I want the Video tab to be the natural landing tab for a video and
    the Audio tab available beside it, so that the two representations of one file
    sit together the way image/metadata already do.

## Implementation Decisions

**Audio extraction (dedup-core).**
- A new core function ensures a video's audio track exists as a cached WAV, keyed by
  content hash, mirroring the existing `ensure_video_frame` / `video_frame_rgba`
  shape and living beside them in the `thumbnail` module. It shells out to ffmpeg,
  writes `<hash>.wav` into the existing `thumbnail::cache_dir()`, and returns the
  path (idempotent: existing file is reused, not re-extracted).
- ffmpeg is the single decode path — no rodio-direct attempt, no fallback branch —
  chosen for robustness across codecs (AAC/AC3/Opus) and consistency with the
  ffmpeg dependency video already carries. Missing ffmpeg → the function fails
  cleanly and the caller omits the Audio tab.
- Detecting "has an audio track" reuses the existing ffprobe-based media inspection
  already used for duration; a video with no audio stream must not offer the tab.

**Consuming the extracted audio (dedup-gui).**
- `waveform::spec_rgba` and the audio `Player` are **unchanged**: they receive the
  cached WAV path in place of the original file. The existing paired-play (A/B
  flicker, one sink audible) works for two videos for free once both sides resolve
  to their extracted WAV paths.
- The audio transport gains a discrete **rate-step** control
  (**0.25 / 0.5 / 0.75 / 1 / 1.5 / 2×**, 1× default), applied equally to single and
  paired playback. Rate changes are **pitch-preserving**: rather than rodio's
  `set_speed` (which resamples and shifts pitch), a non-1× rate is served by an
  ffmpeg **`atempo`** pre-render into the cache — `<hash>-r<NNN>.wav` — which the
  `Player` then plays at 1×. This extends the same extract-to-WAV seam and adds no
  new dependency. `atempo` covers 0.5–2.0 in one pass; 0.25× chains it
  (`atempo=0.5,atempo=0.5`). First selection of a rate renders once (a beat of
  latency); it is cached thereafter. In the paired A/B flicker both sides render at
  the chosen rate so they stay aligned.

**Representation offering (dedup-gui `lightbox`).**
- A video's representation set gains **Audio** when (and only when) an audio track
  is present. The `Video` representation already carries `filmstrip_textures`,
  `selected_frame`, and `is_playing` scaffolding — these become live.
- Tab ordering keeps Video as the landing representation for a video, with Audio
  offered beside it, consistent with how the other kinds sit in `offered()`.

**Video filmstrip + scrub (dedup-gui `compare_view`).**
- The filmstrip is a fixed row of **N evenly-spaced stills per side**, decoded via
  the existing `thumbnail::video_frame_rgba(source, hash, idx, count)` grid (the
  current single-still `(0, 1)` call becomes `(0..N, N)`). Start N at **8**,
  tunable; frames are cached JPEGs so the strip is instant after first build.
- Clicking a filmstrip region sets a shared **fraction** (0.0–1.0). Each side maps
  the fraction to a timestamp against **its own** duration and decodes that arbitrary
  frame via the existing `fingerprint::video_frame(source, at)`; the enlarged result
  is shown A | B. Decode is on-demand and off the UI thread, consistent with the
  existing `spawn_decode` pattern.
- The fraction→per-side-timestamp mapping and the rate-step cycle are extracted as
  **pure functions** (no egui, no I/O) so they can be unit-tested directly.

**Feasibility investigation (deliverable, not code).**
- Produce a findings document assessing in-app synced A/B video playback: candidate
  decode/upload approaches (e.g. gstreamer, ffmpeg-next, manual ffmpeg-pipe + wgpu
  texture upload), the sync + flicker + mute-by-default interaction model, new
  dependency/build-system cost (recall ALSA/ffmpeg are already required), and a
  clear recommendation. No production code; captured as a research ticket under this
  feature.

## Testing Decisions

Good tests here assert **external behavior** — extracted output that decodes, tabs
that are offered, frames that are laid out, mapped values — never private fields or
call order.

- **Core audio extraction** — new `dedup-core/tests` coverage that runs the
  extractor on a fixture clip and asserts the result is a real, decodable WAV, plus
  that a clip with no audio stream yields no track. Prior art: the existing
  `ensure_video_frame` / video-fingerprint tests, which are ffmpeg-gated the same
  way; this test follows that gating so machines without ffmpeg skip rather than
  fail.
- **Pure mapping logic** — direct unit tests on the fraction→per-side-timestamp
  function (a 50% playhead maps to 1:00 on a 2:00 clip and 0:52 on a 1:45 clip) and
  on the rate-step cycle (the six stops 0.25–2×, wrap/clamp behavior, 1× default).
  These are the highest seam for the alignment guarantee and need no UI. Prior art:
  the pure-math tests in `similar.rs` (`similarity_video`).
- **Pitch-preserving rate render** — assert the ffmpeg `atempo` extractor produces a
  decodable WAV for a representative rate (and that 0.25× — the chained case —
  works), ffmpeg-gated like the audio-extraction test it extends.
- **Representation offering + filmstrip layout** — reuse the existing
  `compare_view` `egui_kittest` harness: assert that a video now offers the Audio
  representation and a Video filmstrip, and assert the filmstrip's **rects** (N
  frames laid out, not merely that a label exists), per the GUI convention that a
  label query passes even when clipped. Prior art: the existing `RepresentationKind`
  tab assertions in `compare_view` tests.
- **No new test** is written for the feasibility investigation; its deliverable is a
  reviewed document.

## Out of Scope

- **Manual sync offset (unlock → nudge → relock).** The proportional playhead is
  designed to accommodate a future manual offset that hand-aligns two clips with an
  intro trim or drift, applying to both the frame scrub and the audio flicker. It is
  **not built in this pass** — only the proportional baseline it will extend.
- **Building in-app video playback.** This spec commissions only a feasibility
  report. No decode/playback engine, no synced video flicker, no new heavy media
  dependency is added here regardless of the report's conclusion.
- **The external-player path.** `external::open` (watch a clip full-speed in the OS
  player) is unchanged and remains the way to actually watch video.
- **Video content fingerprinting.** The stored temporal video hash and similarity
  grouping (`fingerprint`, `similar`) are untouched; this is purely a viewer feature.
- **Absolute-timestamp alignment.** The playhead is proportional only; a same-length
  absolute-lock mode was considered and set aside (the future offset covers the
  drift case).

## Further Notes

- The audio A/B flicker never plays two soundtracks audibly at once — the existing
  paired `Player` keeps exactly one sink audible — which is why "muted by default"
  needed no special handling for audio. The mute-by-default concern belongs entirely
  to the (out-of-scope) in-app *video* playback, and is captured in the feasibility
  report.
- Filmstrip frame count N starts at 8 as a legibility default and is a cheap knob;
  the browse cards sample 5, but a diff overview benefits from a denser strip.
- Cache hygiene: extracted WAVs share `thumbnail::cache_dir()` and the `<hash>`
  keying already used for stills, so they are collision-free across repos and reused
  across sessions.

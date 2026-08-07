# Feasibility report: in-app synced video playback

Deliverable of ticket `05-feasibility-in-app-video-playback.md` (research, no
production code). Written 2026-08-07, against the codebase as of the video-diff
feature (filmstrip + proportional scrub + soundtrack Audio tab landed).

## Question

Should the shared viewer (`compare_view::DiffCompare`) ever play video
**in-app** — two clips playing synced, A/B flicker, muted by default with one
side carrying sound on "normal play" — instead of only handing off to the
external OS player (`external::open`)?

## What exists today (the baseline being compared against)

- **Filmstrip + proportional scrub**: 8 cached stills per side, a shared
  fractional playhead, on-demand exact-frame decode per side via one-shot
  `ffmpeg` invocations (`fingerprint::video_frame`). No persistent decoder
  process, no state.
- **Soundtrack A/B**: a video's audio track is extracted once to a cached WAV
  and driven through the existing paired `rodio` sinks — synced, gap-free
  flicker with exactly one side audible, including pitch-preserving rate stops.
- **Watching** happens in the OS player, full speed, hardware-accelerated.

So the triage questions "same footage?" (filmstrip + scrub) and "same
soundtrack?" (Audio tab) are already answered without a playback engine. What
in-app playback would add is *motion* — judging temporal artifacts (frame-rate
differences, interlacing, stutter, encode smearing) and flicking between two
moving clips at the same instant.

## Candidate decode/upload approaches

### 1. GStreamer (`gstreamer` / `gstreamer-app` crates)

A full media framework with demux/decode/convert pipelines; `appsink` hands
RGBA frames to the app, which uploads them as `egui` textures.

- **Pros**: battle-tested A/V sync machinery, hardware decode where available,
  precise seeking, pausing, rate control; the sync model (two pipelines slaved
  to one clock) is exactly the "two clips in lockstep" primitive we would need.
- **Cons**: a *heavy* new dependency tree — the system GStreamer libraries plus
  per-codec plugin packs (`gst-plugins-{base,good,bad,ugly}`). Packaging burden
  lands on every user, dwarfing the current "ffmpeg on PATH is optional"
  posture. API is famously ceremony-heavy; error surface (missing plugins per
  distro) is large. Windows/macOS bundling is its own project.

### 2. `ffmpeg-next` (bindings to libav*)

Link the ffmpeg libraries and decode in-process.

- **Pros**: one library family we already conceptually depend on; full control
  over decode + scaling; no subprocess churn.
- **Cons**: native linkage against libav* versions that differ per distro
  (build breakage is the norm — the crate needs the right ffmpeg dev headers at
  build time, and ABI churn across ffmpeg 5/6/7 is real). We would trade "runs
  without ffmpeg, degrades gracefully" for a *hard build-time* dependency on
  ffmpeg dev libraries — a regression for a tool whose core is file triage, not
  playback. A/V sync, clocking, and frame pacing must all be hand-built.

### 3. Manual `ffmpeg` pipe + wgpu texture upload

Spawn the already-required `ffmpeg` binary per side
(`-i clip -f rawvideo -pix_fmt rgba -`), read frames from stdout on worker
threads, upload each frame as a texture (exactly how decoded previews are
uploaded today), and pace presentation with `ctx.request_repaint_after`.

- **Pros**: zero new dependencies — it is the same subprocess seam every
  video feature here already uses; degrades exactly like the rest (no ffmpeg →
  feature absent); the frame→texture path exists (`poll`/`load_texture`).
- **Cons**: we own the hard parts ourselves. Frame pacing against wall-clock,
  drift correction between two pipes, seek = kill + respawn both processes
  (latency ~100–300 ms per seek), pause = process lifecycle management.
  Audio-for-video sync ("one side carrying sound on normal play") means slaving
  the rodio sink clock to the video pacing — the classic A/V sync problem,
  hand-rolled. Sustained decode of two clips at, say, 1080p30 RGBA is
  ~350 MB/s of pipe + texture-upload traffic; feasible on a desktop GPU, but
  egui's repaint-the-world model makes it a constant-repaint app while playing
  (fans up, battery down). CPU-only decode of two 4K clips will drop frames.

### A note on "egui video player" crates

Small crates exist (e.g. `egui-video`, historically built on ffmpeg bindings)
but they are single-clip, maintained thinly, and none offer synced dual
playback — the one thing this feature is actually about. They would be a
dependency *and* a fork.

## The interaction model (if built)

- Two clips play slaved to one clock; position is the existing **proportional
  fraction** by default, so unequal lengths stay on the same relative moment
  (playing at slightly different wall-clock rates — a 2:00 and a 1:45 clip
  finish together), with the deferred **manual sync offset** as the future
  corrective for intros/trims — the same offset the scrub playhead is already
  designed to accommodate. An absolute-time mode would only make sense once
  that offset exists.
- **Muted by default**: motion comparison first; two soundtracks at once are
  never acceptable (the standing rule of the paired audio player). "Normal
  play" un-mutes exactly one side — which is precisely the existing paired-WAV
  sink model, reused: video pacing would follow the audio clock of the audible
  side.
- **A/B flicker**: one decoded stream fills the pane, swap flips which stream
  is presented (both keep decoding; flicker must not re-seek). This is cheap
  once both pipelines run — it is presentation-side, like the image flicker.
- It would live inside the existing Video tab as a "play" mode over the
  filmstrip + playhead chrome, not a new tab.

## Dependency / build-system cost

| Approach           | Build cost                                   | Runtime cost                            | Sync burden                |
|--------------------|----------------------------------------------|-----------------------------------------|----------------------------|
| GStreamer          | new system libs + plugins everywhere         | plugin availability per distro          | mostly solved by framework |
| ffmpeg-next        | hard dep on libav dev headers, version churn | none extra                              | entirely ours              |
| ffmpeg pipe + wgpu | **none** (binary already optional-required)  | subprocess + high pipe/upload bandwidth | entirely ours              |

ALSA (`rodio`) and the ffmpeg *binary* are already required/expected; only the
pipe approach stays inside that envelope.

## Relation to existing work

- The **proportional scrub** already gives frame-exact "compare this moment",
  which covers most of the triage value motion would add.
- The **soundtrack A/B flicker** already answers "same recording?" by ear,
  synced and gap-free.
- The **manual sync offset** (deferred) benefits scrub and audio first; a
  playback engine would inherit it, not replace it.

## Recommendation: **defer**

Build it, if ever, only after real triage sessions show the still-frame scrub
failing to answer questions that motion would (stutter/cadence artifacts are
the only clear case found). Rationale:

1. The two questions this app exists to answer for videos — same footage?
   same soundtrack? — are answered today, from cache, with zero new
   dependencies.
2. Every approach either explodes the dependency/packaging surface
   (GStreamer, ffmpeg-next) or hands us the full A/V-sync problem to hand-roll
   and maintain (ffmpeg pipe) for a feature whose incremental triage value is
   narrow.
3. The OS player remains one keystroke away for actually *watching* a clip,
   full speed and hardware-decoded — better than anything we would ship.

**If/when built**: take the ffmpeg-pipe + wgpu route (no new dependencies,
consistent degradation), muted-by-default, proportional clock with the manual
offset, flicker as presentation-swap. Rough cost estimate: ~2–3 weeks of
focused work for a robust two-pipe engine (pacing, seek, pause, EOF, error
paths, tests with generated clips), plus ongoing maintenance of the sync loop —
an order of magnitude more engine code than the whole filmstrip/scrub slice.

## Out of scope confirmed

No production code, no dependency, and no behavioral change ships with this
ticket; `external::open` remains the way to watch a clip.

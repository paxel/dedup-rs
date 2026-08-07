# 05 — Feasibility report: in-app synced video playback

**What to build:** A **written feasibility report** (research deliverable, not code)
assessing whether the shared viewer should ever play video *in-app* — two clips
playing synced with an A/B flicker, muted by default and one side carrying sound on
"normal play" — instead of only handing off to the external OS player. The report
gives us enough evidence to decide, without committing any implementation.

This is a **research** ticket. It writes a document, not production code; it is
grabbable by a research agent but is not a build slice.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] Survey candidate decode/upload approaches — e.g. gstreamer, ffmpeg-next, a
      manual ffmpeg-pipe + wgpu texture upload — with pros/cons for this app.
- [x] Describe the interaction model: two-clip sync, A/B flicker, mute-by-default vs
      "normal play" with sound, and how it would sit in the existing Video tab.
- [x] Assess new dependency and build-system cost (note ALSA + ffmpeg are already
      required) and any platform/runtime risks.
- [x] State how it would relate to the existing proportional scrub and the deferred
      manual sync offset.
- [x] End with a clear recommendation (build / defer / drop) and a rough cost.
- [x] Deliverable is a reviewed Markdown document under this feature directory; no
      production code and no new dependency is added by this ticket.

## Comments

- 2026-08-07 — Report delivered: `.scratch/video-diff/feasibility-in-app-video-playback.md`. Recommendation: defer; if ever built, use the ffmpeg-pipe + wgpu route.

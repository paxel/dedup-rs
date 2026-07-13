# Duplicates tab

![Duplicate Management tab](../screenshots/duplicates_tab.png)

Find exact or perceptually similar duplicates across one or more repositories, review them
with real previews, and delete the worse copies — batched per repo, always confirmed unless
you turn on Quick Delete. This is the GUI equivalent of `repo dupes`, with a full review
workflow on top.

## Repo bar

One chip per registered repository:

- Click the **name** to toggle whether that repo is included in the next FIND. Excluded
  repos are skipped entirely.
- Click the **padlock** to toggle read-only: a closed lock means files in that repo are
  protected from deletion (never preselected, never marked, even by auto-resolve); an open
  lock means they can be deleted like any other file.
- **REFRESH** reloads the repository list (e.g. after adding one in the Repositories tab).

## Mode, threshold, and FIND

- **DUPLICATES** — exact, byte-for-byte matches (same size and BLAKE3 hash). Fast, no false
  positives.
- **SIMILAR** — perceptually similar images and videos: re-saves, re-encodes, or crops that
  don't hash identically but look alike. Reveals a **similarity threshold** slider (50–100%,
  `similarity % = (1 − hamming distance / bits) × 100`); lower catches more — and riskier —
  matches, 100% is bit-identical, and ≥99.5% is labeled "identical" since it's visually
  indistinguishable in practice. The threshold persists across launches.
- **FIND** searches every included repo per the selected mode.

**QUICK DELETE** gives every group its own DELETE NOW button that deletes marked files
immediately, no confirmation — turn it off to go back to confirming every batch.

Below the results: **AUTO-RESOLVE REST** marks every non-best copy for deletion (skipping
read-only repos) so you only have to review the marks, and **DELETE MARKED (n)** deletes
everything currently marked, batched per repo in one transaction, behind a confirmation.

## Result groups

Results page 50 groups at a time (← / → to navigate). Each group shows a header (copy count,
size, reclaimable bytes) and one card per file:

- Thumbnail (click to open the [lightbox](#lightbox)), path, repo, size, dimensions, mtime.
- **from `<repo>`** — if this file's provenance is known (it was copied/synced in from
  another repo).
- Audio files get inline **PLAY/PAUSE** + a seek bar (see [Audio preview](#audio-preview)).
- The best copy is starred **BEST**.
- **KEEP / DELETE** toggles this copy's mark. Nothing is deleted until you press DELETE
  MARKED (or DELETE NOW under Quick Delete).
- A file in a read-only repo shows a **read-only** badge instead of KEEP/DELETE —
  right-click or long-press it to unlock just that one file (a deliberately inconvenient
  escape hatch, never bulk-set, reset on the next FIND).
- Right-click anywhere on a card for **OPEN** (hand the file to the system's default app) and
  **SHOW IN FOLDER** (reveal it in the file manager) — the full-fidelity escape hatch for
  any file type, and the designated way to actually play a video full-screen.

## Lightbox

Click any image or video thumbnail to open the full-window lightbox.

![Lightbox in A/B compare mode](../screenshots/lightbox_compare.png)

**Images**: mouse-wheel zooms around the cursor, drag pans. `F` fits to window / `1` shows
true pixels (100%, one screen pixel per image pixel). `←`/`→` step through the group's other
copies. `Del`/`K` toggles the mark on the shown file (respecting read-only). `Esc` or CLOSE
exits. Full-resolution decoding runs off the UI thread with an aggressively-capped texture
cache, so even a very large photo never freezes the interface — the thumbnail shows upscaled
until the full image lands.

**Videos**: the lightbox is a scrubbable filmstrip instead of a zoomable image — ten
evenly-spaced stills, with the frame under the cursor enlarged, so you can identify a clip
and judge its quality without full playback. Frames are extracted with `ffmpeg` on demand and
cached; without ffmpeg the card and strip fall back to a placeholder. Full playback is the
OPEN (external app) path.

### A/B compare

Press `C` (or the COMPARE button; images only, needs ≥2 copies in the group) to pit the shown
copy against the group's best copy, with a shared, resolution-independent zoom/pan:

- **SIDE BY SIDE** (default) — both panes at once, each labeled A/B.
- **FLICKER** — one full-window pane; `space` swaps between A and B in place, the fastest way
  to spot compression artifacts.
- The bottom metadata strip shows both files' size and dimensions, with the larger value
  highlighted.
- **MARK A** / **MARK B** toggle either copy's deletion mark independently; `Del`/`K` marks B
  (the compare candidate) while comparing.
- **EXIT COMPARE** (or `C` again) returns to the single-image view.

## Audio preview

Audio duplicate cards get an inline **PLAY/PAUSE** button, an elapsed/total readout, and a
seek bar — confirm two "similar" tracks are actually the same recording (and which sounds
better) without leaving the app or opening an external player. Only one file plays at a time;
starting another replaces it. Playback survives scrolling and stops automatically when you
switch away from the Duplicates tab. Requires ALSA on Linux at build time (see the README);
with no audio device at runtime, the controls still render and playback is simply a no-op.

## Video preview

Video duplicate cards show a real still frame (sampled mid-timeline) instead of a generic
icon, using the same evenly-spaced grid the lightbox filmstrip uses — so the card's frame is
reused there instead of being extracted twice. Without ffmpeg on `PATH`, the card falls back
to the placeholder.

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

Click any thumbnail to open the full-window lightbox — including the typed placeholder a
document, archive or other non-visual duplicate shows instead of a picture, which opens on
its [Text](#representation-tabs) preview.

![Lightbox in A/B compare mode](../screenshots/lightbox_compare.png)

### Representation tabs

A file can be looked at in more than one way, so the lightbox is tabbed by *representation*.
The tab bar offers every representation at least one of the two compared files has —
**Overview**, **Image**, **Video**, **Audio**, **Metadata**, **Text** — and nothing else: a
photo has no Audio tab, an untagged FLAC no Metadata tab. Selecting a tab draws only the
column(s) whose file supports it, always left (A) against right (B), never stacked. If you
step to a copy that lacks the current representation, the lightbox drops back to Overview.

**Overview** is where a lightbox opens: repo badge, thumbnail, path, size/dimensions/duration,
mtime and mime for each side, the DELETE / DELETE A / DELETE B mark pill, and the COMPARE
button that hands off to the native view. While comparing, `< i / N >` cycles B through every
*other* copy in the group.

**Metadata** shows the file's tags. For MP3/WAV/AIFF that is the ID3 editor: EDIT TAGS opens
Title/Artist/Album/Year/Track/Genre for that copy, the `>` beside a field offers the value any
other copy in the group carries (adopt the best one), and SAVE TAGS writes only the tags — the
audio itself is untouched. `T` from the audio view jumps straight here. A file in a read-only
repository is shown but never editable. For images the tab shows the EXIF capture facts
(camera, taken) as recorded; they are not edited here.

![The Metadata tab, editing one copy's ID3 tags against another's](../screenshots/lightbox_metadata.png)

**Text** is the representation for everything that is not image, video or audio — documents,
archives, anything a thumbnail cannot describe. It shows the head of each file (the first
64 KB) as text, or as an offset/hex/ASCII dump when the file is not text, so two same-sized
"duplicates" can still be told apart by eye. Long files scroll inside their column.

![The Text tab, a markdown file beside a binary one](../screenshots/lightbox_text.png)

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

Press `C` (needs ≥2 copies in the group, and a group with a visual — images or videos) to pit
the shown copy against the group's best copy, with a shared, resolution-independent zoom/pan:

- **SIDE BY SIDE** (default) — both panes at once, each labeled A/B.
- **FLICKER** — one full-window pane; `space` swaps between A and B in place, the fastest way
  to spot compression artifacts.
- **Videos compare scrubbed in sync**: a shared filmstrip sits below the two panes, and
  clicking a still moves **both** sides to that frame — the same position on each clip's
  ten-still grid (the same time-*fraction*, not the same absolute timestamp if their durations
  differ) — so you can line up the same moment and spot a re-encode or crop.
- The bottom metadata strip shows both files' size and dimensions, with the larger value
  highlighted.
- **MARK A** / **MARK B** toggle either copy's deletion mark independently; `Del`/`K` marks B
  (the compare candidate) while comparing.
- **EXIT COMPARE** (or `C` again) returns to the single view.

![Two clips in A/B compare with the shared frame scrubber](../screenshots/video_compare.png)

Groups with nothing to show (duplicate PDFs, text, or other non-visual files) have no compare —
`C` does nothing there.

## Audio preview

Audio duplicate cards get an inline **PLAY/PAUSE** button, an elapsed/total readout, and a
seek bar — confirm two "similar" tracks are actually the same recording (and which sounds
better) without leaving the app or opening an external player. Only one file plays at a time;
starting another replaces it. Playback survives scrolling and stops automatically when you
switch away from the Duplicates tab. Requires ALSA on Linux at build time (see the README);
with no audio device at runtime, the controls still render and playback is simply a no-op.

In the audio lightbox the player header carries the same mark pills as every other view:
**DELETE** for a single copy, and **DELETE A** / **DELETE B** while comparing, each toggling
only its own copy — so a copy can be marked without leaving the comparison. A copy in a
read-only repository shows a disabled, struck-through `… (Protected)` pill instead.

Stepping to another copy with `←`/`→` keeps your transport state: if playback was running it
continues on the new copy, and if you had paused it stays paused — with the newly shown copy
loaded, so pressing play resumes the file you are actually looking at.

## Video preview

Video duplicate cards show a real still frame (sampled mid-timeline) instead of a generic
icon, using the same evenly-spaced grid the lightbox filmstrip uses — so the card's frame is
reused there instead of being extracted twice. Without ffmpeg on `PATH`, the card falls back
to the placeholder.

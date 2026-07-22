# dedup-rs — Improvement Roadmap

## Remaining / deferred work

- **Light theme toggle** (deferred by decision 2026-07-08): requires converting
  `theme.rs` constants to a runtime palette across all views. Still dark-only.
- **Performance at scale**: banded grouping, staged pipelines, and the multi-reference
  diff's merged content index are fine at ~10⁵ files; revisit content-index memory
  (`HashMap<(u64,[u8;32]), _>` across all references) and timeline streaming at 10⁷.
- **Testing discipline** (standing practice): every GUI feature ships with kittest
  geometric tests + an `--ignored` render snapshot; every core feature with temp-repo
  integration tests; store format changes must include a legacy-decode test (pattern:
  `store.rs::v1_entries_decode_and_flag_images_stale`).

## A usability
- **review pane per-row diff button** opening the lightbox diff view (for PURGE
  no diff button). Deferred to *Unified lightbox & compare* below — it is the same
  "compare two textures" surface. The rest of this item is done: the review rows now
  carry a thumbnail + size / dimensions / duration / mtime via the shared
  `media_cell` widget (also used by the Duplicate cards), so a new file type extends
  in one place.
- sync repos. it seems the grouping of repos belongs to the first tab
  - **Done (slice 1):** group management now lives on the Repositories-tab repo cards —
    MAKE MAIN (repo → group), SINK INTO an existing group, per-group ADD REPO (clone of
    the main at a new path), UPDATE ALL, UNGROUP, a per-group mode pill, collapsible
    SINK(S), and SINK OUT. Reuses the existing per-group `SyncGroup` model + DUPLICATE flow.
  - **Done (slice 2):** push mode is now **per sink** (`SyncGroup.mode` → `SyncSink { repo,
    mode }`, with a version-2 registry migration that gives legacy groups' sinks the old
    group mode). Each sink carries its own MIRROR/ADD ONLY pill on the Repositories tab and
    the Sync Groups tab; the empty-main mirror guard now triggers when any sink mirrors.
  - **Done (slice 3):** the operational tabs (Transfer, Grooming, Duplicates, Browse)
    hide sinks from their repo pickers — only mains and ungrouped repos are offered, via
    `Store::sink_repo_names`. A previously-selected repo that becomes a sink is dropped on
    the next tab show (each view already validates its selection against its repo list).
    The Repositories and Sync Groups tabs still show all repos, since they manage groups.
  - **Done (slice 4, final):** the tab is renamed **Repo Sync** with two panes — GROUPS
    (unchanged group push) and COMPARE, a read-only overview that diffs every repo against a
    chosen reference (unique / shared / missing content counts, via `sync_group::diff_overview`
    off-thread). **A#3 is complete.**

## Unified lightbox & compare

Today there are four overlapping preview/compare surfaces: the Duplicate tab image/video
lightbox (`dupes_view` + `lightbox.rs`), a *separate* `dupes_view::audio_lightbox`,
`diff_inspect.rs` (a weaker static two-pane for DIFF conflicts — no zoom/pan/flicker/
waveform), and the `browse_view` preview dock. Collapse them into one lightbox.

Guiding principle: we are not judges, we provide flexible tools. Any two sides that can
produce *an image* — a photo, a video frame, a spectrogram — can be compared side by side
and flicker-swapped, regardless of mimetype. If a side has no visual, compare disables
itself.

The enabler is already in place: every visual reduces to a `ColorImage`/`TextureHandle`
(image decode, video frame extraction, `waveform::spec_image`). Compare is then just
"compare two textures."

Work:
- Define a "previewable" abstraction: yields an optional texture + facts (+ the caller's
  own per-item actions). Image / video-frame / audio-viz implement it; text/binary yields
  `None`.
- Generalise `CompareState.other: usize` (a duplicate-group index) into an abstract B
  source, so the two sides can be different repos or types (what DIFF and cross-type
  compare need).
- Gate compare on `both sides yield a texture` — nothing to compare ⇒ features disabled.
- Keep the action strip caller-supplied: Duplicate marks-for-deletion/best, DIFF does
  copy/rename/overwrite/delete, Browse does tags. Unify the viewer, not the actions.
- Route DIFF COMPARE through it and delete `diff_inspect.rs`; fold in `audio_lightbox`.
  Video and cross-type compare (currently absent) then fall out for free.
- Decide the amplitude waveform: it is painter-drawn, not a texture (only the spectrogram
  is). Add a `waveform::wave_image()` renderer, or always compare audio as spectrograms.

Sequence: (1) previewable abstraction + generalise `CompareState`; (2) route DIFF, delete
`diff_inspect`; (3) fold in audio; (4) enable video/cross-type compare.

**Done (slice 1):** `lightbox::full_texture(&FileFacts, …)` is the viewer-agnostic
texture resolver (generalising `dupes_view::lightbox_texture`), and `CompareState.other:
usize` is now `CompareState.b: FileFacts` — the abstract B *rendering* source. The
Duplicate image + audio lightboxes resolve B's texture from those facts and re-find B's
group member (for this tab's mark actions) by on-disk path; when a later slice points B
outside the group, no member matches and the caller supplies the actions. Behaviour is
unchanged.

**Done (slice 2):** DIFF COMPARE (the Transfer tab's BY PATH conflict inspector) now renders
through the shared `lightbox::draw_compare` + `CompareState` primitives — the same
zoom/pan/flicker the Duplicate lightbox has — instead of its own hand-rolled two-pane preview.
`diff_inspect.rs` is **deleted**. The DIFF caller (`transfer_view::DiffCompare`) keeps its own
off-thread decode (which handles **both** images and video stills, so no video regression — it
does *not* route through the image-only `full_texture`), holds the two sides as
`FileFacts` + `repo`/`rel_path` (action identity), and maps CLOSE/OVERWRITE/DELETE onto the
same `BoardAction`s the row offers. Flicker/zoom compare turns on only once *both* sides
have actually decoded a texture; a side that yields none — a non-previewable type, or a
decode that came back empty (e.g. video with no ffmpeg) — settles to a "no preview" note and
keeps compare disabled ("if a side has no visual, compare disables itself"), so a failed
decode never spins "decoding…" forever. Still to do: slice 3 (fold in `audio_lightbox`) and
slice 4 (video / cross-type compare).

**Done (slice 3):** audio is now a texture-yielding *previewable*. A single resolver,
`dupes_view::previewable_texture(&FileFacts)`, dispatches by kind — image/video via
`lightbox::full_texture`, audio via its spectrogram (`waves` → `spec_texture`), text/binary →
`None` — and the audio lightbox builds its spectrogram texture through it, so an audio file
yields a `(texture, size)` the same way an image does. (That audio→texture production is
verified by code inspection + the unchanged `spec_texture`/`spec_image` path and the manual run
step, not by the suite: no headless fixture supplies a decodable audio file, so the resolver's
audio arm has no automated production test — only its dispatch is unit-tested, and the
image-size assertion is the one that discriminates.) Per the user's "texture-enable, keep
compare" decision, audio's own compare *rendering* is deliberately unchanged: the two waveforms
still stack vertically on a shared time axis, drawn full-width with the click-to-seek cursor and
ID3 tag panels — `draw_compare`'s horizontal, letterbox-fit, zoom/pan geometry would degrade
that, so audio was **not** routed through it. `waveform::wave_image()` (amplitude envelope → a
texture) is unneeded under this decision and was not added.

**Done (slice 4, final — epic complete):** video dup groups now A/B compare, **scrubbable in
sync**. `previewable_texture` gained a `video_frame: Option<usize>` param and a correct video arm
(`thumbs.get_video` — the slice-3 doc wrongly routed video through image-only `full_texture`);
the image lightbox's `a_tex`/`b_tex` resolve through it, a video at the shared `scrub` index. A
shared filmstrip (extracted into `draw_filmstrip`, reused by the single-video view) sits below
the two compare panes; clicking a still sets the one `LightboxState::video_frame`, so both sides
move to the same fixed-grid fraction (not the same absolute timestamp when durations differ).
Compare *entry* is gated on mime-previewability (`a_is_image || a_is_video`) — video turns on,
non-visual dup groups (duplicate PDFs/text) turn off ("if a side has no visual, compare disables
itself"), and the single-view hint drops zoom/compare for those. Per the user's "scrubbable in
sync" choice (over the simpler single-frame option). Automated coverage is the compare-entry gate
+ the video-compare branch laying out without panic; actual frame *production* needs ffmpeg + a
real file, so it is inspection + the manual step, not suite-verified — the scrubbable video
compare has not itself been run (headless has no ffmpeg; the manual pass is its acceptance test).

On "cross-type": the *resolver* (`previewable_texture`) is type-agnostic, so the machinery for
comparing two different-typed visuals exists — but **no surface actually does it**. Dup groups
are same-kind; DIFF (slice 2) uses its own decode and shows a placeholder for a non-visual side
rather than comparing image-vs-spectrogram. So "epic complete" means the four unification slices
landed and video A/B compare works — not that cross-type visual compare is a reachable feature
(it has no caller). **The *Unified lightbox & compare* epic is complete** in that sense.

**The deferred review-pane per-row diff button is the immediate follow-on** (it opens the
slice-2 DIFF compare surface from a review row).

## Engineering backlog
- **Per-sink main re-read.** `plan_group_sync`/`run_group_sync` re-open the main and
  re-run `collect_source_entries` over its whole index once per sink. Collect the main's
  entries and content-key set once before the loop. Touches the shared `plan_sync`/
  `diff_sync` signatures (also used by Transfer, CLI), so it needs care.
- **Sink baked into `ReviewRow.target_path`.** The sync preview stores
  `format!("{sink}: {rel}")`, so sorting by target path sorts by sink name and a rel-path
  containing ": " is ambiguous. Give `ReviewRow` a repo/scope field rendered as its own
  column.
- **`diff_board` micro-efficiency:** `totals(rows)` computed twice per frame; the paging
  strip duplicates `review.rs`'s; `sort`'s comparator clones a whole `DiffFile` per
  comparison to read one field. Cosmetic at current scale.
- **Empty-walk on scan is warn-only.** A scan that finds no files where the index held
  some marks everything missing (a legitimate emptying must still propagate). If the index
  loss proves annoying, add a confirmation (GUI) / `--force` (CLI) before a scan may mark
  *every* entry missing.

## Recognition & extensibility  *(far future)*
- Face recognition and object recognition for photos/images.
- VLA tagging of files to topics; word clouds for documents.
- MP3 tag handling; metadata extraction for all known formats.
- Plugin support for new formats; an API to externalise features.

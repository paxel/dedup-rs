# dedup-rs — Improvement Roadmap

> [!NOTE]
> A detailed, refined TODO roadmap synthesizing all QA findings, bug fixes, UI tab overhauls, and engineering backlog items is available in [`ai/roadmap.md`](file:///home/axel/develop/dedup-rs/ai/roadmap.md).

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
- Some clarifications. What I thought of how comparing lightbox should work:
  - We define a series of interfaces that the lightbox display can call on a instance
  - eg
    - get image representation (including interface for saving changes, which might or might not be allowed for this kind of media)
    - get audio representation
    - get meta data representation (e.g. idv3 including the saving changes)
    - get mark interface to read and set the mark status
    - get the dedup data, aka, last modified, mime, current repo, path
    - get textual representation
    - get video representation
  - when two files are lightboxed, the top tab shows the available represntations of at least 1 of A and B
  - when selected only the sides that have a represntation are shown, and can be compared against each other. ALL are shown left and right. no top and bottom
  - when both sides exist a compare button is presented that goes in the comparison mode where audio can be played gapless on change, curves can be overlayed and spectograms are shown, or for images the zoom, and rotate and such
  - we might introduce and improve the comparisons later and adapt the interface
  - so the first step of comparison, the one with the tab on top should always be the intro to comparison. and it should be very light, it basically should only ask the file representation for its dedup/compare features and paint them. and offer selection for the two columns in case of more than three files. the comparable data is handed off to the compare fw, and if modification is allowed the modified data is handed back to the file to save it. with the "overwrite or create new" solved already. the file defines if overwrite is possible and if create new is possible, if neither no safe or save as is offered in the ui.
  - for every feature that we current offer we might need to invent a interface on the data representation.
- the repo sync page:
  - no group generation exists here. you only can select left and right repo and then compare them. and for that we show the compare pane that is currently in transfer. and this compare pane shuld look like in the dedup review, including the compare button for 2 files with same path but different hash


































































## Recognition & extensibility  *(far future)*
- Face recognition and object recognition for photos/images.
- VLA tagging of files to topics; word clouds for documents.
- MP3 tag handling; metadata extraction for all known formats.
- Plugin support for new formats; an API to externalise features.

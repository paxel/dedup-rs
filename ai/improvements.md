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
- for the review pane we need more infos. the filename columns need to contain basically the same infos as the duplicate tabs group items. thumbnail, audio duration, size, etc. extremely nice would be a diff button between both that opens a lightbox diff view
  - for purge no diff button but still the info panel.
  - these type based info panels should be in a way that we can easily extend them in the future
- sync repos. it seems the grouping of repos belongs to the first tab
  - repositories get a "main repo button. if clicked they are converted to repo group. getting an elbow and and an add repo button.
  - the add repo button is for adding new repos. they inherit everything of the main repo but the path
  - existing repos get a "sink" button. which when clicked offers existing groups to be added to
  - the main group gets a sub elbow sinks, that can be colapsed
  - the main group gets a update all button that updates all repos of the group
  - each sink has a toggle pill mirror to select if the repo should be mirrored or just copied to
  - in all transfer groom etc views only the main groups are offered
  - group sync becomes repo sync where groups can be synced, but also ALL repos diffed vs another

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

# dedup-rs — Improvement Roadmap

Consolidated 2026-07-30 from `ai/improvements.md` + `ai/roadmap.md` + `ai/qa.md` (now
deleted — this is the one file). Every item below was verified against current source,
not just against the old docs: several previously-`[ ]` items in `ai/roadmap.md` turned
out to already be implemented and are dropped rather than carried forward. See the chat
history / `git log` for the removed content if you need the "why" behind a closed item.

## Active batch

Eleven of the items below are specced and ticketed in
[`.scratch/backlog-clearance/`](../.scratch/backlog-clearance/spec.md) — one sequential
agent, fixed order, tickets `01`–`11`. Light theme and performance-at-scale are the only
open items deliberately left out.

## Decisions resolved (2026-07-30/31)

- **Audio comparison is spectrogram-only.** The amplitude waveform is painter-drawn rather
  than a texture, and compare works on textures. No `waveform::wave_image()` will be added;
  amplitude stays available in the native audio view with its existing toggle. Accepted
  consequence: entering compare from the amplitude view changes the visual.
- **The native audio compare header gains inline `DELETE A`/`DELETE B` pills**, in addition
  to Overview-based marking — chosen so marking does not differ by media type, matching the
  image compare header. Ticket `06`.
- **A scan that would mark every indexed entry missing must be authorised**: confirmation in
  the GUI, `--force` on the CLI. The existing warning stays but is no longer the only
  defence. Ticket `07`.
- **DIFF compare is routed through the shared lightbox** and `DiffCompare` is deleted, rather
  than bolting a spectrogram onto the second compare surface. Ticket `11`.

## Open work (verified against source, 2026-07-30)

- **Light theme toggle** (deferred by decision 2026-07-08): requires converting
  `theme.rs` constants to a runtime palette across all views. Still dark-only.
- **Performance at scale**: banded grouping, staged pipelines, and the multi-reference
  diff's merged content index are fine at ~10⁵ files; revisit content-index memory
  (`HashMap<(u64,[u8;32]), _>` across all references) and timeline streaming at 10⁷.
- **Review-pane per-row compare button.** Grooming board rows (`grooming_view.rs`) only
  ever get `Cmd::Apply`/`Cmd::Hide` — there is still no way to open a compare/diff view
  from a PURGE/DEDUPE/ORGANIZE row, unlike DIFF rows which get `Cmd::Compare`. This is
  the direct follow-on now that the unified board (below) has landed.
- **Per-sink main re-read.** `plan_group_sync`/`run_group_sync` (`sync_group.rs`) still
  call `plan_sync` once per sink, each of which re-collects the main repo's entries from
  scratch. Collect the main's entries and content-key set once before the loop. Touches
  the shared `plan_sync`/`diff_sync` signatures (also used by Transfer, CLI), so it needs
  care.
- **Filter negation (`!pattern`)**: `dedup_core::filter::FileFilter` has no negation
  support. Add syntax (e.g. `!*.mp3`, `!/cache/`) plus a "Negate (NOT)" checkbox in
  `filter_ui.rs`.
- **Filter case-sensitivity toggle**: no `case_sensitive` field on `FileFilter` yet. Add
  the field and an `Aa` toggle in `filter_ui.rs`.
- **Auto-refresh repo stats on tab switch**: `app.rs`'s `Tab::Repositories` match arm is
  still empty on tab-switch — navigating to Repositories after a delete does not refresh
  counts/free space from `redb` until a manual reload.
- **DIFF filename-level diff highlighting**: BY HASH's differing-name list has no
  character-level highlight of what differs between names.
- **DIFF board batch header actions**: no "rename all remaining / delete all remaining /
  copy all missing" bulk actions for large DIFF row counts.
- **Audio: Play/Pause state retention.** No code checks whether playback was paused
  before switching to "Next" — playback state is not preserved across a file switch.
- **Audio: switcher freeze (>2 files) — needs a regression test.** `other_member_indices`/
  `format_other_switcher_label` (added for the Overview cycler) plausibly already fix
  this, but per the prior pass's own note, neither this nor the item below has actually
  been run/tested since.
- **Audio: ID3 tag sync glitch — needs a regression test.** Same caveat: the index-mapping
  fix likely already covers "tag shown every second file," but it's unverified.
- **Transfer DIFF compare is a second, weaker compare surface.** *Corrected 2026-07-31 — the
  previous entry here understated this, and the old roadmap's "Unified lightbox & compare
  epic is complete" claim was wrong.* `diff_inspect.rs` was deleted, but its logic was
  re-created as `DiffCompare` in `transfer_view.rs:3426`. `DiffSide::previewable()` is
  literally `is_image() || is_video()` (`transfer_view.rs:3376`), so comparing two MP3s from
  a DIFF row yields `no preview for audio/mpeg`. It has its own decode threads and texture
  slots, and no tabs, metadata, text, spectrogram or gapless flip. The QA complaint ("compare
  of two audio is completely broken… obviously not reused from duplicates view") is still
  literally true. Fixed by ticket `11`, which deletes `DiffCompare`.

## Standing practice

- **Testing discipline**: every GUI feature ships with kittest geometric tests + an
  `--ignored` render test that writes a PNG to look at; every core feature with temp-repo
  integration tests; store format changes must include a legacy-decode test (pattern:
  `store.rs::v1_entries_decode_and_flag_images_stale`). The pixel-diff snapshot test was
  removed on 2026-07-28: its baseline was gitignored and so never committed, making it
  unpassable on any machine but the one that last generated it — and being `#[ignore]`d,
  the drift went unnoticed for 22 days. Lesson carried forward from the lightbox/board
  work: **a label-query-only kittest test does not catch layout/overlap bugs — render
  it.**

## Recognition & extensibility *(far future)*

- Face recognition and object recognition for photos/images.
- VLA tagging of files to topics; word clouds for documents.
- MP3 tag handling; metadata extraction for all known formats.
- Plugin support for new formats; an API to externalise features.

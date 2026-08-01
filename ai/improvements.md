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

## Decisions resolved (2026-07-30 – 2026-08-01)

- ~~**Three viewers for one job.**~~ Done 2026-08-01 — the
  [`.scratch/lightbox-redesign/`](../.scratch/lightbox-redesign/spec.md) epic is complete,
  tickets `01`–`05`. `compare_view::DiffCompare` is the one viewer; every caller (Duplicates,
  Browse, review boards, DIFF) opens it with its own pool and its own actions (marks for
  Duplicates via `set_marks`/`DiffPick::ToggleMark`, board commands for DIFF), and the
  Duplicates tab's tabbed lightbox, its separate audio viewer and the Browse single-image
  viewer are **deleted** — pinned by
  `compare_view::tests::nothing_in_the_crate_references_the_deleted_viewers`, which scans the
  crate for their identifiers. The five audio regression tests survived with their assertions
  unchanged (gapless pair flip, paused stepping, per-copy tags, four-copy cycler, independent
  marks). Deliberately not carried over, recorded in the CHANGELOG: the waveform/spectrogram
  toggle and the video filmstrip scrubber. (The in-viewer image edit save was initially
  dropped too, then restored the same day at the user's request — as a shared-viewer feature
  with new semantics: saves keep the file's modified time or stamp it from the EXIF capture
  date, and an overwrite re-indexes the file at once via `update::refresh_file_entry`,
  because a kept timestamp makes the change invisible to the (size, mtime) scan skip.)
  Pool identity is the **absolute path** (cross-repo duplicates share their relative path),
  and a stepped side re-decodes (`refresh_side`) — keyed caches died with the old viewers.
- **Audio comparison is spectrogram-only.** The amplitude waveform is painter-drawn rather
  than a texture, and compare works on textures. No `waveform::wave_image()` will be added;
  amplitude stays available in the cards' inline preview. (The old audio view's toggle left
  with that view, 2026-08-01.)
- ~~**The native audio compare header gains inline `DELETE A`/`DELETE B` pills**~~. Done
  2026-07-31 (ticket `06`), built from the shared `mark_pill` helper so labels and the
  protected state cannot drift from the image header. The mark keys and markability are
  resolved *before* the drawing closure (which cannot borrow `self` again) and the toggles are
  collected and applied after, which is the deferred mechanism the old NOTE said was missing.
- ~~**A scan that would mark every indexed entry missing must be authorised**~~. Done
  2026-07-31 (ticket `07`): `update_repo_authorized(.., allow_empty)` carries the decision;
  `update_repo` keeps its signature and delegates with `false`, so all ~58 call sites were
  untouched. The refusal (`UpdateError::WouldEmptyIndex`) is raised *before* anything is
  marked, so a refused scan leaves the index byte-for-byte as it was. CLI: `--force`. GUI: the
  worker reports `JobOutcome::UpdateWouldEmpty` and a confirmation offers SCAN ANYWAY, which
  re-queues the job as `JobKind::UpdateForced`.
- **DIFF compare is routed through the shared lightbox** and `DiffCompare` is deleted, rather
  than bolting a spectrogram onto the second compare surface. Ticket `11`.

## Open work (verified against source, 2026-07-30)

- **Light theme** — specced in [`.scratch/light-theme/`](../.scratch/light-theme/spec.md),
  ticket `01` landed 2026-08-01. The palette is now a `theme::Palette` value installed in a
  **`thread_local`** (not a `static` — the test suite runs in parallel and a shared palette
  would let light/dark assertions race), read through accessors (`theme::text()`), with the
  raw values private so new code cannot bypass the active palette. `theme::apply` takes the
  palette to install. Still dark-only: `DARK` is the only palette, and the rendered board
  screenshot is byte-identical to before, so the appearance provably did not change.
  Remaining: tickets `02`–`05` (light values, preference wiring, Settings control, identicon)
  — **unblocked** since 2026-08-01, when the lightbox-redesign epic landed.
- **Performance at scale**: banded grouping, staged pipelines, and the multi-reference
  diff's merged content index are fine at ~10⁵ files; revisit content-index memory
  (`HashMap<(u64,[u8;32]), _>` across all references) and timeline streaming at 10⁷.
- ~~**Review-pane per-row compare button.**~~ Done 2026-07-31 (ticket `10`). DEDUPE rows carry
  the counterpart they were missing (`diff_print` already returned
  `DiffItem::Equal { reference_path }`; the preview was discarding it), so a row shows the
  surviving copy on the right with the pool named in the header — and they now offer
  `Cmd::Compare`, opening the shared `compare_view`. Only rows with a counterpart offer it:
  PURGE/PRUNE are one-sided, and ORGANIZE's two sides are the same file at two paths.
- ~~**Per-sink main re-read.**~~ Done 2026-07-31 (ticket `02`): `diff.rs` gained a
  `SourceView` (the source's filtered entries + root + name, collected once) plus
  `plan_sync_from`/`diff_sync_from` taking one. `plan_sync`/`diff_sync` keep their exact
  signatures and now just collect a view and delegate, so Transfer and the CLI were
  untouched; only `plan_group_sync`/`run_group_sync` collect once and share across sinks.
  The empty-main mirror guard still runs *before* collection, so the refusal is unchanged.
- ~~**Filter negation (`!pattern`)**~~ and ~~**case-sensitivity toggle**~~. Done 2026-07-31
  (ticket `01`): negation is per *condition* (`!name:*.mp3`, `!mime:image`) rather than per
  pattern, which composes uniformly across every facet and needs no value escaping — `!` is
  only special at a condition boundary, so `name:!important` stays literal. Case mode is a
  whole-expression modifier (`case:insensitive`, the `Aa` toggle) carried down the match
  traversal, so it reaches inside negation and leaves size/date conditions alone.
- ~~**Auto-refresh repo stats on tab switch**~~. Done 2026-07-31 (ticket `03`): the
  tab-transition block moved into `DedupApp::sync_shown_tab`, where `Tab::Repositories` now
  calls `reload_all()` gated on `worker.active_count() == 0` — the same gate every other
  `reload_all` call site uses, since it opens each repo db. A busy frame is skipped safely
  because the running job's completion handler reloads anyway.
- ~~**DIFF filename-level diff highlighting**~~. Done 2026-07-31 (ticket `08`):
  `board::name_diff_ranges` computes the differing byte ranges from an LCS table (guarded at
  512 chars), `highlight_job` renders them as a `LayoutJob` with a highlighted background.
  Compares the *file name*, not the path, and pairs a side's names against the other side's in
  order — a name with no counterpart is left plain. An elided label falls back to plain text
  rather than painting offsets that no longer line up.
- ~~**DIFF board batch header actions**~~. Done 2026-07-31 (ticket `09`): a bulk bar above the
  DIFF board offers `COPY MISSING >` / `< COPY MISSING` / `RENAME ALL L` / `RENAME ALL R`,
  gated on the relations the *listed* rows actually hold. "Listed" means after the
  show-unchanged toggle and excluding hidden rows, so hiding is how a row is opted out.
  Confirmation states the exact count; `start_bulk` runs the plan off the UI thread, honours
  cancel, attempts every operation and reports successes and failures separately.
- ~~**Audio: Play/Pause state retention.**~~ Done 2026-07-31 (ticket `04`). The real defect was
  worse than the report: the nav branch acted *only* when playing, so a paused step left the
  **previous** file loaded — the lightbox showed one copy while play would resume another.
  `Player::load_paused` (a `paused` flag on `Cmd::Play`) now loads the newly shown copy and
  holds it, so transport state and the loaded file both follow the navigation.
- ~~**Audio: switcher freeze (>2 files)**~~ and ~~**ID3 tag sync glitch**~~. **Verified, not
  fixed**, 2026-07-31 (ticket `05`): both were already resolved as a side effect of the
  Overview cycler's index mapping, and now have regression tests on a **four-copy audio**
  group (the existing cycler test used images, and audio dispatches through `audio_lightbox`,
  so the path was genuinely uncovered). The cycler walks three distinct others and wraps,
  never showing the member count where the others count belongs; each copy reads back its own
  ID3 tags, with copies 1 and 3 asserted to differ — the exact reported symptom. No behaviour
  changed, so there is no CHANGELOG entry.
  Worth recording: arrow-nav *while comparing* deliberately flips which copy is audible rather
  than re-indexing A — re-indexing would collide A with B and force a reloading pause, which
  is the "freeze" the report described. Cycling B lives on Overview by design.
- ~~**Transfer DIFF compare is a second, weaker compare surface.**~~ Done 2026-07-31
  (tickets `10`/`11`). *The old roadmap's "Unified lightbox & compare epic is complete" claim
  was wrong:* `diff_inspect.rs` had been deleted, but its logic was re-created as a private
  `DiffCompare` inside `transfer_view`, whose `previewable()` was literally
  `is_image() || is_video()` — so two MP3s yielded `no preview for audio/mpeg`.
  Now: `DiffCompare` lives in its own `compare_view` module and renders through the shared
  `lightbox` helpers (`tab_kinds` / `draw_tab_bar` / `draw_columns` / `draw_metadata_column` /
  `draw_text_column`), so DIFF has a real representation tab bar — audio as a spectrogram
  (`waveform::spec_rgba`), documents as Text, tags as read-only Metadata. `draw_tab_bar` taking
  `&mut RepresentationKind` instead of a whole `LightboxState` is what made it shareable, and
  Grooming's DEDUPE rows reuse the same surface with no new viewer code.

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

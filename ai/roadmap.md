# dedup-rs — Comprehensive Refined Roadmap

This document serves as the master implementation specification for upcoming features, refactorings, bug fixes, and architectural overhauls in `dedup-rs`. It synthesizes past QA bug reports ([qa.md](file:///home/axel/develop/dedup-rs/ai/qa.md)), architectural evolution notes ([improvements.md](file:///home/axel/develop/dedup-rs/ai/improvements.md)), and user requirements into actionable, step-by-step TODOs for automated agents or developers.

---

## 1. Lightbox Architecture: Data Representation Interfaces [IN PROGRESS]

**Status (2026-07-26):** The representation scaffolding (§1.2 structs/enums) was built but never
wired into any view — the `[COMPLETED]` tag above was premature. Since then:
- **Overview screen (§1.3.3) is now built and wired into `lightbox_modal`** as
  `DupesView::draw_lightbox_overview`, gated on `state.active_tab == RepresentationKind::Overview`.
  Verified by rendering it (not just label-querying it) via
  `dupes_view::ui_tests::doc_screenshot_lightbox_overview` →
  `docs/screenshots/lightbox_overview.png` (single file, not comparing) and
  `lightbox_overview_compare.png` (A/B comparing). That render caught a real layout bug: the two
  columns used `compare_split` (absolute rects) inside a flow-based `horizontal_top`, so column B's
  content overlapped column A's and the cycler/EXIT COMPARE controls landed under B instead of
  below both columns. Fixed by using flow-based `allocate_ui_with_layout` per column (not
  `compare_split`, and not `ui.columns` either — that hardcodes `top_down_justified`, which
  stretches child widgets, e.g. the thumbnail's hairline-stroked rect, to the full column width).
  **Lesson for any further tab work in this file: a label-query-only kittest test does not catch
  layout/overlap bugs — render it.**
- §1.3.4's selector-label and index-mapping fixes are in via `other_member_indices`/
  `format_other_switcher_label`, directly verified both by unit tests and by the cycler shown in
  `lightbox_overview_compare.png` (`<1/3>` for a 4-file group, not `<3/4>`).
- **Still open:** the tab bar can navigate Overview ↔ the *native* view of A's own kind, but
  clicking a tab for a genuinely different representation than what's currently showing does
  nothing beyond flip `active_tab` — there's no actual per-kind content switch/renderer dispatch
  yet (§1.3.1–1.3.2). Metadata/Text tabs remain unadvertised (no renderer exists).
- **Overview is now the default tab** (§1.3.3: "the tab on top should always be the intro to
  comparison"). `LightboxState::new` sets `active_tab: RepresentationKind::Overview`; the earlier
  native-first default was a staging compromise while the screen was being built, now reverted.
  Updated the ~20 test call sites that constructed `LightboxState::new(...)` and depended on
  native-first (they now explicitly set `.active_tab` to the kind they're actually testing);
  rewrote `overview_tab_shows_facts_and_switches_back_to_native` (Overview-first is now the
  asserted behaviour, not something reached via `I`) and the setup half of
  `overview_cycles_b_through_others_with_correct_label`. All three doc-screenshot/render tests
  that build a `.compare` state directly (`doc_screenshot_lightbox_compare`,
  `doc_screenshot_video_compare`, `render_lightbox`) also needed an explicit native `.active_tab`,
  otherwise they'd now render Overview's two-column facts view instead of the full native
  side-by-side compare they're named for and documented from.
- **Audio-specific coverage of the new default**, added after review flagged that all six
  pre-existing audio tests bypass Overview by setting `.active_tab = Audio` directly, meaning the
  real "open an mp3 group → land on Overview → click Audio" path had never been rendered or
  tested — and audio dispatches through a genuinely different function (`audio_lightbox`, not
  `lightbox_modal`) once past Overview, so the image-only round-trip test didn't cover it:
  - `overview_audio_tab_switches_to_native_player`: Overview shows no native player controls by
    default on an audio group; clicking "Audio" switches to it (marker: the `SPECTROGRAM` toggle,
    which is unique to the native player — a plain `PLAY` query is ambiguous because the
    background dupe cards also carry their own inline play button).
  - `overview_compare_button_enters_audio_compare_with_gapless_flip`: entering compare via
    Overview's `COMPARE` button (as opposed to native `C`) is a different code path —
    `draw_lightbox_overview`'s `enter_compare` sets `state.compare` directly, bypassing
    `audio_lightbox`'s own `toggle_compare` branch — so the existing gapless-flip regression test
    (which only enters via `C`) didn't prove the Overview entry stays gap-free. It does: the
    paired-stream load in `audio_lightbox` is driven generically by `comparing` on the next play
    action, not by which branch set `state.compare`. Verified empirically, not just by reading.

### 1.1 Context & Problem Statement
Currently, preview and comparison logic is fragmented across `dupes_view.rs`, `lightbox.rs`, and `transfer_view.rs`. Visual comparison, audio waveforms, spectrograms, and ID3 tag editing are handled through ad-hoc conditional branches. 

We need a clean **Data Representation Interface** model where any file instance can expose one or more representations. The Lightbox UI becomes a tabbed viewer operating strictly on these representations.

### 1.2 Data Representation Traits & Structs
Target Files: [`crates/dedup-gui/src/lightbox.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/lightbox.rs), [`crates/dedup-gui/src/media_cell.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/media_cell.rs)

Implement representation descriptors for file instances:
- **`DedupDataRepresentation`**: Provides `rel_path`, `repo_name`, `size`, `mtime`, `mime_type`, and read-only status.
- **`ImageRepresentation`**: Provides decodable RGBA texture/image handle, dimensions, zoom/pan bounds, flicker swap support, and optional image mutation (rotate, crop) with saving capabilities.
- **`AudioRepresentation`**: Provides spectrogram `TextureHandle`, painter-drawn amplitude waveform, audio stream handle, seek position, and play/pause controls.
- **`MetadataRepresentation`**: Exposes ID3/EXIF tag fields (artist, title, album, year, track, comment) and read/write capabilities for saving tag changes back to disk.
- **`VideoRepresentation`**: Provides extracted filmstrip frame textures, video seek timeline, and play/pause state.
- **`TextBinaryRepresentation`**: Provides raw string preview or hexadecimal dump for non-media files.
- **`MarkInterface`**: Provides read/write access to duplicate deletion marks (`DELETE`, `DELETE A`, `DELETE B`, `PROTECTED`).

### 1.3 Lightbox UI & Navigation Flow
Target File: [`crates/dedup-gui/src/lightbox.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/lightbox.rs)

1. **Top Tab Bar**:
   - Inspect the available representations of Left (File A) and Right (File B).
   - Display tabs for all representations present in *at least one* file (e.g. `[Overview]`, `[Metadata]`, `[Audio]`, `[Image]`, `[Video]`, `[Text]`).
   - When a tab is selected, render ONLY the column(s) that support that representation.
2. **Two-Column Layout**:
   - Layout is strictly **Left (File A) vs. Right (File B)** side-by-side. Never stack top-and-bottom.
3. **Intro / Overview Mode**:
   - Default tab when entering Lightbox. Light weight rendering of basic facts (`DedupDataRepresentation`).
   - Presents file selector dropdowns for Left and Right columns when group size > 2.
   - If both Left and Right support the selected representation, render a prominent **"Compare"** button to enter full comparison mode (flicker/side-by-side for images, overlaid curves/spectrograms/gapless audio for sound).
4. **File Selection & Switcher (<A/N>) Logic**:
   - The Left column defaults to the current main item.
   - The Right column selector iterates over the **other** items in the duplicate group.
   - **Selector Label Fix**: When Left is fixed to File index $K$, the Right selector MUST display `<current_other_idx / total_other_files>`. E.g., for a 4-file group with File 3 on Left, Right selector must step through 1..3 of 3 OTHERS (must **NEVER** display `<3/4>`).
   - **Index Mapping Fix**: Fix the index modulo bug where 4 group members mapped to only 2 underlying file references (which caused ID3 tags and previews to flicker/repeat every 2 clicks).
5. **Edit & Save Permissions**:
   - Check write permissions before rendering edit widgets (e.g., ID3 tags or image rotation).
   - The file representation specifies if `Overwrite` or `Save As` is supported. If the parent repository is read-only, hide/disable all save buttons.

---

## 2. Lightbox & Compare Bug Fixes

Target Files: [`crates/dedup-gui/src/dupes_view.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/dupes_view.rs), [`crates/dedup-gui/src/lightbox.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/lightbox.rs)

### 2.1 Image Compare Fixes
- [ ] **Flicker Mode Pill Swap**: Fix state tracking so when `show_b` toggles in flicker mode, the displayed pill correctly toggles between `DELETE A` and `DELETE B` to reflect the active texture.
- [ ] **Side-by-Side Mark Triggering**: Fix button click handler so clicking `Mark A` modifies ONLY File A's mark, avoiding accidental dual-marking of both A and B.
- [ ] **Read-Only / Protected Repos**: Disallow marking files in read-only repos. Display `DELETE (Protected)` in a disabled state with strikethrough text.
- [ ] **Repo Name Badges**: Render prominent Repo badges (using [`repo_chip.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/repo_chip.rs)) in the header of both Left and Right preview panes so the repo source is always unambiguous.
- [ ] **Mark Pill Labels**: Standardize pill text across views: `DELETE`, `DELETE A`, `DELETE B`.
- [ ] **Side-by-Side Unmark Inconsistency**: Fix state retention when removing Mark B so Mark A remains untouched and correctly rendered.

### 2.2 Audio Compare & Playback Fixes
- [ ] **Play/Pause State Retention**: When clicking "Next MP3" or switching items in Lightbox, check the active `Player` state. If currently paused, keep paused. Do NOT auto-resume playback unless it was already playing.
- [ ] **Switcher Freeze (> 2 Audio Files)**: Fix event handling lockup when flipping through audio files after pressing "Compare" in groups with > 2 files. Ensure channel signals and UI repaints do not block the main loop.
- [ ] **ID3 Tag Sync Glitch**: Fix state indexing bug where ID3 tag modifications were applied to wrong file indices due to 1..4 iterating over a size-2 array.
- [x] **Audio Compare Mark Buttons**: the audio compare header itself still has no inline `DELETE A`/`DELETE B` pills, but the Overview screen (§1.3.3, now built) provides marking for both A and B before/after entering native compare, which covers the same need. Leave this line item open only if inline header pills (as opposed to Overview-based marking) are still wanted — ask before building both.
- [x] **Preserve the Gapless A/B Flip**: restored — the nav-while-comparing branch in `audio_lightbox` calls `Player::flip()` on the pre-loaded paired stream instead of re-seeking from disk. Guarded by the extended `dupes_view::ui_tests::audio_lightbox_opens_compares_plays_and_escapes`, which asserts `snap.paired` stays true and the audible hex flips between A/B on ArrowRight/ArrowLeft while comparing.
- [ ] **Not yet verified** (no regression test written this pass, so "gapless restored" does not imply these are closed): *Switcher Freeze (> 2 Audio Files)* and *ID3 Tag Sync Glitch* above. The Overview cycler is the likely path to files 3+ now, which plausibly fixes both, but neither has been run/tested.

---

## 3. Repo Sync Tab Removal & GROUP SYNC in Transfer [DONE, superseding this section's original plan]

**Status (2026-07-27):** This section originally planned a 1:1 compare pane living *inside*
Repo Sync. Mid-session the user reconsidered the tab split itself: group *membership*
(create/add-sink/remove-sink/mode) already lives on the Repositories tab, and pairwise
same-path/different-hash compare is already Transfer's DIFF (BY PATH) mode + `diff_board`'s
COMPARE button — Repo Sync's only genuinely unique capability was *pushing* a group (there was
no other GUI path to run `plan_group_sync`/`run_group_sync`). Decision: **delete the Repo Sync
tab entirely** and add pushing as a new **GROUP SYNC** command on the Transfer tab.

Landed:
- `sync_view.rs` deleted; `Tab::SyncGroups` removed from `app.rs`; `docs/gui/sync-groups.md`
  removed, folded into `docs/gui/files.md#group-sync`; `help_content.rs` updated.
- `Command::GroupSync` in `transfer_view.rs`: only offered when SOURCE is a sync group's main
  (`TransferView::current_group`, refreshed on source change / repo reload). Hides the single
  TARGET picker; shows a new SINKS multiselect panel (`group_sinks_bar`), defaulting to every
  sink, each showing its own stored ADD ONLY/MIRROR mode (not editable here). FILTER and
  REVIEW/RUN behave like every other command.
- Built as its **own lane** (own `Msg::GroupPreview`/`GroupDone`, `spawn_group_preview` /
  `apply_group_preview` / `raise_group_confirm` / `start_group_sync`), not forced into the
  generic `RunConfig`/`StartDest` pipeline — that pipeline's `StartDest` variants are all
  single-target, and a multi-sink push doesn't fit without contaminating the simple cases.
  Mirrors the file's existing DIFF-is-a-separate-lane precedent.
- Calls `plan_sync`/`diff_sync` **directly** per selected sink (not
  `dedup_core::sync_group::plan_group_sync`/`run_group_sync`), because those two don't accept
  a filter and GROUP SYNC's FILTER panel needed to actually narrow the plan, not just be
  visible-but-inert. `guard_mirror_source`/`delete_mode` were made `pub` in `sync_group.rs` so
  the empty-main-mirror refusal survives the bypass — called explicitly at both plan time and
  run time (state can change between REVIEW and RUN).
  `plan_group_sync`/`run_group_sync`/`diff_overview`/`RepoOverview` are now GUI-orphaned
  (dedup-core library API with real tests, no live caller) — left in place, not deleted; the
  user asked to delete the *tab*, not the underlying capability.
- 8 new tests: gating (offered only for a group's main), TARGET-hidden/SINKS-shown +
  default-all-selected, REVIEW counts, an actual end-to-end RUN that lands a real file in the
  sink on disk, the empty-mirror-main refusal surviving the `plan_sync`/`diff_sync` bypass,
  sink deselection narrowing the plan, and the FILTER actually narrowing it (not just visible).
- Doc screenshot `docs/screenshots/transfer_group_sync.png` (`doc_screenshot_transfer_group_sync`)
  — rendered and visually checked per the Overview-screen lesson above: a label-query suite
  alone would not have caught a layout regression here either.

**Not done, out of scope for this pass** (were §3.2/3.3 subtasks of the original plan, about
`diff_board`'s general BY PATH conflict rows, not GROUP SYNC specifically): `diff_board`'s
cells are still plain text (`paths_cell`/`sizes_cell`/`dates_cell`), not `media_cell`
thumbnails like the review board; no dedicated COMPARE column; no filename-level diff
highlighting; no batch rename/delete-all/copy-missing header actions. These would need
`RepoDiffRow`/`DiffFile` to carry `FileFacts` (currently they don't), which touches
`plan_repo_diff` and its dedup-core tests — a separate, larger change if still wanted.

---

## 4. Search Filter & Repo UI Enhancements

Target Files: [`crates/dedup-gui/src/filter_ui.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/filter_ui.rs), [`crates/dedup-core/src/filter.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-core/src/filter.rs), [`crates/dedup-gui/src/app.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/app.rs)

### 4.1 Filter Negation & Case Sensitivity
- [ ] **Filter Negation (`!pattern`)**:
  - Extend `dedup_core::filter::FileFilter` to support negation syntax (e.g., `!*.mp3`, `!/cache/`).
  - Add a "Negate (NOT)" checkbox toggle in `filter_ui.rs`.
- [ ] **Case Sensitivity Toggle**:
  - Add a `case_sensitive: bool` option to `FileFilter` and render a matching `Aa` toggle button in `filter_ui.rs`.

### 4.2 Tab Switching DB Refresh
- [ ] **Auto-Refresh Repo Stats**: When navigating to the Repositories tab (e.g. after performing duplicate deletions), automatically trigger a stats refresh from `redb` so updated file counts and free space reflect immediately without manual refresh.

---

## 5. Engineering Backlog & Micro-Optimizations

Target Files: [`crates/dedup-core/src/sync.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-core/src/sync.rs), [`crates/dedup-gui/src/diff_board.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/diff_board.rs), [`crates/dedup-gui/src/review.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/review.rs)

- [ ] **Single Main Re-read in Group Sync**:
  - In `plan_group_sync` / `run_group_sync`, collect main repo entries and content-key sets ONCE before looping over sinks, rather than re-reading the main index per sink.
- [ ] **Disambiguate `ReviewRow.target_path`**:
  - Replace ambiguous string formatting `format!("{sink}: {rel}")` with explicit `repo: String` and `rel_path: String` fields on `ReviewRow`. Render repo in its own dedicated column.
- [ ] **`diff_board.rs` Micro-efficiencies**:
  - Cache `totals(rows)` calculations per frame.
  - Share paging strip logic with `review.rs`.
  - Fix sorting comparator to borrow fields instead of cloning entire `DiffFile` instances.
- [ ] **Empty Walk Warning Guard**:
  - If a disk scan returns 0 files for a repository whose database index currently contains entries, issue a warning prompt (GUI) or require `--force` (CLI) before marking all indexed files missing.

---

## Verification & Definition of Done

Every task completed under this roadmap MUST adhere to the project standards outlined in [`AGENTS.md`](file:///home/axel/develop/dedup-rs/AGENTS.md):
1. **Compilation & Lints**: `cargo clippy -- -D warnings` must pass cleanly without warnings.
2. **Formatting**: `cargo fmt --check` must be clean.
3. **Tests**: `cargo test` must pass all unit and integration tests.
4. **Documentation**: Update [`CHANGELOG.md`](file:///home/axel/develop/dedup-rs/CHANGELOG.md) and [`ai/improvements.md`](file:///home/axel/develop/dedup-rs/ai/improvements.md) upon completing items.

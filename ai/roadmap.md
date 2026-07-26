# dedup-rs — Comprehensive Refined Roadmap

This document serves as the master implementation specification for upcoming features, refactorings, bug fixes, and architectural overhauls in `dedup-rs`. It synthesizes past QA bug reports ([qa.md](file:///home/axel/develop/dedup-rs/ai/qa.md)), architectural evolution notes ([improvements.md](file:///home/axel/develop/dedup-rs/ai/improvements.md)), and user requirements into actionable, step-by-step TODOs for automated agents or developers.

---

## 1. Lightbox Architecture: Data Representation Interfaces [COMPLETED]

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
- [ ] **Audio Compare Mark Buttons**: Add `DELETE A` and `DELETE B` mark pills directly into the Audio comparison view header. *(The stashed attempt was removed because it referenced image-lightbox bindings that don't exist in `audio_lightbox` and had no deferred-action channel there; re-add during the audio re-home with proper audio-scope mark bindings.)*
- [ ] **Preserve the Gapless A/B Flip (regression watch)**: The stashed audio-nav rewrite (switching copies while comparing) replaced the **gapless** A/B swap — `Player::flip()` on a pre-loaded *paired* stream — with a plain `Player::play()` re-seek from disk, which reintroduces an audible gap/restart. The audio re-home MUST **restore the gapless paired-flip path** (keep both A and B loaded, flip the audible channel without re-decoding) rather than replay. Guarded by `dupes_view::ui_tests::audio_lightbox_opens_compares_plays_and_escapes`, which asserts `snap.paired` and the gap-free flip. This regression currently sits in the working tree (tests still pass, so it is a behaviour regression, not a test failure) and must be reconciled, not blessed.

---

## 3. Repo Sync Tab Architecture & Transfer View Overhaul

Target Files: [`crates/dedup-gui/src/sync_view.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/sync_view.rs), [`crates/dedup-gui/src/transfer_view.rs`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/transfer_view.rs)

### 3.1 Move Transfer Compare to Repo Sync
- [ ] **Remove Group Compare in Transfer Tab**: Remove the legacy multi-repo grouped comparison with score numbers from `transfer_view.rs`.
- [ ] **Integrate 1-to-1 Compare in Repo Sync**: In `sync_view.rs`, allow selecting a **Left Repo** and **Right Repo**. Render the 1-to-1 diff table directly in Repo Sync.
- [ ] **Unified Lightbox Modal in Sync**: For rows with matching relative paths but differing content hashes, clicking "Compare" MUST open the unified Lightbox representation viewer.

### 3.2 Sync Table Layout & UX Improvements
- [ ] **Column Width Optimization**: Remove excessive empty horizontal padding. Auto-fit columns to content size and increase row height/font size for improved legibility.
- [ ] **Media Cell Thumbnails**: Embed [`media_cell`](file:///home/axel/develop/dedup-rs/crates/dedup-gui/src/media_cell.rs) thumbnail previews directly inside the Left and Right file columns of the Sync table.
- [ ] **Dedicated Compare Column**: Place a dedicated **Compare** button column between Left Path and Right Path columns.
- [ ] **Button Text Truncation Fix**: Enable multiline text wrapping or min-width constraints for action buttons so labels are never cut off.

### 3.3 Diff Highlighting & Batch Actions
- [ ] **Filename Diff Highlighting**: Highlight character-level differences in file names (e.g. blue background for divergent characters) when comparing Left and Right paths.
- [ ] **Batch Action Buttons**: Add header batch action buttons for large result sets (> 1000 items):
  - `Rename All Left` / `Rename All Right`
  - `Delete All Left` / `Delete All Right`
  - `Copy Missing Left -> Right` / `Copy Missing Right -> Left`
- [ ] **Cross-Type Audio/Media Sync Compare**: Re-use the unified Lightbox component so comparing audio files or cross-type media in Repo Sync works seamlessly without placeholders or broken renders.

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

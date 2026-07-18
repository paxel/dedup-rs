# dedup-rs — Improvement Roadmap

## Vision

dedup-rs is a **data-inheritance triage tool**. The scenario it serves: someone dies (or a
machine dies) and leaves behind a NAS, broken PCs, and an unsorted heap of disks full of
redundant backups. A caretaker must find the useful and important material — documents,
photos, crypto wallets, keys — without eyeballing terabytes of duplicates. As the
"no hardcopies" generation ages, this is a recurring, real problem.

Media strategy is **hybrid**: in-app image zoom, in-app audio playback, video as
scrub-able frame strips; one click hands any file to the system's external app for full
fidelity.

All four phases originally planned in this document (review tooling, sanitize workflow,
content coverage, forensic layer) have shipped. What remains is a near-term preview &
lightbox polish wave (Phase 6), the deferred Browse (Phase 7) and Recognition (Phase 8)
layers, and cross-cutting debt.

---

## Remaining / deferred work

- **Light theme toggle** (M/L, deferred by decision 2026-07-08): requires converting
  `theme.rs` constants to a runtime palette across all views. Still dark-only.
- **Performance at scale**: banded grouping, staged pipelines, and the multi-reference
  diff's merged content index are fine at ~10⁵ files; revisit content-index memory
  (`HashMap<(u64,[u8;32]), _>` across all references) and timeline streaming at 10⁷.
- **Testing discipline** (standing practice, not a task): every GUI feature ships with
  kittest geometric tests + an `--ignored` render snapshot; every core feature with
  temp-repo integration tests; store format changes must include a legacy-decode test
  (pattern: `store.rs::v1_entries_decode_and_flag_images_stale`).


## Phase 5 — Transfer, Grooming & smarter ETA  - done
---

## Phase 6 — Preview & Lightbox polish  *(near-term)*

The Duplicates lightbox is where triage actually happens, and it has three gaps: audio is
near-unusable (files render as the generic broken-image placeholder), the keyboard/​hint
affordances are incomplete, and there is no way to fix or re-tag a file without leaving the
app. This wave makes the lightbox a first-class comparison surface for **both** images and
audio, with safe in-place edits. (Folded from the "Open issues" list, kept verbatim below.)

### 6.1 Keyboard shortcuts & discoverable hints

**Lightbox — ✅ done (2026-07-14).**
- **Escape is a universal "back"**, not an immediate close: it pops one view level per press
  — flicker → side-by-side → single image → closed. This is the intended principle for all
  modals/future views (applied to the lightbox now; rolled out elsewhere with the cross-view
  work below).
- **`Space` drives flicker**: it enters flicker from side-by-side (previously button-only),
  then swaps A/B once there.
- **Hint "vanished on compare" was a real layout bug** (user was right): the hint string
  existed, but compare stacks three bottom lines (path A, path B, hint) into a strip only
  tall enough for two, so the hint rendered *below* the window bottom (measured at y≈709 in
  a 700 px window). Fixed by growing the bottom strip (and shrinking the viewport to match)
  in compare mode; a geometric test now asserts the hint stays on-screen.
- The compare hint bar is now **mode-aware** (distinct side-by-side vs flicker strings) and
  names every available shortcut.

**Cross-view shortcuts — ✅ done (2026-07-15).** A shared `util::shortcut_bar` renders a
persistent hint line (same LILAC style as the lightbox) at the top of each view, and each
view now handles keyboard shortcuts (guarded by `egui_wants_keyboard_input()` so text fields
still type, and — for Duplicates — skipped while a lightbox/confirm modal owns the keyboard):

- **Duplicates grid:** `F` find · `←`/`→` page · `M` toggle match mode.
- **Transfer:** `1` copy · `2` move · `P` preview · `R` run.
- **Grooming:** `1`–`4` command (dedupe/purge/empty-dirs/organize) · `P` preview · `R` run.

Tests: `grid_shortcut_toggles_match_mode` (dupes), `number_keys_select_command` (transfer &
grooming).

### 6.2 Audio preview tile — ✅ done (2026-07-14)

- **Fixed the "broken image" look:** audio cards used to fall back to the generic
  placeholder (an image-square icon + the raw mime string). They now render a
  **deterministic fingerprint glyph** — a waveform whose bar heights and accent colour come
  from `AudioFp.chunk_hashes[0]` (each bar an independent hash byte, no mirror symmetry, so
  distinct audio looks distinct) — with the duration centred below, bordered to match the
  image thumbnails (`paint_audio_glyph` in `dupes_view.rs`).
- **Honest scope correction:** the glyph signals *identity*, **not** similarity. The chunk
  hash is BLAKE3 (full avalanche), and audio "similars" are grouped on **exact**
  `chunk_hashes` equality (`similar.rs`), so within any displayed group every member's glyph
  is identical by construction. Same content → same glyph; different content → different
  glyph. It cannot show gradations of similarity (the earlier "similar audio → similar glyph"
  claim was wrong).
- **Deferred:** clicking the glyph to open an audio lightbox lands with 6.3 (the tile is
  non-interactive for now — the existing PLAY controls handle playback). No audio lightbox
  exists yet, so wiring the click to the image lightbox would just show "decoding…" forever.

### 6.3 Audio lightbox & audible comparison — ✅ done (2026-07-14, scope "both")

- **Real decoded waveforms, not the fingerprint.** The fingerprint glyph is identical for
  every copy in a group (grouped on exact hash), so it can't show diffs — confirmed with the
  user, who chose the full build. A new `waveform.rs` decodes each file to a normalized
  amplitude envelope on a background pool (mirrors `FullResCache`), cached by content hash.
  Extraction **streams** with a peak-fold that halves resolution when it fills, so it needs
  no length estimate (the decoder's reported duration proved unreliable — a 2 s clip reported
  6.3 s) and stays O(buckets) memory in one pass.
- **Audio lightbox** (`audio_lightbox` in `dupes_view.rs`): clicking an audio tile opens a
  full-window view with each copy stacked for A/B compare, a playback cursor, and a transport
  bar. `P` play/pause, `←`/`→` switch copy, click to play that copy from that spot, `C`
  compare, `Esc` steps back a level. Reuses `LightboxState`/`CompareState`.
- **Spectrogram view + real flicker** (follow-up on user feedback): the amplitude waveform is
  a flat block for loud/compressed music, so `S` toggles a **spectrogram** (frequency×time,
  brightness = per-band loudness) built from a dependency-free radix-2 FFT/STFT in the same
  streaming pass (`waveform.rs::AudioViz`; magma colormap; GPU texture cached by hash). And
  `space` now drives **flicker exactly like the image lightbox** — enters flicker from
  side-by-side, then swaps A/B — so you can flick A↔B (spectrogram or waveform) to spot
  differences. (Play/pause moved off `space` to `P` to free it for flicker, per the user.)
- **Spectrogram fix** (bug the user caught — "bright left edge, rest black; goes black after
  1 min of a 6 min song"): the real culprit was the **column fold losing its time axis**. The
  STFT halves resolution when the column buffer fills, but — unlike the envelope — it wasn't
  increasing frames-per-column, so old columns pooled a max over ever-more frames (bright)
  while newer ones covered a handful (black), collapsing the whole song into the first few
  columns. Fixed with a `col_step` that doubles on each fold (mirrors the envelope), so
  columns keep a uniform time span. My first pass only changed the intensity scale (dB with a
  70 dB floor + DC-bin drop) — a real contrast improvement, but *not* the bug; the short (3 s)
  render test never folded, so it hid the defect. New unit test decodes a long tone and
  asserts its bin is lit at x=0, mid, **and the last column**.
- **Gap-free audio switching in compare** (bug the user caught, then extended): swapping in
  flicker changed only the picture, not the sound. `player.rs` now has a **paired mode** — two
  sinks play the same offset in sync, one muted. The pair is kept loaded whenever you're
  comparing-and-playing, so **both** a flicker `space` swap **and** a side-by-side click on the
  other copy are an instant volume **flip** (no reload, no gap); a click only seeks if it
  actually moves the playhead. The playback cursor follows to the audible copy.
- **One cursor, not two** (bug fix): the cursor keyed off content hash, so exact-duplicate
  copies (shared hash) both lit up. Now tracked by row via `LightboxState.audio_active`.
- **Keep-offset switching, both places** (the flagged "maybe" — user said yes): switching
  copies (in the lightbox *and* the card grid) keeps the current playback offset. Threaded
  `start_ms` through `Player::play` (seek-on-start).
- **Honest note:** within a group the opening bytes are identical, so views line up early and
  real differences show later (or in a slightly different length).
- **Deferred:** the *small pause* the user flagged is gone for both the flicker swap and
  side-by-side clicks. Only `←`/`→` (which changes *which* copy is A, a structural change) and
  the very first play/click that loads the pair still reload — an acceptable one-off.
- Tests: `waveform.rs` envelope+spectrogram unit tests (WAV generated in-test — no
  audio-writer dep), `audio_lightbox_opens_compares_plays_and_escapes` (P/S/space-flicker/Esc),
  `switching_audio_copies_keeps_offset`, and `--ignored` `render_audio_lightbox` (spectrogram).

### 6.4 Lightbox image editing (lossless) — ✅ done (2026-07-15)

- **Edit controls** in the single-image lightbox: `ROT L` / `ROT R` (90° steps), `FLIP H` /
  `FLIP V`, plus `RESET` and `SAVE` once edited. A live preview shows the rotated/flipped
  image (zoom/pan track the new dimensions); `imgedit.rs::apply_ops` composes the ops.
- **JPEG decision (user):** re-encode at quality 95 — no new dependency. PNG/BMP/etc. stay
  bit-exact lossless; JPEG loses a little (labeled in the save modal). True lossless JPEG
  would need a C library (libjpeg-turbo); declined.
- **Caveat (not yet handled):** overwriting a file changes its bytes, so its stored
  content-hash/dimensions in the index go stale until a re-scan. Acceptable for now; a
  re-index-on-edit hook is a follow-up.

### 6.5 Audio id3 tags — ✅ done (2026-07-15)

- **Display:** the audio lightbox shows an `Artist — Title` summary in the bottom strip
  (single view), read via the new `id3` crate and cached per hash (`id3tags.rs`).
- **Editor (user chose "add id3"):** a `✎ TAGS` button (or `T`) opens a modal with
  Title / Artist / Album / Year / Track / Genre. `SAVE TAGS` writes **only the tags** back
  (audio untouched, other frames like album art preserved); empty fields clear that frame.
- **6.6 reuse:** the modal carries the "writes the tags to the file on disk" note and stays
  open after saving. `Esc` closes the editor before it backs out the lightbox.
- **Scope:** ID3v2 only (MP3/WAV/AIFF); FLAC/OGG show no tags (read returns `None`).
- Tests: `id3tags.rs` round-trip + clear-field unit tests (real bare-MP3 in-test), and
  `audio_lightbox_edits_and_saves_id3_tags` (T → edit → SAVE writes to disk, others
  preserved, lightbox stays open); `--ignored` `render_audio_tags`.

### 6.6 Safe in-place saves — ✅ done (2026-07-15) *(cross-cutting for 6.4 & 6.5)*

- **Save decision (user): offer both each time.** The save modal presents `OVERWRITE
  ORIGINAL` (red) vs `SAVE A COPY` (writes a non-colliding `_rot` sibling — never loses data)
  vs `CANCEL`, with the ack "Overwriting changes the file on disk and cannot be undone."
- Overwrite is **atomic** (temp file + rename, so a failed encode can't truncate the
  original); a copy never overwrites an existing file.
- Saving keeps the lightbox open and the preview showing (verified by test). Wired for images
  now; 6.5's id3 writes will reuse the same modal.

### 6.7 Audio compare polish & id3 in the diff view — ✅ done (2026-07-15, from user feedback)

Follow-ups on the audio lightbox (6.3) and id3 editor (6.5):

- **Fixed — arrow-switch paused and the cursor stuck on top.** `←`/`→` in compare used to
  re-navigate the *A index*, reloading a single sink (gap) and, with 2 copies, colliding A
  and B so the line stayed on top. Now arrows **flip which copy is audible** (gap-free via the
  loaded pair) and move the cursor with it; they only step the index in single view.
- **id3 tags in the diff view — one panel per row (symmetric).** A **read-only** tag panel
  sits on the right of *each* waveform row: A's tags beside the A wave, B's beside the B wave
  (not A/B columns in one box — the first cut put B off-screen and read as "no table"). Each
  panel lists Title/Artist/Album/Year/Track/Genre with differing values highlighted, and
  **flickers** with its row so tag diffs pop when flicking A↔B.
- **Per-panel EDIT button** opens the editor modal for *that* copy (`open_tags` now carries a
  group index); the panels stay read-only. Top-bar `TAGS`/`T` edits the current copy.
- **Pick values from any copy.** The editor collects the distinct value of each field across
  *all* the group's copies; each field has a menu to adopt any of them.
- Tests: `audio_compare_arrow_flips_audible_copy`, `tag_editor_offers_values_from_all_copies`,
  and the `--ignored` `render_audio_tags` now renders the A/B diff table.

### 6.8 ID3v1 read fallback — ✅ done (2026-07-15)

- **Read** falls back to ID3v1 (the 128-byte trailer on older/ripped MP3s) when there's no
  ID3v2, so those files show tags in the diff panels instead of blank — `id3tags::read` uses
  `id3::v1v2::read_from_path` (v2, then v1).
- **Write stays ID3v2.4** and now **strips any ID3v1 trailer** so a stale v1 can't shadow the
  edit (user's add) — via `id3::v1v2::write_to_path`, which removes v1 after writing v2.
- Tests: `read_falls_back_to_id3v1`, `writing_v2_strips_the_v1_trailer`.

---

## Phase 7 — Browse & forensic layer

A fifth **Browse** tab: a superfile/lazygit-style, DB-driven, directory-based file
browser for one repo, hosting the forensic tools. Layout top→bottom: repo picker →
shared FILTER wizard → clickable breadcrumb → two columns (subdirs | files) → preview
dock (per file type) with a command-button dock on its right. Keyboard: in the dirs
pane `←` parent / `→` enter / `↑↓` move; `Tab` switches to the files pane; mouse works
everywhere.

- ✅ **7.1 Store foundation** — separate `annotations` redb table keyed per repo+rel
  (`get_annotations` / `set_annotations` / `all_annotations`), trims+dedups, empty
  clears the row. No `FileEntry` format bump. Round-trip test.
- ✅ **7.2 Browse shell** — `Tab::Browse` + `browse_view.rs`: single-repo picker,
  breadcrumb, two-column subdirs|files derived from `for_each_file_entry` (no FS
  access), keyboard nav (← parent / → enter / Tab switch). Functional + render tests.
- ✅ **7.3 Filter pruning** — the shared `filter_ui::FilterBuilder` (between repo picker
  and breadcrumb) prunes the whole navigation via `FileFilter`: a file shows only if it
  matches, and a subdir shows only if a file beneath it matches (so the tree only leads
  to matches). Filter changes reset the selection. Unit-tested.
- ✅ **7.4 Preview + command dock** — hard, splitter-resizable panel layout (left
  subdirs pane · centre file table · bottom preview dock · right command column) that
  never reflows on navigation. File table via **egui_extras** (drag-resizable columns,
  click-to-sort headers with a caret indicator). Preview by type (image/video →
  thumbnail, audio → waveform, text → scrollable lines, else hex header + strings) and
  commands (Open with default app, Reveal). Both panes keep their selection marked
  (bright/dim by focus). On-disk files touched only here; text/binary bodies cached per
  rel-path. Functional + sort + render tests. (Still to reuse: the full image/audio
  lightboxes and the id3 editor as command buttons — folded into 7.5/7.6.)
- ✅ **7.5 Annotations UI** — in the command dock (shown *first*, so it's visible without
  scrolling): free-form multi-tags as removable tag-badges, an add-tag field (Enter or
  button), and an existing-tag picker that **filters as you type** (autocomplete) so you
  never retype a tag. Wired to `get`/`set`/`all_annotations`. Integration-tested. The file
  table also gained two sortable DB-backed columns: **INFO** (image `W×H` / audio `m:ss`)
  and **ANNOTATIONS**. Annotations render everywhere as **tag-glyph badges** (the vendored
  Phosphor subset has no tag icon, so the glyph is hand-painted).
- ✅ **7.6 Multi-select + flatten** — files-pane multi-select (Ctrl/Cmd-click toggles,
  Shift-click ranges; arrows/nav collapse it) with a batch dock ("N selected" → tag all
  selected). A **Flatten** toggle (right of the breadcrumb) hides the dirs pane and lists
  every matching file under the current dir recursively, named by sub-path. Flatten +
  batch-tag unit-tested, render-verified. (Batch id3-field-set on selected audio is
  deferred — needs the id3 write path.)
- ✅ **7.7 Forensic extras** — an **Inspect bytes** command toggle forces a byte view for
  media/text files (hidden for unknown types, which already show the byte fallback); a
  **Hex | Strings** switch in the preview picks which half to show (both at once was too
  much). All Browse toggles/rows are now self-painted (`paint_chip`) so they stay legible
  in every state — the theme's `override_text_color` made plain `selectable_label`s
  cream-on-amber on hover (captured in the new `egui-desktop` skill). Unit-tested;
  render-verified incl. hover.
- ✅ **7.8 Annotation filter** — a **`tag:`** condition in the shared FILTER wizard, so
  annotation filtering works the same way everywhere the wizard is used (Browse, Transfer,
  Grooming) rather than as a Browse-local widget. Core's `FileFilter` gained an `Anno`
  variant; because tags live in a separate table `FileFilter::matches` can't see, an
  `AnnotatedFilter` loads the repo's annotation map once and every streaming filter path
  (`count_matches`, diff, groom, date-export) matches through it (`matches_tagged`). The
  wizard's TAG editor suggests the repo's existing tags. This also **unlocks the
  annotation-driven Transfer exports** (export important / archive unimportant, per 5.1),
  since Transfer/Grooming now filter on tags. Unit-tested end-to-end (parse → count →
  listing). *(Organize auto-rules match on identity only, so `tag:` in a rule is a no-op —
  a deliberate boundary, they're separate from the wizard.)*

## Phase 8 — Recognition & extensibility  *(far future)*

- Face recognition and object recognition for photos/images.
- VLA tagging of files to topics; word clouds for documents.
- MP3 tag handling; metadata extraction for all known formats.
- Plugin support for new formats; an API to externalise features.

---

## Open issues & requests — 2026-07-17

### Bugs

- ✅ **Duplicate "best" pick ignores writability** — resolved 2026-07-17. Within each dupe
  group, copies in a **read-only (protected) repo** are now promoted to "best"
  (`dupes::promote_protected_first`, stable so content order still breaks ties among equally-
  protected copies), so the writable duplicate falls to the deletable tail and is the one
  marked by default. No-op when all repos are read-only (the default). *(Repo-level read-only,
  per the user; the filesystem permission bit is not tracked.)*
- ✅ **`.m3u` treated as audio** — resolved 2026-07-17. A shared `fingerprint::is_audio_mime`
  excludes playlist MIME types (`audio/x-mpegurl`, `audio/mpegurl`, `audio/x-scpls`, …) from
  the audio treatment, wired into core fingerprinting and every GUI audio check (Browse
  category, dupes audio controls / thumbnails / lightbox). Playlists now fall to the generic
  byte/strings preview rather than a waveform. *(Already-indexed `.m3u` files keep their stale
  audio fingerprint until a re-scan; the GUI no longer treats them as audio regardless.)*
- ✅ **Video lightbox scrubbed on mouse-move** — resolved 2026-07-17. The shown frame was
  bound to the cursor's x over the *whole* viewport, so any mouse movement changed it. Now
  the filmstrip stills are **click-to-pin** (per user's choice): clicking a still shows that
  frame enlarged and it stays put; mouse movement never changes it. Pinned frame lives in
  `LightboxState::video_frame` (defaults to the middle still, resets when navigating to
  another copy). Cells show a pointing-hand cursor + hover outline.

### Features

- ✅ **Shared FILTER on the Duplicates tab** — resolved 2026-07-18. The Duplicates view now
  hosts the shared `filter_ui::FilterBuilder` (same widget as Transfer/Grooming/Browse),
  sitting between the REPOS bar and the MODE/FIND controls; the first included repo backs its
  MIME/TAG pick-lists. *(Semantics decided with the user: **keep whole groups that contain a
  match** — FIND finds all duplicate groups, then keeps any group with ≥1 member matching the
  filter and shows **all** its copies. So `tag:important` surfaces the dupes involving tagged
  files, other copies included.)* Core: `find_similar` gained a `filter: Option<&FileFilter>`
  (retains matching groups in memory); a new `dupes::retain_matching_keys` filters the exact
  **plan keys** by streaming each key's members (short-circuit on first match), so the
  memory-light paged exact path is preserved — no filtering is done when the expression is
  empty. `find_exact_duplicates` is unchanged (its CLI/folder-export callers don't filter).
  The GUI parses the expression once up front (surfacing a bad filter immediately) and threads
  it into the background FIND. Tests: core `retain_matching_keys_keeps_whole_group_when_any_member_matches`
  and the `find_similar` filter assertions in `similar_repo`; GUI `filter_wizard_is_present`
  and `find_applies_the_filter` (drives FIND with `name:g0_`, 5 groups → 1).
- ✅ **Transfer `sync` command** — resolved 2026-07-17. The Transfer tab gained a third
  **SYNC** command (`3` from the keyboard) that mirrors the source into the target repo at
  the same relative path: it copies content the target lacks and, with an opt-in **DELETE
  MISSING** toggle, deletes target files whose content the source has since lost. The core
  `diff_sync` (already CLI-wired) was reused as-is — SYNC itself does **not** delete arbitrary
  target files absent from the source; its "delete" means *propagate the source's own
  deletions*, matching the CLI's `--delete-missing`. *(Scope decided with the user: SYNC
  reuses core semantics; DELETE MISSING defaults off so SYNC is additive unless asked. A true
  content-mirror shipped separately as the **MIRROR** command below.)* SYNC is repo→repo only,
  so it hides the DEST/subdir/folder/DUPEPOOL controls and shows its own OPTIONS bar. To match
  COPY/MOVE's live run panel, `diff_sync` now emits per-file `DiffEvent` progress via `DiffRun`
  (one step per acting entry; `done ≤ total`); a new `plan_sync` backs the preview/confirm
  counts without touching disk. Tests: core `plan_sync_lists_copies_and_deletes` +
  `sync_emits_progress_for_copies_and_deletes`, GUI
  `sync_mode_shows_delete_toggle_and_hides_transfer_controls` (+ extended `number_keys`),
  and `--ignored` `render_transfer_sync`.
- ✅ **Transfer `mirror` command** — resolved 2026-07-17. A fourth **MIRROR** command (`4`)
  makes the target an exact **content-mirror** of the source: it copies content the target
  lacks and **deletes everything in the target the source does not have**, so the target ends
  up holding exactly the source's content. Implemented by generalising `diff_sync`'s delete
  bool into a `SyncDelete` enum `{ None, Missing, Absent }`; MIRROR uses `Absent`. The mirror
  **deletes first, then copies**, so a copy can reclaim a path a delete frees (target holds
  *different* content at a source path → the path ends up with the source's content). It is a
  mirror by **content**: identical content already in the target at a *different* path is kept,
  not relocated (consistent with the app's content-based model — paths never matter for
  existence). MIRROR is always destructive (red pill, no toggle, a red "DELETES EXTRAS"
  warning bar) and repo→repo like SYNC. The CLI `--mirror` flag now means this true mirror
  (was copy + delete-missing). Tests: core `mirror_deletes_target_content_absent_from_source`,
  `mirror_deletes_first_so_a_copy_reclaims_the_freed_path`, `plan_sync_absent_lists_mirror_deletes`;
  GUI `mirror_mode_shows_warning_and_hides_toggles` (+ extended `number_keys`) and `--ignored`
  `render_transfer_mirror`.
- ✅ **Confirm/preview step for copy / move / sync / mirror** — resolved 2026-07-17. All
  commands share the PREVIEW grid (first `from → to` rows; SYNC/MIRROR also list red `path →
  deleted` rows) and a CONFIRM modal that states the counts before running ("copy X … / copy
  X and delete Y …"); PROCEED turns red whenever the run would delete (MOVE, MIRROR, or SYNC
  with DELETE MISSING on).
- ✅ **Repo relocate: folder picker** — resolved 2026-07-17. The inline relocate editor now
  has a **CHOOSE…** button opening the native folder picker (reusing the add-form's threaded
  `rfd` flow, routed by a new `FolderTarget` so the result lands in the relocate buffer). The
  path field remains for typing. Relocate also now **clears the stale "missing" repo status**:
  after a successful relocate it drops the row's carried-over `Location` and re-probes
  location/reachability against the new path (→ Local/Remote if the folder is now reachable,
  or back to Missing if the new path is also bad). *(Repo-level status, not per-file flags.)*
- ✅ **Drag-and-drop add repositories** — resolved 2026-07-17. Dropping one or more folders
  onto the window adds each as a repo in one gesture: names are derived from the folder
  basename (`sanitize_repo_name`, filesystem-safe) and made unique against existing repos +
  others in the same drop (`unique_repo_name` → `name-2`, `name-3`, …). Non-folder drops are
  ignored; the view switches to Repositories with an "Added N repositories: …" notice (errors
  surfaced too). A full-window "Drop folders to add them as repositories" hint appears while
  folders hover. Blocked during an active update (registry locked, like the ADD button).
  Unit-tested (name sanitizing + de-dupe).
- **`prune` command (from the old Java tool)** — remove deleted (missing) files from the DB
  and clean up / compact the index files. *Open question:* still needed with redb, or does
  compaction/`mark_missing` already cover it? Verify before building.
- **Wrap single-line group rows** — some group rows (e.g. the repo chips) lay out on one
  horizontal line and scroll out of the frame when there are many items in a small window.
  They should **line-break / wrap** to multiple rows instead. Applies to almost every group
  *except* the compare groups (whose side-by-side layout is intentional).
- **Compare-group action buttons** — give each compare group its own group-level buttons:
  **mark all**, **mark none**, and **hide group** (hidden until the next FIND). Quicker bulk
  handling of a group without touching each file.

### Design questions (filter ↔ repo)

- **Filter with no repo selected** — the FILTER wizard's MIME/TAG pick-lists are repo-backed,
  so they're empty until a repo is chosen (you can still type raw conditions). Decide whether
  the filter should be **disabled/hidden until a repo is selected** or stay usable-but-
  unassisted. *(Confirmed direction: keep the editor-based, repo-backed pick-list — "A".)*
- **Repo change can make an active filter moot** — conditions are kept verbatim across a repo
  switch, so a `mime:`/`tag:` value that doesn't exist in the new repo silently matches
  nothing. Decide: keep as-is (transparent 0-match), surface a warning, or clear conditions
  on repo change. Suggestions already refresh to the new repo.

---

### Source remarks (verbatim, kept as reference)

user demands changes:

* The ETA calculation is waaay off. the last time it predicted about 2h and it took 6. even 1h before finish the eta was still like 8 minutes. there must be some better prediction algos. is there a lib that allows that? if not we should create something like that: a function where you put total items, concurrent lanes, and then duration per lane and ask for eta every 5s to have a less flickering display?
* I dont understand the also REF selection. I find it not intuitive and there must be a better solution for whatever it tries to solve. create a plan with different options for the user to decide
* The Files section should be renamed to transfer section. because its transfering  files from different repos together. the delete should be moved to the new fourth section: grooming
* the move to and copy to commands are added, where unique files are copied or moved into a folder that the user can specify with a folder selector
  * the move and copy to have also mode selector where duplicates and siliars can be selected and an inverter so that duplicates instead are cpied / moved
* the grooming should have a top selector for the command and as it is the most complex one every command should have its own layout.
* the delete has a source repo selector and a multi repo selector for the remaining repos. the source repo deletes all duplicates that are in any of the selected repos.
* the organize command has a filter a path generator where the taret path can be generated with placeholders and alternatives to define the new reative path of files in a repo
  * maybe multiple filters and paths
  * the complete selection can be named, stored and reactivated by the user on other repos or in te future. for repeating or modifying the organisation
  * no file should ever get lost or overwritten here.
  * a preview similar to the copy move is required
  * a progress when executed too
* small tools like: 
  * delete empty dirs
  * delete ALL of a filter: mime, size name. the name filter should allow some kind of wildcards to ensure that you can say: ends with .db or starts with copy_of
* the fith page will be browse where all the forensic tools will be added. allo the user to filter files, display them, mark them with annotations (trash, important, etc)
* with annotations a new filter for annotations makes sense
* binary / hex view of at least the header of files
* strings on demand on unknown files
* the transfer copy to with annotation filter can be an export of important or otherwise tagged stuff. similar the move to can be an removal and archival of unimportant stuff
* face recognition of photos and image is a far future task
* object recognition also
* vla of files to specific topics
* word clouds to documents
* mp3 tags handling
* meta data extraction of all known formats
* plugin support for new formats
* api for externalize features


### Open issues (verbatim, folded into Phase 6)

* The lightbox has a shortcut description on the bottom. it goes away when you choose compare
* I like the shortcut description and I want one in every view and also as much as possible shortcutable
  * in lightbox the flicker view and there especially the swap need a key, maybe space?
* the display of audio is quite awful. the icon looks like a broken image. maybe show some meta info and maybe display the fingerprint as a glyph? you know where some short seed generates some small image, by taking some bits as color some as mirror some as line generator. dunno. some small human recognicable visualisaton. or optionl just the fingerprint as a grayscale block.
  * clicking the fingerprint image should bring you to the lightbox and the audio data is converted to nice audio graphs that can be compared against each other, so you can SEE the diffs?
* playing audio in the lightbox should allow switching between different audios and continues to play so you can also hear the differences of similar files
  * maybe this should also be possible in the normal view? when you play another play button, it keeps the offset?
* the lightbox should have an edit button to flip and rotate images losless with the option to actually overwrite the image with the flipped rotated image
* the audio lightbox should have a id3 display, and maybe an editor to change the id3 tag and save it
* all save actions in the lightbox need a "sure, you change that, you know" kind of ack for the user. saving does not leave the lightbx

# dedup-rs — Improvement Roadmap

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


## Phase 8 — Recognition & extensibility  *(far future)*

- Face recognition and object recognition for photos/images.
- VLA tagging of files to topics; word clouds for documents.
- MP3 tag handling; metadata extraction for all known formats.
- Plugin support for new formats; an API to externalise features.

---

## Open issues & requests — 2026-07-17

### Features
- **Wrap single-line group rows — ✅ done (2026-07-18, REPOS bar).** Repo chip rows now
  line-break onto multiple rows instead of running off the right edge in a narrow window.
  egui can't wrap the composite `Frame` chips itself (it only wraps items whose size it
  knows before layout, and any wrapping layout also grabs the full panel height), so
  `repo_chip::chip_row` packs the chips into `horizontal_top` rows by hand using each chip's
  size measured the previous frame (cached in egui temp memory); one row reproduces the old
  bar exactly, and `repo_chips_wrap_when_narrow` asserts the wrap + per-row top-alignment.
  The compare (file-card) group rows keep their intentional horizontal scroll.
- **Unified repo selectors — ✅ done (2026-07-18).** Every repo selector (Duplicates,
  Transfer, Grooming, Browse) renders repos with one shared chip (`repo_chip.rs`): a
  generated per-repo identicon + name, accent-filled when selected, with a padlock as a 3rd
  item on Duplicates. All manual REFRESH/RELOAD buttons removed — each view now
  `sync_repos`es non-destructively when its tab is shown (`app.rs` tracks `synced_tab`).
- **Compare-group action buttons — ✅ done (2026-07-18).** Each Duplicates group header now
  has **MARK ALL** (mark every writable copy for deletion — protected copies skipped),
  **MARK NONE** (clear the group's marks), and **HIDE** (dismiss the group from the list
  until the next FIND; `hidden` set, reset on FIND). Buttons use `repo_chip::small_button`.

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

## Open issues & requests — 2026-07-18

### Immediate
- **Repo selection: mark all / mark none — ✅ done (2026-07-18).** Every multi-select repo
  row (Duplicates *include*; Transfer & Grooming *dupe pools*) now has MARK ALL / NONE
  buttons in a header line (`repo_chip::small_button`). Duplicates repos now **default to
  excluded** — no wall of highlighted chips on open; FIND already says "Select at least one
  repo" when none are picked. Read-only lock still defaults on (safety).

### Sync groups & remote backup  *(future)*
- Backing up to a remote is currently manual: DUPLICATE a repo, RELOCATE the copy to the
  remote path, then UPDATE it. Make it a native feature. Mark repos as a **sync group**:
  one **main** repo plus one or more remote **sinks**. In the normal repo lists the sinks
  are **collapsed** (shown only when expanded) and otherwise treated as sinks of their main
  repo. A dedicated **Sync Groups** tab maintains the groups: run a sync (push main →
  sinks), detect **external changes in a sink** that may need migrating back to the main,
  resolve divergence, etc.

### Preview panel → review board  *(near-term; grows into sync compare)*
- **Visual pass — ✅ done (2026-07-19).** Both transfer/grooming previews now render through
  one shared, scalable **side-by-side diff** (`review.rs`, à la Beyond Compare). Four columns:
  `STATUS │ ⟨source abs path⟩ │ ⟨target abs path⟩ │ STATUS` — the two status columns are
  per-side icons (Phosphor: green `+` added, red trash removed, grey `✓` unchanged, grey `✗`
  absent), and each middle column shows the file's **relative path** on that side, coloured to
  match its status; a side where the file is absent is an empty path cell. So a copy is
  source-`✓`/grey path + target-`+`/green path; a deletion is source-trash/red path +
  target-`✗`/empty. An icon summary sits on top and unchanged rows (least interesting) are
  **hidden behind a toggle** by default. The virtualised, click-to-sort table reuses Browse's
  `egui_extras` pattern + the now-shared `util::sort_header` and scrolls through huge sets —
  sort a status column to group e.g. all deletions. No core changes; the summary shows true
  totals even when the sample is capped.
- **Interactive half — ✅ done (2026-07-19).** Every actionable row now has a **✗ reject**
  toggle (row dims + strikes through; RUN skips it; the summary counts rejections and the
  confirmation notes them) and a **→ apply** button (execute just that action immediately;
  the preview refreshes when it finishes). No new single-file primitives were needed: the
  core `DiffRun` carries an optional exclude set + `only` allowlist consulted by every batch
  op (copy/move/sync/mirror/export/dedupe/purge/organize/prune), so a single-row apply is
  the battle-tested batch op restricted to one row. Row keys are namespaced (`s:<rel>` /
  `t:<rel>`, `diff::source_key`/`target_key`) because a mirror can delete and copy the same
  relative path. Rejections clear whenever the preview's configuration changes
  (`clear_preview`), and survive a single-row apply's refresh. The same panel later powers
  **sync-group compare** (deleted / different / new files across group repos; equal files
  optional — usually too noisy) and **manual per-file sync** between repos.

---

### Source remarks (verbatim, kept as reference)

user demands changes:

* the organize command has a filter a path generator where the taret path can be generated with placeholders and alternatives to define the new reative path of files in a repo
  * maybe multiple filters and paths
  * the complete selection can be named, stored and reactivated by the user on other repos or in te future. for repeating or modifying the organisation
  * no file should ever get lost or overwritten here.
  * a preview similar to the copy move is required
  * a progress when executed too
* face recognition of photos and image is a far future task
* object recognition also
* vla of files to specific topics
* word clouds to documents
* meta data extraction of all known formats
* plugin support for new formats
* api for externalize features



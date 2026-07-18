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
- **Wrap single-line group rows — ✅ done (2026-07-18, REPOS bar).** The Duplicates REPOS
  chip row now line-breaks onto multiple rows instead of running off the right edge in a
  narrow window. egui can't wrap the composite `Frame` chips itself (it only wraps items
  whose size it knows before layout, and any wrapping layout also grabs the full panel
  height), so `repo_bar` packs the chips into `horizontal_top` rows by hand using each
  chip's size measured the previous frame (`repo_chip_sizes`); one row reproduces the old
  bar exactly, and `repo_chips_wrap_when_narrow` asserts the wrap + per-row top-alignment.
  The compare (file-card) group rows keep their intentional horizontal scroll.
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



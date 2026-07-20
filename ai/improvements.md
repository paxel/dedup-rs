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


## Open issues & requests — 2026-07-18

### Sync groups & remote backup
- Backing up to a remote is currently manual: DUPLICATE a repo, RELOCATE the copy to the
  remote path, then UPDATE it. Make it a native feature. Mark repos as a **sync group**:
  one **main** repo plus one or more remote **sinks**. In the normal repo lists the sinks
  are **collapsed** (shown only when expanded) and otherwise treated as sinks of their main
  repo. A dedicated **Sync Groups** tab maintains the groups: run a sync (push main →
  sinks), detect **external changes in a sink** that may need migrating back to the main,
  resolve divergence, etc. It must also be possible to add existing repos to a group, an move repos out of a group
- new command in the transfer is for manually diffing two repos. (can also be two repos in a group but also outside of a sync group) creates a diff view of the files. 
  - you can diff by hash, if both exist and have same path: grey (default is hide equals completely), otherwise both yellow and two action buttons "rename" one in column 1 the other in column 4 and if the 1 is clicked the left repo file is renamed to the right repo name and vice versa if a file is missing on one side the side without the file gets a copy button and the other a delete button. thats basically all there is right?
  - you can diff by path, if both exist and have equal hash: grey (default is hide equals completely), otherwise we need to have some lightbox where we can display all the details of both files depending on their types. in the lightbox and in the table view we need the options to delete, rename or overwrite with other side is needed for both sides.
    - if one side is missing the copy and delete buttons as in the hash based diff is required
- we could also think about having size and modified date in the table.







# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.5.0] - Unreleased

### Added

- **APPLY CHANGES: a third sink push mode.** Between ADD ONLY and MIRROR: the
  push copies what the sink lacks *and* deletes from the sink what the main
  itself deleted — the backup follows the main's edits, while files the main
  never had stay untouched. Picked per sink on the Repositories tab (the mode
  pill is now three chips); a locked APPLY CHANGES sink is excluded from the
  push just like a locked MIRROR sink.
- **EMPTY DIRS cleans a group's sinks with its main.** Sinks are not offered
  as groomable repositories, so their stale directory skeletons had no way to
  be cleaned; running EMPTY DIRS on a backup group's main now sweeps every
  sink in the same run (empty directories hold no data, so sink locks don't
  apply), and the run report says how many repositories were swept.
- **SHOW IN BROWSE on every duplicate copy.** Right-click (or long-press) a
  copy in the Duplicates tab and pick SHOW IN BROWSE: the app switches to the
  Browse tab with that file selected in its folder — judge a duplicate by the
  company it keeps (siblings, naming, the directory it lives in) without
  hunting for it by hand.

### Fixed

- **AUTO-RESOLVE REST no longer leaves read-only-paired groups behind.** In a
  group holding one writable and one read-only copy, auto-resolve could end up
  marking nothing (it tried to mark the protected copy and was refused), so
  thousands of such groups survived every auto-resolve and only picked up
  marks page by page. It now keeps the protected copy and marks the writable
  duplicate — the same choice the group cards show.
- **No more dead broken-image cells on review rows.** A file with no preview
  yet (and audio whose fingerprint failed, e.g. some audiobooks) showed an
  inert broken-image glyph you could not click. Such audio now falls back to
  the byte-view preview like other opaque files, and the placeholder shown
  while any preview is still loading is styled like a real cell (extension
  chip, no broken glyph) and opens the viewer on click like everything else.
- **EMPTY DIRS no longer tells you to press a REVIEW button it doesn't have** —
  its hint now says to press RUN.
- **DELETE MARKED no longer looks like it deleted only one file.** The
  deletion always worked, but the follow-up re-search instantly replaced the
  "Deleted 45 file(s)" message with the fresh group count while the new first
  page's automatic marks refilled the button — reading as if nothing had
  happened. The status line now keeps the deletion summary next to the new
  group count.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.5.0] - Unreleased

### Added

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
- **DELETE MARKED no longer looks like it deleted only one file.** The
  deletion always worked, but the follow-up re-search instantly replaced the
  "Deleted 45 file(s)" message with the fresh group count while the new first
  page's automatic marks refilled the button — reading as if nothing had
  happened. The status line now keeps the deletion summary next to the new
  group count.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

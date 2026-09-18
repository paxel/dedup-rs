# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.8.0] - Unreleased

### Added

- **The activity window: long work runs alone, and shows itself.** A
  Duplicates FIND and DELETE MARKED now run in one central window that blocks
  the rest of the app while they work and shows what is happening — which
  index is being read, then "grouping images", "grouping videos", "grouping
  documents" and "comparing audio" with a percentage, elapsed time, an
  estimate of what is left, and each problem as it occurs — with a single
  CANCEL. When the work finishes the same window shows the result report.
  Nothing else that changes an index can start meanwhile, and a repository
  scan waits for it (and it for a scan). This is the first tab on the new
  standard ([ADR 0003](docs/adr/0003-every-action-reports-and-long-work-runs-alone.md));
  the other tabs follow one by one.
- **Notification cards and an event log.** A quick action — a group's DELETE
  NOW, an accept or un-accept — is answered at once by an animated card in
  the top-right corner naming the action, the file and the repository, with
  an unread count on the **LOG** button beside it. Every file the app
  deletes is appended to an event log (`events.jsonl` under the
  configuration directory, one JSON object per change, kept across
  restarts), and LOG opens it: newest first, with a filter box.
- **Duplicates asks its questions in order.** The tab shows the mode choice
  first — DUPLICATES or SIMILAR FILES, with the similarity threshold beside
  the SIMILAR chip and QUICK DELETE on the same row — then the repository
  chips once a mode is chosen, then the filter and FIND on one row once a
  repository is included. Flipping the mode keeps the repository pick. After
  a FIND the three sections fold into one summary line and CHANGE unfolds
  them.

### Changed

- **Similarity search reports its phases and compares audio on every
  core.** The search used to run silently and compare audio fingerprints one
  pair at a time; a large audiobook library looked frozen at 0 % for minutes.
  It now reports each phase with a percentage and scores the audio pairs in
  parallel, with the same groups as before.
- **A group's delete now says which files could not be removed.** The
  delete counts carry every removed and every failed file by name, so the
  event log and the result report can list them.

### Removed

- **MERGE REST BY NAME** on the DIFF board, added in 0.7.0, is gone again.
  Pairing leftovers by file name produced thousands of guessed rows that did
  not fit the transfer workflow: each one had to be settled by hand, and the
  board gave no feedback beyond redrawing without the file. The Chromaprint
  audio matching that shipped alongside it stays.

### Fixed

- **Turning a page in Duplicates starts at the page's first group.** The
  list used to keep the previous page's scroll position, so page two opened
  wherever page one had been scrolled to and had to be scrolled back up.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

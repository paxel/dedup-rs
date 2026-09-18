# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.8.0] - Unreleased

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

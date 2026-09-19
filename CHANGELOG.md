# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.8.1] - Unreleased

### Changed

- **A quick FIND no longer costs two clicks.** The activity window now waits
  two seconds before it appears, so a search that answers in less than that
  never puts a modal on screen at all, and a FIND that finishes without problems reports in
  a notification card that expires by itself instead of a window to dismiss.
  A FIND that hit problems, or was cancelled, still ends in the report.

### Fixed

- **The activity window keeps one width while it works.** During an UPDATE or a
  transfer RUN the window grew and shrank with every file, because the phase
  line was as wide as the path it names. The window now takes 80 % of the app
  window, between 560 and 720 points wide, and a path too long for it loses its
  head — the file name at the end stays readable, and the whole path is in the
  tooltip.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

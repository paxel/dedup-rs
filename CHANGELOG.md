# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.8.1] - Unreleased

### Fixed

- **The activity window keeps one width while it works.** During an UPDATE or a
  transfer RUN the window grew and shrank with every file, because the phase
  line was as wide as the path it names. The window now takes 80 % of the app
  window, between 560 and 720 points wide, and a path too long for it loses its
  head — the file name at the end stays readable, and the whole path is in the
  tooltip.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

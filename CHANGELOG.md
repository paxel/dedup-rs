# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.8.1] - Unreleased

### Changed

- **A FIND, a REVIEW or a PREVIEW folds the selection sections instead of
  hiding them.** In Duplicates, Transfer and Grooming the selection sections
  used to be replaced by one summary line with a CHANGE button, so changing
  one thing meant bringing the whole selection back. Every section — the
  FILTER included — now folds to its own header bar, and clicking one flips
  just that section open again.
- **A quick FIND no longer costs two clicks.** The activity window now waits
  two seconds before it appears, so a search that answers in less than that
  never puts a modal on screen at all, and a FIND that finishes without problems reports in
  a notification card that expires by itself instead of a window to dismiss.
  A FIND that hit problems, or was cancelled, still ends in the report.

### Fixed

- **The top bar's buttons stay visible and reachable.** The LOG button floated
  in the top-right corner on top of whatever was underneath — STATUS, HELP,
  ABOUT and SETTINGS among them — and the notification cards started high
  enough to cover them too, so a run of cards could hide HELP for half a
  minute. LOG is now part of the top bar beside STATUS, with the same unread
  count, and the cards begin below the bar.
- **The activity window keeps one width while it works.** During an UPDATE or a
  transfer RUN the window grew and shrank with every file, because the phase
  line was as wide as the path it names. The window now takes 80 % of the app
  window, between 560 and 720 points wide, and a path too long for it loses its
  head — the file name at the end stays readable, and the whole path is in the
  tooltip.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

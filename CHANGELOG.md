# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [3.0.0]

### Changed

- **The tabs are gone; the app opens on a start page.** Your repositories are
  cards on the left — click a name chip to select one or many, or ALL /
  LOCAL / NONE for the lot — and every command is on one list on the right,
  grouped by intent: INVESTIGATE (browse, find duplicates or similar files,
  diff, check), MAINTAIN (update, prune, reorganize, and a repository's own
  rename, relocate, clone, remove, remote flag and group membership), MOVE
  (copy, move, sync, mirror, group sync) and CLEAN (dedupe against, purge by
  filter, empty directories). A command the selection does not allow is
  greyed and says why on hover. UPDATE and CHECK run on the selection at
  once; a repository edit opens on its card; every other command opens its
  view under a header strip that names the selection and the command, with
  the repositories already filled in, and the START bar on the strip returns
  to the start page with the selection kept. A backup group's frame carries
  SELECT GROUP, which selects the main and every backup at once and gives
  GROUP SYNC its direction. The card's padlock is the session lock.

- **Audio has a seek bar, not a volume-slider lookalike.** The round-handled
  slider under a previewed track is gone. In Browse the position now runs
  along its own bar directly under the spectrogram — the same width as the
  picture, always there, the played part filled, a knob at the playhead that
  thickens as the pointer reaches it. Click or drag anywhere on it to seek.
  The audio rows in a Duplicates group carry the same bar.

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

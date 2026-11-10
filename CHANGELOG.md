# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [4.0.0]

### Added

- SYNC: push backup groups to their backups after a preview of what each one gains and
  loses; a backup can be left out of a run.
- COPY: copy what a repository and the reference repositories lack from several sources,
  each content once, optionally under a subfolder and a path template. A taken name gets a
  numeric suffix; nothing is overwritten.
- DELETE: delete from several repositories what the reference repositories already hold,
  with a switch for files a reference has deleted, and a warning for references updated
  before the repositories they are checked against.

### Changed

- The app opens on a repository page instead of the start page, and a bar along the top
  switches between REPOSITORIES, DUPLICATES, SYNC, COPY and DELETE. Each page chooses its
  own repositories; there is no shared selection any more.
- Repository page: a text filter and LOCAL / MAINS / ALL grey out the repositories that do
  not match; SELECT ALL / SELECT NONE, SET ALL RO / RW, and a thick border around a selected
  card. Backup groups never fold and are built from the cards' right-click menus.
- UPDATE and DELETE EMPTY DIRS first show what they would change, and act only after OK.
- DUPLICATES: drag repositories into a read-only and a read-write card, which lock and
  unlock them. PREVIEW opens the result in a window of its own, with the best copy of each
  group kept and the rest marked; GO deletes.

### Removed

- The start page's command list, header strip and favorites.
- From the GUI: CHECK, DIFF, MOVE, MIRROR, GROUP SYNC BACK, PURGE BY FILTER, ORGANIZE and
  PRUNE. The command line still has them.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

# Copy

Copy into one repository every file it lacks, from one or more sources, by content
(size + hash — paths never matter).

## Getting here

Click **COPY** in the [navigation bar](index.md#top-bar).

## The pool and the cards

Drag repositories from the pool into three cards:

- **COPY FROM — SAFE FROM CHANGES**: the sources. Nothing in them changes; a repository
  dropped here is locked.
- **COPY INTO — ONLY GAINS FILES**: the one target. Nothing in it is deleted or
  overwritten, so its lock stays as it is.
- **ALREADY KNOWN IN — SAFE FROM CHANGES**: further repositories whose content counts as
  already there. A repository dropped here is locked.

A file is copied when neither the target nor any reference has ever held its content —
not even as a file deleted there since — and no earlier source already brings it: content
found in two sources is copied once. A source or reference unlocked on another page no
longer fits its card and falls back into the pool, with a note saying why. The cards keep
their contents while the app runs and are empty after a restart.

## HOW: where the copies land

- **SUBFOLDER** — a folder inside the target the copies land under; empty means the
  target's root.
- **TEMPLATE** — a path template, as [ORGANIZE](grooming.md) uses: `{year}/{month}/{o-name}`
  sorts the copies by date. Empty keeps each file's path from its source.
- The [filter builder](index.md#filter-builder) limits which files take part.

Similar (not identical) files are not considered; look for them afterwards in the target
with [DUPLICATES](duplicates.md).

## PREVIEW, GO and CANCEL

**PREVIEW** lists every copy — source, path, and where it lands — in a window of its own;
the main window is locked until GO or CANCEL. A name already taken in the target is never
overwritten: the copy gets a numeric suffix (`photo (1).jpg`), and the preview marks it.

- **GO — COPY n FILE(S)** copies as listed; the progress and the report show in the preview
  window, and afterwards it plans again, so it shows what is left.
- **CANCEL** closes the preview and copies nothing.

Every copied file is written to the event log (**LOG**).

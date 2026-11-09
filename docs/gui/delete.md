# Delete

Delete from one or more repositories every file whose content a reference repository
already holds, by content (size + hash — paths never matter).

## Getting here

Click **DELETE** in the [navigation bar](index.md#top-bar).

## The pool and the cards

Drag repositories from the pool into two cards:

- **DELETE FROM — FILES HERE GET DELETED**: files in these repositories are deleted when a
  reference holds their content. A repository dropped here is unlocked.
- **ALREADY KNOWN IN — SAFE FROM CHANGES**: the references. Nothing in them changes; a
  repository dropped here is locked.

Content held only by the repositories you delete from — two of them sharing a file, but no
reference — stays: that is what [DUPLICATES](duplicates.md) is for. Content
[accepted](duplicates.md#accepted-content) in a repository is never deleted from it. A
repository whose lock is changed on another page no longer fits its card and falls back into
the pool, with a note saying why.

## HOW: what counts as already known

- **DELETE MISSING: TRUE / FALSE** — a reference that once held a file and has since deleted
  it only remembers it as a missing entry. With TRUE such a file is deleted here too, so a
  deletion in the reference carries over; with FALSE only content a reference really holds
  counts.
- A reference last updated before one of the repositories you delete from is flagged in
  amber: its index may claim files it no longer holds, so UPDATE it first.
- The [filter builder](index.md#filter-builder) limits which files take part.

Similar (not identical) files are not considered.

## PREVIEW, GO and CANCEL

**PREVIEW** lists every file that would be deleted in a window of its own, marking those
only a deleted reference copy points at; the main window is locked until GO or CANCEL.

- **GO — DELETE n FILE(S)** deletes as listed and marks the files missing in their index;
  the progress and the report show in the preview window, and afterwards it plans again.
- **CANCEL** closes the preview and deletes nothing.

Every deleted file is written to the event log (**LOG**).

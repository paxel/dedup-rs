# Sync

Push backup groups from their mains to their backups. A group, its main and its backups,
and each backup's push mode are set up on the [repository page](repository-page.md#backup-groups).

## Getting here

Click **SYNC** in the [navigation bar](index.md#top-bar).

## The pool and the card

The pool holds the main of every backup group. Drag a main into the **MAINS TO SYNC** card:
its backups come along, each with its stored mode.

- **ADD ONLY** copies what the backup lacks and never deletes anything there. The backup
  keeps its lock.
- **APPLY CHANGES** also deletes what the main itself deleted, and **MIRROR** deletes
  everything the main does not hold. Both can lose files in the backup, so the card unlocks
  those backups.

A backup's **IN THIS RUN** switch leaves it out of this run only; the group itself is not
changed. A main's **(x)** puts it back into the pool. If a backup that this run would delete
in is locked again on another page, its group falls back into the pool and the page says
why. The card keeps its contents while the app runs and is empty after a restart.

## PREVIEW, GO and CANCEL

**PREVIEW** plans every push in the activity window and opens the plan in a window of its
own: per group and backup what it would gain (`+`) and lose (`−`). The main window is locked
until that window ends with GO or CANCEL.

- **GO — PUSH** copies and deletes as listed. A group whose main has no indexed files is
  refused as a whole when any of its backups would lose files, because an unmounted main
  looks exactly like an emptied one. The progress and the report show in the preview
  window; afterwards it plans again, so it shows what is left.
- **CANCEL** closes the preview and changes nothing.

Every copied and deleted file is written to the event log (**LOG**).

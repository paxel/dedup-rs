# Sync Groups tab

Keep one repository backed up to one or more others, without the manual "duplicate the repo,
relocate the copy, rescan it" dance.

A **sync group** is one **main** repository plus the **sinks** it is pushed to. A repository
belongs to at most one group, and while it is in one it cannot be renamed or removed — the
group would otherwise be left pointing at something that no longer exists.

## Creating a group

Type a name in **NEW GROUP**, pick the repository that is the group's main, and press
**CREATE**. Only repositories that belong to no group are offered.

## Members

The selected group shows its **MAIN** and each **SINK**:

- **ADD SINK** — take a free repository into the group as a sink.
- **MAKE MAIN** — push from that sink instead; the previous main becomes a sink, so nothing
  leaves the group by turning it around.
- **TAKE OUT** — remove a sink from the group. Its files are untouched; only the grouping
  changes.
- **DELETE GROUP** — forget the group entirely. Every member keeps its files.

## Mode

The mode is configured **per group** and decides what a push does:

- **ADD ONLY** — copy content a sink lacks, and never delete anything from it. A sink may
  keep files the main no longer has.
- **MIRROR** — also delete sink content the main does not have, so each sink ends up holding
  exactly the main's content. Those deletions cannot be undone.

Because MIRROR deletes whatever the main lacks, a main that holds *nothing* would mean
"delete everything". A push is therefore refused outright when the main has no indexed
files — most often a main that was never scanned, or one whose drive failed to mount and
so scanned as an empty directory. Scan the main and try again. ADD ONLY never deletes and
is never refused.

Content is compared by size + BLAKE3 hash, never by path, so a file that already exists in
the sink under a different name is not copied again.

## Review and run

- **REVIEW** (`P`) plans every sink and shows the result in the shared review board: each
  row is a file to copy into a sink (marked *added*) or, in MIRROR mode, to delete from it
  (marked *removed*), with the sink named in the path so several sinks read as one list.
  Each row carries a thumbnail and the file's size, dimensions or duration, and date.
  Nothing on disk is touched.
- **RUN SYNC** (`R`) plans the push, then asks for confirmation stating how many files it
  will copy and — in MIRROR mode — how many it will delete, naming any sink the push would
  empty completely. It then pushes on a background thread. Sinks are handled one after
  another and independently, so one unplugged backup drive doesn't stop the others. The
  summary reports what was copied and deleted, and says plainly when a run fell short:
  which sinks failed, which were never reached (and are therefore stale), how many files
  failed to copy, and whether it was cancelled.

The main is never changed by a push. To bring a change that was made *inside a sink* back to
the main, use the [Files tab's](files.md#diff-board) **DIFF** command with the sink and the
main — its per-row COPY resolves exactly that.

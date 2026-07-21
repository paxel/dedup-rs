# Repo Sync tab

Two panes, switched at the top: **GROUPS** (keep repositories backed up to one another) and
**COMPARE** (diff every repo against one reference).

## Groups

Keep one repository backed up to one or more others, without the manual "duplicate the repo,
relocate the copy, rescan it" dance.

A **sync group** is one **main** repository plus the **sinks** it is pushed to. A repository
belongs to at most one group, and while it is in one it cannot be renamed or removed — the
group would otherwise be left pointing at something that no longer exists.

A sink is managed through its group's main, so the other tabs (Duplicates, Files, Grooming,
Browse) do not offer sinks in their repo pickers — only mains and ungrouped repositories.
Groups are created and managed here or on the [Repositories tab](repositories.md#sync-groups).

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

Each **sink** has its own mode (a pill next to it), so a group can mirror some backups and
only-add to others:

- **ADD ONLY** — copy content that sink lacks, and never delete anything from it. It may
  keep files the main no longer has.
- **MIRROR** — also delete sink content the main does not have, so that sink ends up holding
  exactly the main's content. Those deletions cannot be undone.

Because MIRROR deletes whatever the main lacks, a main that holds *nothing* would mean
"delete everything". A push is therefore refused outright when the main has no indexed
files *and any sink is set to MIRROR* — most often a main that was never scanned, or one
whose drive failed to mount and so scanned as an empty directory. Scan the main and try
again. A group whose sinks are all ADD ONLY never deletes and is never refused.

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

## Compare

The **COMPARE** pane diffs every repository against one you choose, by content (paths never
matter). Pick a **REFERENCE** repository and press **DIFF ALL**; each other repo then shows:

- **UNIQUE** — content only that repo has (the reference lacks it).
- **SHARED** — content both it and the reference hold.
- **MISSING** — reference content that repo does not have.

This is read-only — an overview to see at a glance which repos are ahead of, behind, or
overlap the reference. Nothing is changed; to act on a difference, use the [Files tab's](files.md#diff-board)
**DIFF** command on the pair.

# Transfer tab

![Transfer tab](../screenshots/files_tab.png)

Copy, move, or delete files between repositories by content (size + BLAKE3 — paths never
matter), with an assisted filter builder and a review before anything runs. This is the GUI
equivalent of [`diff cp`/`mv`/`rm`](../cli.md#diff).

## Repo pickers

- **SOURCE** — the repository whose files are considered for transfer/deletion.
- **TARGET** — where COPY/MOVE places files (and always a reference for "already known"); for
  DELETE, the reference whose known content makes source files deletable.
- **ALSO REF** — optional extra reference repositories. A source file counts as new only when
  **none** of the target or these extra references already has its content — this is what
  makes multi-disk triage correct (unique against the destination repo *and* every
  already-processed disk, not just one).

## Command

- **COPY** — copy source files the target (and ALSO REF repos) doesn't have into the target
  repo. Source files are left in place.
- **MOVE** — same, but marks the source entries missing afterward.
- **SYNC** — copy source content the target lacks into it at the same relative path; turn on
  **DELETE MISSING** to also delete target files whose content the source has since lost. The
  source is never changed.
- **MIRROR** — copy source content the target lacks *and* delete everything in the target the
  source does not have, so the target ends up holding exactly the source's content. Deletions
  cannot be undone.
- **GROUP SYNC** — push a backup group's main to some or all of its sinks, each in its own
  stored mode. Only offered when SOURCE is a group's main; see [Group sync](#group-sync)
  below.
- **GROUP SYNC BACK** — the reverse: pull one sink's changes *back* into the main. Also only
  offered when SOURCE is a group's main; see [Group sync back](#group-sync-back) below.
- **DIFF** — compare the two repos side by side and resolve the differences yourself, one
  row at a time (see [Diff board](#diff-board) below). Nothing runs as a batch.

## Into (subfolder)

For COPY/MOVE, an optional relative subfolder inside the target — files keep their
source-relative path underneath it (e.g. `photos/2020/a.jpg` into `imports/batch1` lands at
`<target>/imports/batch1/photos/2020/a.jpg`). Leave blank to place files at the target root.
**BROWSE** opens a native folder picker rooted at the target to pick or create the subfolder.

## Filter builder

Pressing **+** offers MIME / NAME / SIZE condition types; each added condition becomes a
removable pill with an inline editor (multiple conditions combine with AND):

- **MIME** — matches files whose detected MIME type contains a substring (clickable
  suggestions from the source repo's actual MIME types, with counts, narrowed as you type).
- **NAME** — matches files whose path contains a substring (a live, debounced match count
  shows against the source repo). A **NOT** toggle inverts a condition (`NOT NAME: *thumb*`
  keeps everything that is *not* a thumbnail), and an **Aa** toggle makes text matching
  ignore capitalisation.
- **SIZE** — an operator + byte count, e.g. `>=1000`.

Every kind offers previously used values as one-click quick-picks, remembered across
sessions. **CLEAR** removes every condition.

**Presets**: save the current condition set under a name for one-click reuse (**SAVE**),
apply a saved preset (click its pill), or forget one (**×**). **EXPORT**/**IMPORT** move the
whole remembered-value + preset history to/from a JSON file, for backup or sharing between
machines.

## Diff board

![The DIFF board](../screenshots/transfer_diff_board.png)

**DIFF** compares the source (left) and target (right) repo on the same
[review board](index.md#the-review-board) every other preview uses: each side's paths, size
and date, with the commands between them.

**PAIR BY** decides what counts as one row:

- **BY HASH** — files are matched by content, so the same photo under two names is a single
  row. Same content at the same path is *equal*; same content under different names offers
  **RENAME** on each side (renaming that side's file to the other's name); content only one
  side has offers a **COPY** into the side that lacks it and a **DELETE** on the side that
  holds it. Where two names differ, the differing characters are highlighted on each
  side, so you can see at a glance whether the difference is a suffix, a counter or a
  different extension. The comparison ignores the folder the files sit in, and an inserted
  character marks only itself rather than everything after it.
- **BY PATH** — files are matched by their path inside the repo. Same path with the same
  content is *equal*; same path with different content is a conflict, offering
  **OVERWRITE** on each side (replacing that side's file with the other's) and **DELETE** on
  each side. Clicking the row opens the two versions in the viewer.

Above the board, **ALL LISTED** offers bulk actions for reconciling large repositories:
**COPY MISSING >** / **< COPY MISSING** send everything only one side has to the other, and
**RENAME ALL L** / **RENAME ALL R** rename each side's files to the other's names. Only actions
the rows on screen can actually use are offered. They act on every row *currently listed* — so
hiding a row is how you leave it out — and a confirmation states the exact count first. If some
operations fail the summary says how many succeeded and how many did not.

**Clicking a row** opens the two versions side by side over the whole window, in the same
shared viewer every surface opens — so what you get depends on the file type, not on which tab
you happen to be in. The [viewer](duplicates.md#the-viewer-lightbox) is tabbed by
representation: two images compare as pictures (wheel-zoom, drag-pan, Space to flicker), two
videos as aligned filmstrips with a shared playhead, two audio files as spectrograms with
gapless A/B playback, two documents as line-aligned extracted text, and any two files as an
aligned hex diff — each side also showing the repo it lives in and its size, date and type,
with the larger size and newer date highlighted. A side with nothing to show says so. The same
OVERWRITE OTHER / DELETE actions are available per side inside the comparison, so the decision
is made where it is being judged. `Esc` steps back out of flicker, then closes; **CLOSE**
leaves without changing anything.

![DIFF compare — two versions of the same path side by side](../screenshots/diff_compare.png)

When one side holds the same content under several names, that side is narrowed down first:
**DEL ALL** drops every copy on that side (after confirming) and
**KEEP 1** asks which copy to keep and deletes the others. When the *other*
side offers several names, RENAME asks which name to take. After every action the board
re-compares the two repos, so the row's commands always reflect the current state.

Equal rows are hidden until **SHOW UNCHANGED** is pressed, and each action is applied to disk
and to both repo indexes immediately — there is no RUN button and no batch confirmation.
**HIDE** parks a row you have decided to leave alone; it comes back on the next REVIEW.

## Group sync

Keep a repository backed up to one or more others, without the manual "duplicate the repo,
relocate the copy, rescan it" dance. Groups themselves (creating one, adding/removing sinks,
setting each sink's mode) are managed on the [Repositories tab](repositories.md#sync-groups);
this tab is where a group is actually *pushed*.

![GROUP SYNC selected: TARGET hidden, SINKS shown](../screenshots/transfer_group_sync.png)

Pick the group's main as **SOURCE** — its chip carries the **★ MAIN** badge — and the
**GROUP SYNC** command appears. Selecting it replaces the single **TARGET** picker with a
**SINKS** panel — every sink of that main's group, defaulting to all selected; **ALL**/**NONE**
toggle the whole set, or click a sink to include or exclude it. Each sink chip is followed by
its stored mode as **MODE: …** — **ADD ONLY** copies content it
lacks and never deletes, so a sink may keep files the main no longer has; **MIRROR** also
deletes sink content the main does not have, so it ends up holding exactly the main's
content — those deletions cannot be undone.

**REVIEW** and **RUN** work as they do for every other command: REVIEW plans every selected
sink and shows what would be copied and deleted, without touching disk — each row naming the
sink it belongs to. A group push is all-or-nothing, so these rows carry no per-row commands; RUN asks for
confirmation — naming the sink count and, for a MIRROR push, any sink it would empty
entirely — then pushes on a background thread. Sinks are handled independently, so one
unreachable backup drive does not stop the others, and the main is never changed. The FILTER
wizard applies here too, narrowing which files count for every selected sink.

## Group sync back

Sometimes the change is on a **sink** — you dropped new files straight onto a backup drive, or
you deleted something from the main *by mistake* and a backup still has it. **GROUP SYNC BACK**
pulls one sink back into its main. It appears next to GROUP SYNC when the SOURCE is a group's
main; you pick **one sink** to reconcile.

![GROUP SYNC BACK: a green new-file row and a blue resurrection row](../screenshots/group_sync_back.png)

REVIEW sorts the sink's files against the main into three kinds:

- **New** (green) — content the main never had. These are your direct edits. **RUN** promotes
  them all into the main in one batch.
- **Resurrection** (blue) — content the main once had and **deleted**, that the sink still
  holds. These are **never** promoted by the batch — resurrecting a file undoes a deletion, and
  only you know whether that deletion was a mistake or deliberate.
- **Path conflict** (amber) — the main has a **different file at that exact path**, so a plain
  promote is impossible (nothing is ever overwritten silently). The main's cell shows its own
  occupying file, so you judge by looking at both. The row offers **`< OVERWRITE`** — replace
  the main's file with the sink's — shown only while the **main** is unlocked, since it deletes
  the main's current version. The batch never touches these rows.

Each row is a full triage decision: **`< COPY`** pulls *just that file* into the main (the only
way a resurrection comes back), and **`DELETE`** on the sink's side removes it there instead —
for the files that turn out to be worth neither keeping nor promoting. It appears only while the
sink is **unlocked** (its padlock in the SINK panel); promoting is never barred, because adding
to the main loses nothing. Clicking a row opens the file itself in the viewer.

Content already in the main is skipped, and a **RESURRECTIONS ONLY** toggle above the rows hides
everything but the blue ones when you want to focus on what would come back. Nothing on the sink
is ever changed. Note that a **MIRROR** sink deletes any direct edits on the next push, so run
GROUP SYNC BACK *before* you push again.

## Review and run

- **REVIEW** shows the first matching transfers (up to a limit) and a total count, without
  touching disk, on the shared [review board](index.md#the-review-board).

  ![A COPY preview on the review board](../screenshots/transfer_review_board.png)
 Each side carries a
  thumbnail (image, video still, audio fingerprint, or a text file's first
  lines — hover the small cell to read the whole preview) and the file's size, dimensions or
  duration, and date — the same info as a Duplicate card. Per row, **APPLY** runs just that
  transfer now and **HIDE** drops it from the board and from what RUN will do. REVIEW and RUN
  are mutually exclusive — starting a run clears the review and vice versa.
- **RUN** starts the command on a background thread after a confirmation dialog. Live
  progress shows a spinner, the file currently being handled, the last few actions, and a
  running count. **CANCEL** stops the operation — files already transferred or deleted before
  cancelling stay as they are; this doesn't roll back, it just stops further work.
- Both repos' indexes are kept in sync as a run proceeds: COPY/MOVE record each transferred
  file in the target's index (with its real on-disk mtime, plus provenance — which repo it
  came from); MOVE/DELETE mark the source entries missing.

# A repository page, and one page per process

Decided on 2026-11-08, after the app's only user tried 3.1.0 and found it
unusable. This decision supersedes
[ADR 0004](0004-start-page-hub-and-one-board.md) (start page hub, one shared
selection, the command list, the header strip). The feedback half of
[ADR 0003](0003-every-action-reports-and-long-work-runs-alone.md) stays in force.

## Why

- Repositories were the one central selection. Which ones had to be selected
  or deselected to enable a command was guesswork.
- It was often hard to see which repositories were selected, and a folded
  group hid whether its members were selected.
- After choosing a command, the full command list stayed in reach, so a
  different command that did not fit the selection could be picked.

## Pages and navigation

One bar along the top of every page: **REPOSITORIES · DUPLICATES · SYNC · COPY ·
DELETE**. The active page is highlighted; a click switches directly; the app
opens on REPOSITORIES.

Each process page chooses its own repositories, in role cards that fit the
process. There is no selection shared between pages.

Everything outside these five pages (browse, diff, check, organize, purge by
filter, prune, favorites, group sync back) leaves the GUI until it gets a page
of its own. The CLI keeps all of it. The shared file viewer stays, as the
preview uses it.

## The repository page

**Left half:** the repository cards, about half the width, behind a movable
separator. Nothing folds. A backup group is a frame around its main and sinks.
There is no dragging on this page. Groups are built from the card's
right-click menu: MAKE MAIN, JOIN GROUP (a list of the mains), LEAVE GROUP,
and UNGROUP in the frame header's menu. A sink's mode is a switch on the sink's
card.

**Right half:**

- A text filter with an `x` to clear it, and a filter row **LOCAL / MAINS /
  ALL** (one at a time; ALL is no filter). A repository that does not match is
  greyed out and sorted to the bottom, never hidden. Greyed out means immune:
  it cannot be selected and no command or bulk switch touches it.
- **SELECT ALL / SELECT NONE**, acting on the repositories that are not greyed
  out. A selected card gets a big, bright border around the whole card.
- **SET ALL RO / SET ALL RW**: locks or unlocks every repository that is not
  greyed out.
- The repository commands, acting on the selection:
  - **UPDATE** and **DELETE EMPTY DIRS** investigate first, then show the
    estimated changes (UPDATE: new, changed, missing, which is today's CHECK;
    DELETE EMPTY DIRS: the folders), then run only after OK, with a progress
    bar. CANCEL is offered at every step.
  - **REFRESH STATUS** changes nothing and runs at once.
  - These run in a modal inside the main window.

## Process pages: shared rules

- **Pool.** All repositories in compact form. A repository dragged into a card
  leaves the pool; the `(x)` on it in the card puts it back.
- **Cards set the lock.** The session lock stays the one protection, and a
  card states it:
  - unlocks: DELETE's "delete from", SYNC sinks in APPLY CHANGES or MIRROR
    mode, DUPLICATES' read-write card;
  - locks: "already known in" (COPY, DELETE), "copy from", DUPLICATES'
    read-only card;
  - leaves it alone: "copy into" (COPY only adds files), SYNC sinks in ADD
    ONLY mode.
  Card titles say what happens there, e.g. "safe from changes" or "copies
  here get deleted".
- **Cards follow the lock.** The lock belongs to the repository, not the page.
  On DUPLICATES a repository whose lock changed elsewhere moves to the matching
  card. A repository whose new lock does not fit its card falls back into the
  pool, and the page says why.
- **Memory.** The cards keep their contents while the app runs and start empty
  after a restart, matching every repository starting locked at launch.
- **Filter card.** DUPLICATES, COPY and DELETE take the shared filter
  (`mime:image`, `!name:*.db`, …); only files that pass take part.
- **PREVIEW** opens the result in a real second OS window: movable to another
  monitor, resizable, minimisable. The main window is locked until the preview
  ends with CANCEL or GO. The preview can be sorted and filtered, but not
  changed; to change the result, CANCEL and change the inputs. After GO the
  same window shows the progress, then the report.

## The pages

**DUPLICATES.** A read-only card and a read-write card; clicking a repository's
padlock in a card moves it to the other card. An EXACT / SIMILAR switch, with
the threshold for SIMILAR. The preview is today's group view without the
controls it no longer needs. Copies only in read-write repositories: the BEST
copy is kept and every other copy is marked, as AUTO-RESOLVE does today.

**SYNC.** Mains are dragged into a "mains to sync" card; each brings its stored
sinks with their stored modes, and a sink can be left out of this run with its
`(x)`. Groups stay defined on the repository page.

**COPY.** Several sources ("copy from"), one target ("copy into") and the
references ("already known in"; the target always counts as one). A file is
copied when no reference, the target or an earlier source already has its
content, so content in two sources is copied once. An optional subfolder in
the target, and under it an optional path template (`{year}/{month}/{o-name}`).
Nothing is overwritten: an occupied name gets a numeric suffix, marked in the
preview. Exact content only; similar files are found afterwards with
DUPLICATES.

**DELETE.** Several repositories to delete from, and the references ("already
known in"). A file is deleted when a reference holds its content. Content held
only by the repositories being deleted from stays; that is DUPLICATES' job.
Only entries that are not MISSING count, and a reference whose last UPDATE is
older than the source's is flagged on its card and in the preview. A switch
**DELETE MISSING: TRUE / FALSE** decides whether content that a reference only
holds as a MISSING entry (deleted there) is deleted from the source too. Exact
content only.

## Consequences

- The start page, its command list, the header strip and the favorites go.
  `CONTEXT.md`'s Selection, Command and Header strip need rewriting.
- Group management moves from selection-dependent commands to card menus.
- The help text and `docs/gui/` are rewritten per page.

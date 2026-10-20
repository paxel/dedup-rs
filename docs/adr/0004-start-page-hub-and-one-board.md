# The start page is the hub, and every result is one board

Decided on 2026-10-20, after the app's only user reported that its five tabs
"feel like five apps developed by different people". The feedback half of
[ADR 0003](0003-every-action-reports-and-long-work-runs-alone.md) stays in
force; its **Layout** section (one reading order per tab) is superseded by
this decision, because the tabs themselves go.

**Start page.** The app opens on one page: the registered repositories as
**repo cards** (identicon, name, path, file count, size, last update age, a
stale warning, the session lock, the main badge), a `+` card to add one, a
totals line (repositories, files, bytes), a row of **favorites**, and the last
few runs from the event log. Repos that form a backup group sit inside a named
**frame**, main first; clicking the frame header selects the whole group.
Cards are multi-select. There is no tab bar.

**Commands.** Beside the cards, the **command list**: every operation the app
offers, grouped by intent — *Investigate* (browse, find duplicates, find
similar, diff, check), *Maintain* (update, prune missing, reorganize, rename, relocate, clone, remove, the remote flag, group membership),
*Move* (copy, move, sync, mirror, group sync), *Clean* (dedupe against, purge by
filter, empty directories). The list is always
complete and never reorders. A command the current **selection** does not
allow is greyed, and its hover text says why: needs exactly one repository,
needs two, needs a backup group, repository is locked. A locked repository
greys every command that deletes or overwrites in it; additions stay allowed,
as the lock has always worked.

**Progressive inputs.** Choosing a command reveals, beside it, only the inputs
that command still needs: a filter, a mode or threshold, from/to roles. A
selection that is exactly a backup group gives the direction for free (main is
the source, sinks are the targets); any other selection of two repositories
gets explicit *from* and *to* roles. Selection order never means anything.
Changing the selection resets the command and its inputs. A fully specified
command can be saved as a **favorite**: a named card on the start page that
re-opens that command with those repositories and inputs in one click.

**Work in place.** Running a command collapses the cards and the command list
into a **header strip** — which repositories, which command, which inputs — and
the result fills the rest of the window. Clicking the strip returns to the
start page with the selection intact.

**One board.** Every result is one **board**: a table of file rows (thumbnail,
name, repository, size, planned action, status). Duplicates are groups of rows,
a diff's rows carry left and right, a reorganize's rows carry from and to, a
browse is plain rows in a folder. The board has one selection model, one mark,
one row-action menu, and a click on any row opens the shared full-window
viewer. There is no card grid and no per-command result layout.

**Preview, then APPLY.** Every command that writes to disk fills the board
with its proposed rows without touching anything, lets the user adjust marks,
and offers one `APPLY` button, which runs behind the activity modal. Commands
that only read (browse, check, find, diff) fill the board directly; where their
board carries marks (a duplicates find, a diff), it offers the same `APPLY`.
A favorite lands on the preview too: it saves the specification, never the
confirmation.

**Skin, not costume.** The LCARS colours, condensed font and stadium buttons
stay as the app's identity. The LCARS layout rules — elbow rails, side bars,
uppercase labels on everything — are dropped wherever they cost a board width
or legibility.

**Shell first.** The reshape lands in that order: the start page replaces the
tab bar while commands still open today's tab bodies under the header strip;
then each body is converted to the one board, command by command; then the skin
pass.

## Why

- Every tab opened with its own repository picker, held its own selection and
  re-synced it on every switch, so the user picked the same repositories five
  times per session and the tab bar was the only thing the five tools shared.
  One selection, picked once, makes the operations views of one thing.
- Every tab answered with its own surface — a card grid, two review boards, a
  conflict board, a listing — each with its own marks and row actions. One
  board means one set of habits, and the visual comparison those surfaces
  tried to carry already lives in the shared viewer.
- The same lifecycle wore three vocabularies: FIND / DELETE MARKED, REVIEW /
  RUN, REVIEW / per-row. Preview then APPLY is one habit, and the one click it
  costs is the click that stands between a saved favorite and a mirrored-away
  sink.
- The repository is the central concept of the data model (groups, mains,
  locks hang off it), so the repositories are the right thing to put first,
  and the start page's cards are where a stale index or a locked repo can be
  seen before any command is chosen.
- The user judges by using. Replacing the shell first puts the most uncertain
  decision — the hub — in their hands before the long board conversion.

## Consequences

- `Tab` and the tab strip go. The `Repositories` tab's cards become the start
  page; its UPDATE / CHECK / REMOVE / group management become commands.
- Each view loses its own repository picker and receives its repositories from
  the start page selection. Until a view is converted to the board, it keeps
  its body and folds its old selection sections shut.
- The board becomes a shared module, as the viewer already is. Views that
  carry marks, row actions and plans converge on its row model.
- Favorites are persisted in the settings file, as a name plus the command's
  specification (repositories by name, inputs). A favorite whose repository no
  longer exists shows as unavailable, with the reason.
- The GUI guide is rewritten around the start page, the command list and the
  board instead of five tabs.

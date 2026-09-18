# Grooming tab

![A PURGE preview on the review board](../screenshots/groom_purge_board.png)

Tidy a single repository in place: remove redundant copies, delete files by filter, clear
empty directories, reorganize files into a dated/templated tree, or drop stale index records.
Every tool previews on the shared [review board](index.md#the-review-board) first — nothing
touches disk until you RUN.

## Reading order

The tab asks its questions top to bottom: **WHAT** (the tool), **WITH WHICH** (the
repository, plus the dupe pool for DEDUPE), **HOW** (the filter or the rules) and only then
**RUN**. Each section appears once the one above it has an answer — a fresh tab shows only
the tool chips — and keeps its answer when an earlier one changes. Once REVIEW or RUN
starts, the sections above fold into one summary line; **CHANGE** unfolds them, and the RUN
section stays so a reviewed plan can be run.

## What: the tool

Pick a grooming tool with the chips (or the number keys `1`–`5`):

- **DEDUPE** — find files in this repo whose content also exists **elsewhere** (in other
  repositories you pick as references) and delete the redundant local copies. Each row names
  the surviving copy that makes the local one redundant, so the list reads "this goes, because
  that stays" — and a **COMPARE** button opens the pair in the shared viewer so you can look
  before you delete. Your repo is only ever trimmed of content that is safely held somewhere
  else — and never of content you [accepted](duplicates.md#accepted-content) in it: accepted
  files are left out of the REVIEW preview and of the run.
- **PURGE** — delete files in this repo matching a [filter](transfer.md#filter-builder).
  Purge needs at least one condition: with no filter nothing matches and nothing is deleted,
  so you can never empty a repo by leaving the filter blank. Useful for sweeping out a class of
  files (thumbnails, a MIME type, everything over a size).
- **EMPTY DIRS** — remove directories left empty on disk (e.g. after a MOVE or DEDUPE). The
  index is unaffected; this only tidies the folder tree.
- **ORGANIZE** — move files into a folder tree built from a **path template**
  (`{year}/{month}/{o-name}` and friends), typically by capture/modification date, so a flat
  dump becomes a browsable `2019/05/…` structure. Rules nest, and the template is editable
  inline with clickable field tokens.
- **PRUNE** — drop index records for files that are no longer on disk (the **missing**
  entries). This is index hygiene only — it removes tracking records, never files.

## With which: the repository

- **REPO** — the single repository to groom. DEDUPE instead picks a **SOURCE** and the
  **DUPEPOOL** repositories it checks against.

## How: filter or rules

- **FILTER** — the same condition builder the [Transfer tab](transfer.md#filter-builder) uses.
  Required for PURGE; optional for DEDUPE, where it narrows which files a preview considers.
- **RULES** — ORGANIZE's ordered list of filter + path template pairs (see the tool above).
  EMPTY DIRS and PRUNE have nothing to set.

## Run

**REVIEW** and **RUN** are the two run buttons; everything above them is a selection.

- **REVIEW** plans in the [activity window](index.md#the-activity-window) — "reading
  'repo'", "pairing files" for DEDUPE — which closes by itself once the board is ready, without
  touching disk. PURGE, DEDUPE and ORGANIZE rows show what would be deleted or moved (DEDUPE
  naming the surviving copy, ORGANIZE the destination path); **HIDE** parks a row so RUN skips
  it. PURGE and PRUNE rows are one-sided and carry no COMPARE (there is no counterpart to look
  at). A row's **APPLY** runs just that row as a quick action: a notification card names the
  file once it is done, the event log keeps the line, and the board refreshes.
- **RUN** plans first (so the confirmation can state the exact count), asks, then applies the
  plan in the activity window: it names each file as it is deleted or moved, counts up to the
  total, lists each problem, and offers **CANCEL** — which stops further work without rolling
  back what already ran. When the run ends the same window shows the result report, and every
  file it changed is in the event log (**LOG**, top right). EMPTY DIRS reports how many
  directories it removed per repository. Only one operation runs at a time: a second REVIEW or
  RUN while one is up is refused with a card naming what is still running.

# Files tab

![File Management tab](../screenshots/files_tab.png)

Copy, move, or delete files between repositories by content (size + BLAKE3 — paths never
matter), with an assisted filter builder and a preview before anything runs. This is the GUI
equivalent of [`diff cp`/`mv`/`rm`](../cli.md#diff).

## Repo pickers

- **SOURCE** — the repository whose files are considered for transfer/deletion.
- **TARGET** — where COPY/MOVE places files (and always a reference for "already known"); for
  DELETE, the reference whose known content makes source files deletable.
- **ALSO REF** — optional extra reference repositories. A source file counts as new only when
  **none** of the target or these extra references already has its content — this is what
  makes multi-disk triage correct (unique against the sanitized dir *and* every
  already-processed disk, not just one).

## Command

- **COPY** — copy source files the target (and ALSO REF repos) doesn't have into the target
  repo. Source files are left in place.
- **MOVE** — same, but marks the source entries missing afterward.
- **DELETE** — delete source files whose content the target (or an ALSO REF repo) already
  has; nothing is written to the target.

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
  shows against the source repo).
- **SIZE** — an operator + byte count, e.g. `>=1000`.

Every kind offers previously used values as one-click quick-picks, remembered across
sessions. **CLEAR** removes every condition.

**Presets**: save the current condition set under a name for one-click reuse (**SAVE**),
apply a saved preset (click its pill), or forget one (**×**). **EXPORT**/**IMPORT** move the
whole remembered-value + preset history to/from a JSON file, for backup or sharing between
machines.

## Preview and run

- **PREVIEW** shows the first matching `from → to` transfers (up to a limit) and a total
  count, without touching disk. PREVIEW and RUN are mutually exclusive — starting a run
  clears the preview and vice versa.
- **RUN** starts the command on a background thread after a confirmation dialog. Live
  progress shows a spinner, the file currently being handled, the last few actions, and a
  running count. **CANCEL** stops the operation — files already transferred or deleted before
  cancelling stay as they are; this doesn't roll back, it just stops further work.
- Both repos' indexes are kept in sync as a run proceeds: COPY/MOVE record each transferred
  file in the target's index (with its real on-disk mtime, plus provenance — which repo it
  came from); MOVE/DELETE mark the source entries missing.

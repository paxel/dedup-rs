# Repositories tab

![Repository Management tab](../screenshots/repositories_tab.png)

Register, scan, and manage repositories — a repository is a name linked to a folder on
disk, tracked by its own index. This tab is the GUI equivalent of the [`repo`
subcommand](../cli.md#repo).

## Action bar

- **ADD REPOSITORY** — opens a dialog to register a new folder (see below). Disabled while
  a scan is running elsewhere in the app.
- **UPDATE ALL** — queues an UPDATE / SCAN for every registered repository, one at a time.
  Already up-to-date repos finish almost instantly.
- **REFRESH STATUS** — re-checks every repository's location/reachability and whether its
  index is stale (a dry-run — no hashing, no writes). This is what produces the LOCAL /
  REMOTE / OFFLINE / MISSING and UP TO DATE / UPDATE REQUIRED pills on each card.

## Repository cards

Each card shows the repository's name, its on-disk path, status pills, a MIME-type
breakdown (top few types by share, pinned top-right), and stats:

| Stat    | Meaning                                                                                                                                                                |
| ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| FILES   | Indexed files (files that vanished from disk since the last scan are excluded).                                                                                        |
| SIZE    | Total on-disk size of indexed files.                                                                                                                                   |
| MISSING | Files indexed before but no longer found on disk — kept as history, excluded from stats and duplicate search until a rescan confirms they're back.                     |
| SCANNED | When this repository was last scanned ("never" if it hasn't been).                                                                                                     |
| TRIAGED | Only shown once triage-done: when this repo's unique content was already copied into a sanitized directory (via `dedup sanitize` or the Files tab's MARK SOURCE DONE). |

Status pills:

- **LOCAL** / **REMOTE** — the folder is on this machine, or on a reachable network mount.
- **OFFLINE** — a network mount that isn't reachable right now; scans and checks will fail
  until it's back.
- **MISSING** — the local folder no longer exists or can't be read; RELOCATE it or restore
  the folder before scanning.
- **UP TO DATE** / **UPDATE REQUIRED** — from the last CHECK: whether there are new,
  changed, or missing files since the last scan.

## Per-repository actions

- **UPDATE / SCAN** — walk the folder, hash new/changed files, mark vanished files missing.
  Fast on a repeat run since unchanged files are skipped.
- **CHECK** — dry-run: report new/changed/missing counts without hashing or writing
  anything. Use this to see if UPDATE / SCAN has real work to do.
- **RENAME** — change the repository's registry name in place; the on-disk folder is not
  moved.
- **RELOCATE** — point the repository at a different on-disk folder, keeping its existing
  index. Use this after moving the data to a new location.
- **DUPLICATE** — clone the entire index into a brand-new repository at a new path. The
  source is left completely unchanged — for branching off a snapshot, not moving anything.
- **DELETE** — remove the registry entry and its index database. The on-disk files it
  tracked are **never touched**; only the tracking record disappears.

A repository mid-scan or mid-check shows a spinner, live progress (a bar with percent/count
for hashing, a file/dir count while scanning), and a CANCEL button — cancelling a scan keeps
whatever was already hashed committed to the index.

## Add Repository dialog

- **FOLDER** — type a path, or **CHOOSE…** to open a native folder picker.
- **NAME** — the repository's display name; leave blank to default to the folder's own
  name. Must be unique among registered repositories (a clash is flagged in red before you
  can submit).
- **ADD** registers the repository (it is not scanned automatically — run UPDATE / SCAN
  afterwards); **CANCEL** closes without registering anything.

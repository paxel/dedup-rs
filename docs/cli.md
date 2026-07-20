# CLI reference

`dedup` with no arguments opens the GUI. Every subcommand below runs headless. This is a
deeper, example-driven walkthrough; the [README](../README.md#cli-commands) has a
quick-reference table.

All commands operate on **registered repositories** — a repository is a name linked to a
folder on disk, tracked in an index at `~/.config/dedup/repos/<name>/index.redb` (or under
`$XDG_CONFIG_HOME/dedup` when that is set). Each run also writes a session log to
`$XDG_STATE_HOME/dedup/logs`, defaulting to `~/.local/state/dedup/logs`; the ten newest are kept.
Register
one before anything else will work:

```
dedup repo create photos ~/Pictures
dedup repo update photos
```

- [`repo` — register and scan repositories](#repo)
- [`diff` — compare and transfer by content](#diff)
- [`timeline` — browse/export by date](#timeline)
- [`report` — Markdown triage report](#report)
- [`scan` — flag critical files](#scan)
- [`archive` — index and check archive redundancy](#archive)
- [Filter syntax](#filter-syntax)

---

## `repo`

Repository lifecycle management.

| Command                                                             | What it does                                                                                                                                                                                                                                                                                                                    |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `dedup repo create <name> <path>`                                   | Register a new repository. Fails if the name is already taken.                                                                                                                                                                                                                                                                  |
| `dedup repo ls`                                                     | List every registered repository with its stats (file count, size, missing count, last-scan time) — reads maintained counters, never rescans.                                                                                                                                                                                   |
| `dedup repo rm <name>`                                              | Remove the registry entry and delete its index database. **The on-disk files it tracked are never touched** — only the tracking record disappears.                                                                                                                                                                              |
| `dedup repo mv <name> <new_name>`                                   | Rename a repository's registry entry (its target folder is unchanged).                                                                                                                                                                                                                                                          |
| `dedup repo rel <name> <new_path>`                                  | Point a repository at a different folder while keeping its existing index — use after moving the data.                                                                                                                                                                                                                          |
| `dedup repo cp <source> <dest> <path>`                              | Clone `source`'s entire index into a new repository `dest` at `path`. `source` is left completely unchanged; this is for branching off a snapshot, not moving anything.                                                                                                                                                         |
| `dedup repo update <name>... \| -a/--all [-t/--threads N]`          | Walk the repository's folder, hash new/changed files (in parallel across `--threads` threads; `0` = one thread per CPU core), and mark vanished files missing. Already-hashed unchanged files are skipped, so a repeat run is fast. Shows live progress and can be cancelled with Ctrl-C — already-hashed files stay committed. |
| `dedup repo dupes <name>... \| -a/--all [--threshold N] [--delete]` | Find exact duplicates, or with `--threshold <1-100>` perceptually similar files (`similarity % = (1 − hamming_distance / bits) × 100`). `--delete` removes every copy except the best one per group.                                                                                                                            |

Alongside the content hash, `update` computes a perceptual fingerprint by MIME kind: a
512-bit image hash (rotation/mirror invariant), a 512-bit-per-frame video temporal hash
(three frames via `ffmpeg`/`ffprobe` — install ffmpeg to enable it; degrades to content-hash-only
without it), a normalized-text hash for PDFs and office documents (docx/xlsx/pptx/odt/ods/xls),
a duration + chunk hash for audio, and EXIF capture date/camera for images.

---

## `diff`

Compare a **source** repository against one or more **reference** repositories by content
(size + BLAKE3 — paths never matter), and optionally transfer or delete based on the result.

| Command                                                                                       | What it does                                                                                                                             |
| --------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `dedup diff print <source> <reference> [--ref <repo>...] [-f filter]`                         | Classify every source file as New (no reference has it), Equal (a reference already has it — reports which path), or DeletedInReference. |
| `dedup diff cp <source> <reference> <target> [--ref <repo>...] [-i/--into <rel>] [-f filter]` | Copy source files whose content **no** reference knows into `target` (a plain directory, not necessarily a registered repo).             |
| `dedup diff mv <source> <reference> <target> [--ref <repo>...] [-i/--into <rel>] [-f filter]` | Same as `cp`, but marks the source entries missing afterward.                                                                            |
| `dedup diff rm <source> <reference> [--ref <repo>...] [-f filter]`                            | Delete source files whose content **any** reference already knows.                                                                       |
| `dedup diff sync <source> <target> [--copy-new] [--delete-missing] [--mirror] [-f filter]`    | Single-target only (no `--ref`). Copy A's new content into B, and/or delete B's content that A marks missing (`--mirror` = both).        |

**Multi-reference semantics** (`print`/`cp`/`mv`/`rm`): the positional `reference` is always
included and is the copy-back target / primary; `--ref <repo>` is repeatable and adds more
references to the union. A file counts as "new" only when **none** of the references —
positional or `--ref` — already has its content. This is what makes disk-triage correct:
"unique" must mean unique against the sanitized directory *and* every already-processed disk,
not just one:

```
dedup diff cp disk3 sanitized /out --ref disk1 --ref disk2
```

copies from `disk3` only the content that isn't already in `sanitized`, `disk1`, or `disk2`.

`-i/--into <rel-path>` (on `cp`/`mv`) places transferred files under a relative subfolder
inside the target, preserving each file's source-relative path (e.g.
`dedup diff cp A B /backups --into imports/batch1` puts `photos/2020/a.jpg` at
`/backups/imports/batch1/photos/2020/a.jpg`); the subfolder is created if missing, and paths
escaping the target (absolute or containing `..`) are rejected.

Copy/move keep both repo indexes in sync as they run: copies are recorded in the target
repo's index (with the file's real on-disk mtime, so the next scan sees it as unchanged) and
moved/deleted files are marked missing in the source. A file copied or synced into a repo
also records its **provenance** — which repo it came from (`origin:<substring>` filter, shown
on duplicate cards as "from `<repo>`").

---

## `timeline`

```
dedup timeline <repo>... [--all] [--export <dir>] [-f filter]
```

Buckets files by year/month of their **best-known date**: EXIF capture time when present,
else file mtime (so it works, degraded, without EXIF — backups routinely clobber mtimes,
so EXIF is the better signal when it exists). With no `--export`, prints one line per
year-month bucket (file count, total size). With `--export <dir>`, copies matching files
into `<dir>/<year>/<month>/`, preserving filenames (never moves): name collisions get a
numeric suffix, and re-running an export is idempotent — a destination that already holds
the same content is skipped, and existing files are never overwritten.

Compose the `date:`/`before:`/`after:` filters (below) with the usual `mime:`/`name:`/`size:`
to scope the export, e.g. `-f "date:2021 mime:image/"`.

---

## `report`

```
dedup report <repo>... [--all]
```

Read-only. Prints a Markdown audit trail per repository: file/byte counts, exact-duplicate
group count and reclaimable bytes, triage-done status, flagged critical-file counts by
category (see `scan` below), and the top MIME types. This is the caretaker's at-a-glance
summary of what's been reduced, what remains, and what needs review.

---

## `scan`

```
dedup scan <repo>... [--all]
```

Advisory and read-only — flags likely-critical files so they're reviewed **before** disks
are wiped, grouped by category with the reason each was flagged:

- **Wallets** — `wallet.dat` (+ Berkeley DB magic), Ethereum keystore / Electrum wallet JSON,
  `*.wallet`, a BIP-39 seed phrase (12+ consecutive wordlist words) in a small text file.
- **Keys** — SSH/PGP private key headers, `.pem`/`.p12`/`.pfx`/`.gpg`, `.ssh/`/`.gnupg/`
  paths. (Public keys, Keynote `.key` files, and armored public keys/signatures are excluded
  — tuned so these don't drown real findings in noise.)
- **Vaults** — KeePass `.kdbx`/`.kdb`, 1Password `.1pif`.
- **Identity / Financial** — multilingual filename keywords (Testament, Vollmacht, Steuer,
  Versicherung, Kontoauszug, passport, Ausweis, …).

Nothing is ever touched, moved, or deleted by this command.

---

## `archive`

```
dedup archive index <repo>
dedup archive coverage <repo> [--ref <repo>...] [--redundant-only]
```

An inherited `backup_2019.zip` that holds nothing you don't already have loose is a common
class of redundancy that content-dedup alone can't see (its bytes differ from the loose
files). `archive index` reads every zip/tar/tar.gz in a repo and records each member's
content identity (size + BLAKE3) — opt-in and expensive, since it reads every archive fully.
`archive coverage` then reports what fraction of each indexed archive's members already
exist as loose content in the repo itself and any `--ref` repos; an archive at 100% coverage
is fully redundant and safe to delete. `--redundant-only` lists just those. Nested archives
are treated as opaque members (one level deep); encrypted or unreadable archives are skipped.

---

## Filter syntax

`-f/--filter` accepts one or more space-separated fields, combined with AND:

| Prefix                                           | Matches                                                                                                                               |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------- |
| `mime:<substring>`                               | The file's detected MIME type contains this substring.                                                                                |
| `name:<substring>`                               | The file's relative path contains this substring (verbatim — internal spaces are preserved, so keep other prefixes out of the value). |
| `size:<op><bytes>`                               | Size comparison, e.g. `size:>=1000`, `size:<500000`.                                                                                  |
| `origin:<substring>`                             | The repo the file was copied/synced from (provenance) contains this substring.                                                        |
| `date:YYYY[-MM[-DD]]`                            | The file's best-known date (EXIF capture time, else mtime) falls in that year/month/day.                                              |
| `before:YYYY[-MM[-DD]]` / `after:YYYY[-MM[-DD]]` | Best-known date is strictly before / on-or-after the given point.                                                                     |

Example: `-f "mime:image/ name:2020 size:>=1000"` matches images whose path contains `2020`
and are at least 1000 bytes. `-f "date:2021-03"` matches everything from March 2021.

The GUI's File Management tab exposes the same three base fields (`mime:`/`name:`/`size:`) as
an assisted pill builder — see [`docs/gui/files.md`](gui/files.md).

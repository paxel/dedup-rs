# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [Unreleased]

### Changed

- **Every preview and reconcile view now renders on one board.** Grooming's previews,
  Transfer's COPY / MOVE / SYNC / MIRROR / folder-export and GROUP SYNC previews, and
  Transfer's DIFF all share the same three-region layout, colour vocabulary, sort bar and
  virtualised scrolling — previously they were three different tables whose column count
  changed with the command.
- Commands sit in a centre column between the two sides and **act where they point**: a
  left-hand command in the left slot, a right-hand one in the right, and a command and its
  mirror on the same line (`COPY >` beside `< COPY`, `DELETE L` beside `DELETE R`). They are
  never truncated. DIFF's commands gained their side in the name for the same reason.
- Path colour now says the same thing everywhere: grey unchanged, green only-on-this-side,
  red will-be-deleted, amber differing (a conflict, or the same content renamed).
- The per-row `✗` reject toggle became **HIDE**, which drops the row from the board and from
  what RUN will do. GROUP SYNC rows carry no per-row commands, because that push is
  all-or-nothing and a HIDE there could not be honoured.
- Sorting moved from clickable column headers to an explicit bar (side · key · direction).
  This also **fixes DIFF sorting**, which previously changed no row order until the diff was
  re-planned.
- Large previews scroll instead of paging; PREV / PAGE / NEXT are gone.
- A row is as tall as its content needs, so a row offering two commands is short and one
  offering eight is tall, and a side holding the same content under several names lists them
  all against a single thumbnail.

### Added

- Sync-group **mains are badged**: a **★ MAIN** pill on the repository card and a star badge on
  the shared repo chip, so an original is distinguishable from its backups on every tab —
  Repositories, Files, Grooming, Duplicates, Browse and the lightbox.
- On the **Repositories** tab a sync group is now framed by its own **LCARS elbow section**,
  titled with the group name and holding the main and its sinks; ungrouped repositories stay
  bare cards. Groups start folded and the section header opens one, replacing the separate
  `SINK(S) IN '…'` chevron.

### Removed

- The GUI pixel-diff snapshot test (`dupes_view_snapshot`). Its baseline directory was
  gitignored, so no baseline was ever committed and the test could not pass on any machine
  but the one that last generated it; being `#[ignore]`d, the drift went unnoticed.
  Rendering is still verified by the `doc_screenshot_*` / `render_*` tests plus geometric
  layout asserts.

### Fixed

- **GROUP SYNC's review rows no longer bake the sink name into the file path.** The target
  path was `"<sink>: <rel>"`, so sorting by path sorted by sink name and a real path
  containing `": "` was ambiguous. Each row now names its sink with its own repo chip.
- The **GROUP SYNC** sink chips drew the wrong identicon: the push mode was folded into the
  repo name, and the identicon is hashed from that name, so a sink showed a different glyph
  there than on every other tab. The mode is now rendered beside the chip as `MODE: …`,
  matching the sink's own pill on the Repositories tab.

## [0.1.0] - 2026-07-20

### Added

- CLI (`dedup <command>`) and LCARS desktop app (`dedup` with no arguments).
- **Repositories** tab: create, rename, relocate, duplicate, remove and scan repositories, with per-repo stats and MIME breakdown.
- **Duplicates** tab: find exact or perceptually-similar duplicates across selected repos, review as file cards with a best-copy pick, and delete the rest; image/video/audio previews and a zoom/pan lightbox with A/B compare. The lightbox is tabbed by *representation* — **Overview** (facts, repo badges, marks, compare entry), **Image**/**Video**/**Audio**, **Metadata** (ID3 tags, EXIF capture facts) and **Text** (a text or hex preview for documents and other non-media duplicates) — showing only the tabs the two compared files actually offer, and only the columns that support the selected one.
- **Transfer** tab: copy, move or sync files between repos or into a dated folder, filtered by MIME/name/size; **GROUP SYNC** pushes a backup group's main to some or all of its sinks (each in its own ADD ONLY/MIRROR mode) when the source is a group's main; plus **DIFF**, a per-row side-by-side reconcile of two repositories.
- **Grooming** tab: dedupe against other repos, purge by filter, remove empty directories, reorganize by path templates, and prune missing records.
- **Browse** tab: directory-based index browser for one repo with tag annotations.
- `dedup repo <create|ls|rm|mv|rel|cp|update|dupes>`: manage repositories and run scans; `dupes` finds exact or `--threshold` perceptual duplicates.
- `dedup diff <print|cp|mv|rm|sync>`: compare a source repo against one or more reference repos by content and apply the differences.
- `dedup timeline <repos…> [--export <dir>]`: bucket files by date (EXIF, else mtime), optionally into a `<year>/<month>/` tree.
- `dedup report <repos…>`: Markdown triage report of counts, duplicates and flagged files.
- `dedup scan <repos…>`: flag likely-critical files (wallets, keys, vaults, documents).
- `dedup archive <index|coverage>`: index archive members by content and report archive redundancy.
- Content identity by size + BLAKE3 hash, with per-kind perceptual fingerprints (image, video, PDF/office/text/eml, audio) for similarity; scans record EXIF date and file origin.

# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [Unreleased]

### Added

- **Read a document, and compare what two documents say.** On the viewer's **Text** tab, a
  PDF, Word/OpenDocument file, spreadsheet, presentation, or email now shows its **extracted
  words** instead of a hex dump. Comparing two documents lines up their content **side by side**
  and marks what changed — **green** where a line is on only one side, **amber** where a line's
  characters differ — so you can see whether two copies say the same thing. Nothing is declared
  identical; a document with no extractable text (scanned, encrypted, or empty) says so.
  Word/Office documents keep their paragraph breaks, so their content diffs line by line.
- **See a PDF as it looks, side by side.** A new **Render** tab rasterizes a PDF to its page and
  shows it — the document's appearance, not its extracted words. Comparing two, their pages sit
  side by side to judge by eye (no pixel diff, no "same" verdict — different rendering makes that
  meaningless). Needs `pdftoppm` (poppler) at runtime; absent it, the tab just doesn't appear.
- **Text and Hex are now separate tabs.** The viewer's **Text** tab shows *readable* content
  only — a document's extracted words or a plain-text file's text (two text files now diff as
  content, aligned, not as bytes) — while a new **Hex** tab shows the raw bytes of *every* file
  (a head dump, or the full-file aligned hex diff when comparing two). A binary that used to
  show its bytes under "Text" now shows them under "Hex", and keeps Strings for embedded runs.
- **See the text hiding inside any file.** A new **Strings** tab on the viewer shows the
  printable runs embedded in a file's bytes — an image's EXIF strings, an audio file's tags, a
  program's paths and banners. Comparing two files aligns their runs so shared embedded text
  lines up and each side's distinct runs stand out. Offered for every file.
- **Archives are now something you can look inside, pull from, and unlock.** Clicking a
  zip/tar/tar.gz opens the shared viewer on a new **Archive** tab listing its members; click a
  member to open it in place, rendered by its own type (an image as an image, text as text),
  and go back with BACK/Esc. The source archive is never modified.
- **Extract from an archive.** EXTRACT ALL (or a per-member control) writes members into a
  folder you pick — extract into a repository and the next scan indexes the contents as loose,
  triageable files. Extraction never overwrites an existing file (collisions get a `_N`
  suffix) and never touches the source.
- **Password-protected zips.** A locked archive lists its member names (from the zip
  directory) and offers **UNLOCK**: type the password and its members open and extract.
  **RECOVER** tries a built-in list of common passwords for the weak ones (honest ceiling — a
  strong password will not fall), and **EXPORT HASH** copies the archive's hash in hashcat's
  `$zip2$` format (mode 13600) for real GPU cracking elsewhere. Scoped to archives you hold
  and are entitled to.
- **Archive redundancy shows inline in Duplicates.** When a loose file's content also lives
  inside a zip, a read-only **evidence row** names the archive — and the delete confirmation
  **warns** (never blocks) when a delete would leave content surviving only inside an archive.
- **Light appearance.** Settings now offers a **System / Light / Dark** appearance choice
  (Settings → Appearance). Dark stays the default and is unchanged; Light is opt-in, and
  System follows your desktop's light/dark setting. Switching applies immediately and is
  remembered. The review board's colour vocabulary (grey unchanged, green only-here, red
  will-delete, amber differs) stays distinct in both appearances, and repository identicons
  keep their identity while adapting to stay legible on either background.
- **The Text tab is now a real byte-level diff.** Comparing two files, the Text tab shows the
  **whole file** as an **aligned, paginated hex diff**: equal bytes line up, an inserted run
  shows as a green gap on one side, and a substitution shows the differing bytes in amber on
  both — so an inserted header no longer makes everything after it read as different. **Jump to
  next/previous difference** skips long equal runs; a very large or pervasively-different pair
  degrades to a block-level match and **says so** rather than pretending. (Two near-duplicate
  scans that differ only in embedded metadata now read as "same payload, header inserted".)
- **Compare and salvage metadata.** The Metadata tab highlights which EXIF/TIFF fields differ
  between the two sides, and **SAVE METADATA** writes a side's fields to a human-readable
  sidecar in a folder you pick — rescue the Title/Author/Keywords before deleting a copy.

### Changed

- **Flicker is now single-file focus.** In flicker the viewer shows only the visible file's
  facts and its rotate/mirror/save/delete — never both sides — and **SWAP** flips the image and
  all of that chrome together. Flicker is image-only; its controls no longer appear on the Text
  tab. (Previously the hidden side's controls were still clickable, so "ROTATE B" seemed to do
  nothing.)
- **A locked ("Protected") repo now lets you save a corrected copy.** The lock protects existing
  files, so **DELETE** and **overwrite-in-place** stay blocked — shown disabled with the reason
  rather than vanishing — but **save-as-a-new-copy** is allowed, since it only adds a file.
- **The compare viewer's "better" cue prefers the older copy**, and the highlight is a neutral
  distinction marker, not a keep/delete recommendation.
- **Filled buttons stay legible on the light appearance**, and the selected representation tab
  carries a border; a repo's identicon now decorates its name consistently across tabs.
- **Archive members are indexed as part of the normal scan**, gated by the same
  change-detection as every other file: a new or changed archive is read once, an unchanged
  one is skipped. This replaces the separate opt-in `dedup archive index` command (removed);
  `dedup archive coverage` stays and now reads whatever the last scan populated. An encrypted
  archive is indexed shallowly (member names and sizes, marked LOCKED) rather than dropped.

- **One viewer for every file, everywhere.** Clicking any file — a duplicate card (or the
  typed placeholder a document shows), a review board row, a file in Browse, a DIFF conflict —
  opens the same full-window viewer, with the same representation tabs (Image, Video, Audio,
  Metadata, Text), the same per-side switchers, and the actions of the place you came from:
  the Duplicates tab offers its DELETE / DELETE A / DELETE B mark pills, the DIFF board its
  OVERWRITE/DELETE commands. The Duplicates tab's own tabbed lightbox and its separate audio
  viewer are gone; the three viewers were three implementations of one job, drifting apart.
- In the viewer, a file opens **alone**, filling the whole screen; **SHOW B** reveals a second
  side — always another candidate, never the file already shown — and **HIDE B** returns.
  With one file shown the switcher walks the whole group or listing; with two, each side's
  switcher (`< PREV A`, `<1 / 3>`, `NEXT A >`) skips the file the other side shows, and its
  position counts that side's candidates, never the group size. Stepping keeps the audio
  transport state: playing keeps playing the newly shown copy, a deliberate pause stays
  paused with the new copy loaded. Flicker has buttons now — **FLICKER**, **SWAP**,
  **SIDE BY SIDE** — beside the space bar it always answered to, and the two panes of a Text
  (bytes) comparison are **scroll-locked** to the same offset.
- The viewer's **Overview tab is gone**: its facts moved into each side's own title block
  (repo, path, size, date, type, mark pill), so which file you are about to act on is always
  written beside it.
- **ID3 tags are editable in the shared viewer** for a writable audio file — EDIT TAGS /
  SAVE TAGS on the Metadata tab, `T` as the shortcut, with values from every other candidate
  offered for adoption — so the tag editor also reached Browse and the boards for the first
  time. Read-only repositories still never offer an edit.
- Pushing a backup group to several sinks now reads the main repository's index **once** for
  the whole push instead of once per sink, so a group with many sinks plans and runs with
  less repeated work. What each sink receives is unchanged.

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

- **A turned image can be saved to disk from the viewer.** After ROTATE / MIRROR on a
  writable file, **SAVE** offers the choice: **OVERWRITE** replaces the file in place
  (atomically — a failure cannot truncate it), or **SAVE COPY** writes a `_rot` sibling and
  leaves the original untouched. Either way the file **keeps its modified time** — a turned
  scan is still the same photograph from the same date — and when the image carries an EXIF
  capture date, the dialog can instead **stamp the file's date from EXIF**, for scans whose
  file date is only the day they were copied. An overwrite immediately re-hashes and
  re-indexes the file: content identity follows the bytes, and a save that kept its timestamp
  would otherwise be invisible to the next scan.
- **The Metadata tab lists every EXIF field** an image carries — camera, capture date,
  exposure, GPS, whatever is in the file — read on demand, scrolling in its column. The index
  still stores only camera and capture date; the rest never needed indexing to be shown.
- **Filters can now exclude.** Prefix any condition with `!` to invert it — `!name:*.mp3`
  keeps everything that is *not* an MP3, `!mime:image` everything that is not an image.
  Conditions still combine with AND, so `mime:image !name:*thumb*` reads "images, except
  thumbnails". In the filter wizard each condition gained a **NOT** toggle and negated
  conditions read `NOT NAME: *.mp3` on their chip. A `!` inside a value stays literal, so
  `name:!important` still searches for that text.
- **Filters can ignore capitalisation.** The new `Aa` toggle in the filter wizard (or a
  `case:insensitive` token in the expression) makes text conditions match regardless of case,
  so `*.jpg` also finds `PHOTO.JPG`. Matching stays case-sensitive by default, and size and
  date conditions are unaffected.
- Sync-group **mains are badged**: a **★ MAIN** pill on the repository card and a star badge on
  the shared repo chip, so an original is distinguishable from its backups on every tab —
  Repositories, Files, Grooming, Duplicates, Browse and the lightbox.
- On the **Repositories** tab a sync group is now framed by its own **LCARS elbow section**,
  titled with the group name and holding the main and its sinks; ungrouped repositories stay
  bare cards. Groups start folded and the section header opens one, replacing the separate
  `SINK(S) IN '…'` chevron.

### Removed

- With the old Duplicates viewers deleted, two of their extras did not move into the shared
  viewer: the audio **WAVEFORM/SPECTROGRAM toggle** (the viewer shows spectrograms; cards
  keep their inline preview), and the **video filmstrip scrubber** (a clip is now identified
  by one representative still per side; full playback stays the OPEN path).
- The GUI pixel-diff snapshot test (`dupes_view_snapshot`). Its baseline directory was
  gitignored, so no baseline was ever committed and the test could not pass on any machine
  but the one that last generated it; being `#[ignore]`d, the drift went unnoticed.
  Rendering is still verified by the `doc_screenshot_*` / `render_*` tests plus geometric
  layout asserts.

### Fixed

- **The DIFF comparison now has representation tabs, like the Duplicates lightbox.** It could
  previously only ever show a picture, so two MP3s produced `no preview for audio/mpeg` and two
  documents showed nothing at all. Audio now compares as a **spectrogram** and documents through
  a **Text** tab, both rendered by the same shared code the Duplicates viewer uses rather than a
  second implementation. Images and video compare exactly as before.
- **Grooming DEDUPE rows gained a COMPARE button**, opening the same comparison surface the
  Transfer DIFF board uses — so you can look at what a plan is about to delete, beside the copy
  that will survive, before running it. PURGE and PRUNE rows have no counterpart and do not
  offer it.
- **A Grooming DEDUPE row now names the copy that makes the file redundant.** The right side
  shows the surviving file in the pool, so a deletion list reads "this goes, because that
  stays" instead of asking you to trust it. PURGE and PRUNE have no counterpart and stay
  one-sided.
- **DIFF gained bulk actions over every listed row** — COPY MISSING in either direction and
  RENAME ALL L / R — for reconciling repositories with thousands of differences without
  clicking the same command a thousand times. Only actions the listed rows can actually use are
  offered, the confirmation states the exact count, rows you have hidden are left alone, and a
  partial failure reports how many succeeded and how many did not.
- **DIFF now highlights the characters that differ between two names.** In a BY HASH row the
  differing runs get a highlighted background on each side, computed from the longest common
  subsequence — so inserting one character marks just that character instead of everything
  after it. The shared parent directory is never painted, and highlighting never changes a
  row's height.
- **A scan that finds no files is now refused instead of emptying the index.** An unmounted
  drive scans as an empty directory, and marking every entry missing there is unrecoverable —
  worse, an emptied sync-group main turns the next MIRROR push into a wipe of its sinks. The
  GUI asks before continuing (nothing is written unless you confirm) and the CLI requires
  `dedup repo update --force`. Emptying a repository on purpose still works; it now costs one
  explicit confirmation.
- **Comparing two audio files now offers DELETE A / DELETE B directly in the player header**,
  so a copy can be marked without leaving the comparison — matching the image compare header.
  A copy in a read-only repository shows a disabled, struck-through `… (Protected)` pill.
- **Stepping through duplicate audio files while paused no longer resumes playback, and now
  loads the copy you are actually looking at.** Previously a paused step left the previous
  file loaded, so pressing play afterwards played the wrong copy.
- Returning to the **Repositories** tab now re-reads file counts and free space, so deleting
  duplicates on another tab is reflected immediately instead of leaving stale numbers until a
  manual refresh.

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

# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.1.0]

### Added

- **One lock per repository protects its existing files everywhere.** Every repository starts
  **locked** each launch; the padlock on its chip — on every tab — toggles it. A lock means the
  repo's existing files cannot be deleted or overwritten from anywhere in the app: those buttons
  are withheld or disabled (with the reason on hover), MIRROR/MOVE runs that would lose data in a
  locked repo won't start, and a locked MIRROR sink can't be included in a GROUP SYNC push.
  **Adding files to a locked repo is always allowed** — the lock protects what exists, it never
  blocks gaining data. Unlocking is a per-session declaration that loss is acceptable there:
  destructive actions then run without further questions (batch runs keep their plan summaries).
- **A Status centre, so the app never fails silently.** A top-right **STATUS** button (with an
  amber unread badge) opens a health/activity panel. At launch it probes the things that break
  quietly — the **audio output device** (so "no sound and no error" is now a visible warning),
  **ffmpeg/ffprobe**, **pdftoppm**, and **LibreOffice** — and files a **Warning** for anything
  missing. A
  repository whose folder has gone (a disconnected drive, a closed cloud mount) files one
  **Critical** that clears when it returns. Every warning says **since when** it has been true
  (first seen, kept across repeats — "offline since Tuesday" stays Tuesday), and each can be
  **dismissed** with its trashcan (or all at once with **Clear all**; anything still wrong
  refiles on its next detection). Each entry has a **Copy** button (message + a system
  fingerprint) and there's **Copy full report** (adds the recent log) for bug tickets — clipboard
  only, nothing is sent anywhere. An **Activity** section lists running/queued **scans** with
  **Cancel**, so a long scan can be stopped without killing the app.
- **The viewer no longer renders an empty page for a file that isn't there.** When a file's drive
  is disconnected (a closed cloud folder, an ejected disk), the pane says **"This file isn't
  present — its drive may be disconnected"** instead of a blank preview.
- CLI (`dedup <command>`) and LCARS desktop app (`dedup` with no arguments).
- **Installable on all three desktop OSes.** Every release ships prebuilt artifacts: a
  Debian `.deb` and a plain tarball for Linux, a zip for Windows with **two executables**
  (`dedup.exe`, the console CLI, and `dedup-gui.exe`, which opens the app without a console
  window — the double-click target), and for macOS a drag-to-Applications **`dedup.app`
  bundle in a `.dmg`** (unsigned: right-click → Open on first launch) plus a plain tarball,
  for Apple silicon and Intel. Package channels track releases automatically:
  `brew install paxel/tap/dedup`, a Scoop bucket for Windows, and
  `cargo install dedup-rs-cli` from crates.io (the binary is `dedup`). External tools stay
  optional everywhere — ffmpeg, poppler and LibreOffice unlock their features when present.
- **Repositories** tab: create, rename, relocate, duplicate, remove and scan repositories, with per-repo stats and MIME breakdown.
- **Duplicates** tab: find exact or perceptually-similar duplicates across selected repos, review as file cards with a best-copy pick, and delete the rest; image/video/audio previews and a zoom/pan lightbox with A/B compare.
- **Every file gets a preview.** Text files show their **first lines** right on the card
  (review rows show a mini version — hover it for the full head). PDFs show their **actual
  first page** (rendered in the background via poppler). Everything else — databases,
  executables, unknown blobs — gets a **byte view**: the file's head bytes as a greyscale
  pattern (identical content looks identical) with the **extension in big colour-coded
  letters** across the middle, the same hue for the same extension everywhere. All previews
  load in the background; the interface never waits on a disk.
- **File state is painted on the preview — and a cell only talks about itself.** If a side
  has a file, its cell shows that file's preview, veiled only with its *own* state: red
  **WILL DELETE** on a file a plan removes, amber **MISSING** when it is gone from disk. The
  side a file will *arrive* at shows the incoming file's preview under green **NEW**; a side
  that once held exactly this content and deleted it shows a blue **WAS DELETED** tombstone
  cell — every repository remembers what it deleted (until PRUNE), so a COPY/SYNC/DIFF/GROUP
  SYNC preview warns before you silently resurrect a deletion (a plain COPY's engine even
  refuses to; that refusal used to be invisible, now it's a visible row). A conflicting
  target path shows **the occupying file itself** — you judge by looking at both files.
- **Bigger review previews.** Review-board rows grew their preview cell (48 → 64 px) with more
  breathing room, so text and image previews on the board are actually readable.
- **Side-enforced commands.** The review board's command column is split down a midline into
  two half-columns tinted in each side's colour: a command always sits on the side whose file
  it changes, so `DELETE`, `RENAME`, `KEEP 1` and `DEL ALL` need no L/R suffix — the column
  says it. Copies and overwrites sit on the receiving side, their arrow naming where the file
  flows from, and an empty half-column means nothing happens to that side.
- **Transfer** tab: copy, move or sync files between repos or into a dated folder, filtered by MIME/name/size; **GROUP SYNC** pushes a backup group's main to some or all of its sinks (each in its own ADD ONLY/MIRROR mode) when the source is a group's main; plus **DIFF**, a per-row side-by-side reconcile of two repositories.
- **Grooming** tab: dedupe against other repos, purge by filter, remove empty directories, reorganize by path templates, and prune missing records.
- **Browse** tab: directory-based index browser for one repo with tag annotations. A
  **Show deleted** toggle also lists the files the repo remembers but no longer holds
  (tombstones, in blue) — the dock states the fact and the last-modified date, so an
  offline listing's file can be traced to surviving copies. Any name or path label
  anywhere can be **right-clicked → Copy**. Review-board columns title themselves with
  the repo's path in the side's accent (the inverse of the viewer's filename elbow). Selecting
  a file starts a **folder read-ahead** in the background: neighbours' previews are generated
  nearest-first, and the files themselves are warmed (≤ 256 MB each, ~2 GB per folder) so a
  cloud mount (pCloud) pulls them into its local cache — stepping through a folder stays
  instant. Your own clicks always win the disk; changing folder or tab cancels it.
- `dedup repo <create|ls|rm|mv|rel|cp|update|dupes>`: manage repositories and run scans; `dupes` finds exact or `--threshold` perceptual duplicates.
- `dedup diff <print|cp|mv|rm|sync>`: compare a source repo against one or more reference repos by content and apply the differences.
- `dedup timeline <repos…> [--export <dir>]`: bucket files by date (EXIF, else mtime), optionally into a `<year>/<month>/` tree.
- `dedup report <repos…>`: Markdown triage report of counts, duplicates and flagged files.
- `dedup scan <repos…>`: flag likely-critical files (wallets, keys, vaults, documents).
- `dedup archive coverage`: report archive redundancy from what the last scan populated.
- Content identity by size + BLAKE3 hash, with per-kind perceptual fingerprints (image, video, PDF/office/text/eml, audio) for similarity; scans record EXIF date and file origin.
- **Compare videos across their whole timeline.** The viewer's **Video** tab shows an
  **aligned filmstrip** per side — 8 frames sampled evenly across each clip, cached so stepping
  through a group is instant — instead of a single first frame (which two different clips often
  share: black, a slate, a logo). Clicking the strip drops a **shared playhead** that decodes
  the exact frame of *both* clips at that moment, enlarged side by side (A@t | B@t). The
  playhead is **proportional** — a fraction of each clip's own length — so a trimmed or
  re-encoded copy stays aligned at the same relative moment instead of drifting. Once a moment
  is picked, **FLICKER** swaps the two clips' frames in place (Space toggles), the same
  in-place comparison images get.
- **Listen on the Audio tab, compare on the Spectrum tab.** The viewer's **Audio** tab is a
  real transport: each side's **waveform** with a moving **playhead**, an **elapsed / total**
  readout, and **click-to-seek** — click anywhere in the wave to play from that spot (on a
  pair, both sides stay in sync with one audible). The **Spectrum** tab holds the zoomable
  **spectrogram** comparison — the visual fingerprint — with the zoom/pan/flicker the other
  image tabs have. A video with an audio track gets both tabs for its extracted soundtrack
  (silent clips don't; needs `ffmpeg`).
- **Slow a track down without changing its pitch.** The viewer's audio transport has discrete
  speed stops — **0.25 / 0.5 / 0.75 / 1 / 1.5 / 2×** — that preserve pitch, so a slowed
  recording still sounds like itself while you confirm two tracks are the same take. The chosen
  speed applies to the synced A/B pair too, keeping both soundtracks aligned. The first use of
  a speed renders once and is cached; after that it is instant.
- **Pull a backup's changes back into the main.** A **GROUP SYNC BACK** command (the reverse
  of GROUP SYNC) reconciles one sink into its main: files you added straight to the backup are
  **promoted** in a batch (green), while files the main **deleted** that the sink still holds are
  shown as **resurrection** candidates (a blue mark) and never auto-promoted — you pull each
  one back on its own, so you recreate a mistaken deletion without silently undoing a real one.
  Each row is a full triage decision: **`< COPY`** pulls that one file into the main, and
  **`DELETE R`** removes it from the sink instead (shown only while the sink is unlocked) —
  everything the sink holds is either worth promoting or worth purging, in one pass. Clicking a
  row opens the file itself in the viewer.
- **Read a document, and compare what two documents say.** On the viewer's **Text** tab, a
  PDF, Word/OpenDocument file, spreadsheet, presentation, or email shows its **extracted
  words** instead of a hex dump. Comparing two documents lines up their content **side by side**
  and marks what changed — **green** where a line is on only one side, **amber** where a line's
  characters differ — so you can see whether two copies say the same thing. Nothing is declared
  identical; a document with no extractable text (scanned, encrypted, or empty) says so.
  Word/Office documents keep their paragraph breaks, so their content diffs line by line.
  Extraction runs in the background — a slow-to-parse PDF shows a short note for a moment
  instead of freezing the window.
- **See a document as it looks, side by side — page by page.** A **Render** tab rasterizes a
  document's pages on demand and shows them — its appearance, not its extracted words. A PDF
  renders directly; Word, spreadsheets, presentations, OpenDocument — and the legacy formats
  the Text tab can't read, **`.doc` and `.rtf`** — convert in the background via headless
  LibreOffice first (once per session per document). **Each side has its own page control**
  (prev/next, an editable page number beside that side's own page count, and a slider for
  sweeping), so an extra front page on one copy is lined up by hand — the counts are stated,
  and the tool never guesses which page maps to which. **FLICKER** (or `space`) swaps the two
  lined-up pages in place, so a shifted paragraph or changed figure jumps out. Pages render in
  the background — a huge book never freezes the app — and recent pages are kept so stepping
  back is instant. Needs `pdftoppm` (poppler) at runtime, plus LibreOffice for the non-PDF
  formats; absent a tool, the affected tab just doesn't appear (and the Status centre says
  why).
- **Separate Text and Hex tabs.** The viewer's **Text** tab shows *readable* content
  only — a document's extracted words or a plain-text file's text (two text files diff as
  content, aligned, not as bytes) — while the **Hex** tab shows the raw bytes of *every* file
  (a head dump, or the full-file aligned hex diff when comparing two). Comparing two files, the
  Hex tab shows the **whole file** as an **aligned, paginated hex diff**: equal bytes line up,
  an inserted run shows as a green gap on one side, and a substitution shows the differing bytes
  in amber on both — so an inserted header doesn't make everything after it read as different.
  **Jump to next/previous difference** skips long equal runs; a very large or
  pervasively-different pair degrades to a block-level match and **says so** rather than
  pretending. (Two near-duplicate scans that differ only in embedded metadata read as "same
  payload, header inserted".)
- **See the text hiding inside any file.** A **Strings** tab on the viewer shows the
  printable runs embedded in a file's bytes — an image's EXIF strings, an audio file's tags, a
  program's paths and banners. Comparing two files aligns their runs so shared embedded text
  lines up and each side's distinct runs stand out. Offered for every file.
- **Look inside archives, pull from them, and unlock them.** Clicking a
  zip/tar/tar.gz opens the shared viewer on an **Archive** tab listing its members; click a
  member to open it in place, rendered by its own type (an image as an image, text as text),
  and go back with BACK/Esc. The source archive is never modified. Archive members are indexed
  as part of the normal scan, gated by the same change-detection as every other file; an
  encrypted archive is indexed shallowly (member names and sizes, marked LOCKED).
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
- **Light appearance.** Settings offers a **System / Light / Dark** appearance choice
  (Settings → Appearance). Dark is the default; Light is opt-in, and System follows your
  desktop's light/dark setting. Switching applies immediately and is remembered. The review
  board's colour vocabulary (grey unchanged, green only-here, red will-delete, amber differs)
  stays distinct in both appearances, and repository identicons keep their identity while
  adapting to stay legible on either background.
- **Compare and salvage metadata.** The Metadata tab highlights which EXIF/TIFF fields differ
  between the two sides, and **SAVE METADATA** writes a side's fields to a human-readable
  sidecar in a folder you pick — rescue the Title/Author/Keywords before deleting a copy. The
  tab lists **every EXIF field** an image carries — camera, capture date, exposure, GPS,
  whatever is in the file — read on demand, scrolling in its column.
- **A turned image can be saved to disk from the viewer.** After ROTATE / MIRROR on a
  writable file, **SAVE** offers the choice: **OVERWRITE** replaces the file in place
  (atomically — a failure cannot truncate it), or **SAVE COPY** writes a `_rot` sibling and
  leaves the original untouched. Either way the file **keeps its modified time** — a turned
  scan is still the same photograph from the same date — and when the image carries an EXIF
  capture date, the dialog can instead **stamp the file's date from EXIF**, for scans whose
  file date is only the day they were copied. An overwrite immediately re-hashes and
  re-indexes the file, so content identity follows the bytes.
- **ID3 tags are editable in the shared viewer** for a writable audio file — EDIT TAGS /
  SAVE TAGS on the Metadata tab, `T` as the shortcut, with values from every other candidate
  offered for adoption. Read-only repositories never offer an edit.
- **Filters can exclude.** Prefix any condition with `!` to invert it — `!name:*.mp3`
  keeps everything that is *not* an MP3, `!mime:image` everything that is not an image.
  Conditions still combine with AND, so `mime:image !name:*thumb*` reads "images, except
  thumbnails". In the filter wizard each condition has a **NOT** toggle and negated
  conditions read `NOT NAME: *.mp3` on their chip. A `!` inside a value stays literal, so
  `name:!important` still searches for that text.
- **Filters can ignore capitalisation.** The `Aa` toggle in the filter wizard (or a
  `case:insensitive` token in the expression) makes text conditions match regardless of case,
  so `*.jpg` also finds `PHOTO.JPG`. Matching is case-sensitive by default, and size and
  date conditions are unaffected.
- **One viewer for every file, everywhere.** Clicking any file — a duplicate card (or the
  typed placeholder a document shows), a review board row, a file in Browse, a DIFF conflict —
  opens the same full-window viewer, with the same representation tabs (Image, Video, Audio,
  Metadata, Text, Render, Strings, Hex, Archive), the same per-side switchers, and the actions
  of the place you came from: the Duplicates tab offers its DELETE / DELETE A / DELETE B mark
  pills, the DIFF board its OVERWRITE/DELETE commands. A file opens **alone**, filling the
  whole screen; **SHOW B** reveals a second side — always another candidate, never the file
  already shown — and **HIDE B** returns. Each side's top is a **read-only identity block**
  (bordered repo chip, the **bold file name** led by an accent cap, size/date/type), and every
  control sits in a **fixed action bar along the bottom** — navigate on the left, tools in the
  middle, the destructive action inset at the right — so a button never shifts under the cursor
  as the file name changes. The two sides are split by a **centre divider** and every side keeps
  a **fixed half**: a long name or a wide document line scrolls or clips within its own half
  instead of pushing the other side off-screen, and the file name is a **selectable** field that
  sticks to its end so the whole path can be dragged to the front and copied. With one
  file shown a compact switcher (`‹ 1 / 2 ›`) walks the whole group or listing; with two, each
  side's switcher skips the file the other side shows, and its position counts that side's
  candidates, never the group size. The title says whether you are comparing exact
  **duplicates** or a **SIMILAR** search (naming the threshold for a perceptual one), and a
  similar pair shows its own **`A ↔ B N%`** score so a loose match never poses as a byte-for-byte
  duplicate. **Flicker is single-file focus** — one file's facts and its rotate/mirror/save/delete,
  never both — and **SWAP** flips the image and all of that chrome together; the two panes of a
  byte comparison are **scroll-locked** to the same offset.
- **A locked ("Protected") repo lets you save a corrected copy.** The lock protects existing
  files, so **DELETE** and **overwrite-in-place** stay blocked — shown disabled with the reason
  rather than vanishing — but **save-as-a-new-copy** is allowed, since it only adds a file.
- Sync-group **mains are badged**: a **★ MAIN** pill on the repository card and a star badge on
  the shared repo chip, so an original is distinguishable from its backups on every tab —
  Repositories, Files, Grooming, Duplicates, Browse and the lightbox. On the **Repositories**
  tab a sync group is framed by its own **LCARS elbow section**, titled with the group name and
  holding the main and its sinks; ungrouped repositories stay bare cards.
- **One board for every preview and reconcile view.** Grooming's previews, Transfer's
  COPY / MOVE / SYNC / MIRROR / folder-export and GROUP SYNC previews, and Transfer's DIFF all
  share the same three-region layout, colour vocabulary, sort bar and virtualised scrolling.
  Commands sit in a centre column between the two sides and **act where they point** (a
  left-hand command in the left slot, `COPY >` beside `< COPY`, `DELETE L` beside `DELETE R`),
  and are never truncated. Path colour says the same thing everywhere: grey unchanged, green
  only-on-this-side, red will-be-deleted, amber differing. A per-row **HIDE** drops a row from
  the board and from what RUN will do. Sorting is an explicit bar (side · key · direction). A
  row is as tall as its content needs.
- **Grooming DEDUPE rows have a COMPARE button** and name the surviving copy. COMPARE opens
  the same comparison surface the Transfer DIFF board uses, so you can look at what a plan is
  about to delete beside the copy that will survive; the right side shows the surviving file in
  the pool, so a deletion list reads "this goes, because that stays". PURGE and PRUNE rows have
  no counterpart and stay one-sided.
- **DIFF has bulk actions over every listed row** — COPY MISSING in either direction and
  RENAME ALL L / R — for reconciling repositories with thousands of differences without
  clicking the same command a thousand times. Only actions the listed rows can use are offered,
  the confirmation states the exact count, hidden rows are left alone, and a partial failure
  reports how many succeeded and how many did not. In a BY HASH row DIFF **highlights the
  characters that differ between two names**, computed from the longest common subsequence, so
  inserting one character marks just that character; the shared parent directory is never
  painted, and highlighting never changes a row's height.
- **A scan that finds no files is refused instead of emptying the index.** An unmounted
  drive scans as an empty directory, and marking every entry missing there is unrecoverable —
  worse, an emptied sync-group main turns the next MIRROR push into a wipe of its sinks. The
  GUI asks before continuing (nothing is written unless you confirm) and the CLI requires
  `dedup repo update --force`. Emptying a repository on purpose still works; it now costs one
  explicit confirmation.
- Returning to the **Repositories** tab re-reads file counts and free space, so deleting
  duplicates on another tab is reflected immediately instead of leaving stale numbers.
- Comparing two audio files offers **DELETE A / DELETE B** directly in the player header, so a
  copy can be marked without leaving the comparison; a copy in a read-only repository shows a
  disabled, struck-through `… (Protected)` pill. Stepping through duplicate audio files while
  paused loads the copy you are looking at without resuming playback.

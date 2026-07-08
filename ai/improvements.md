# dedup-rs — Improvement Roadmap

## Vision

dedup-rs is a **data-inheritance triage tool**. The scenario it serves: someone dies (or a
machine dies) and leaves behind a NAS, broken PCs, and an unsorted heap of disks full of
redundant backups. A caretaker must find the useful and important material — documents,
photos, crypto wallets, keys — without eyeballing terabytes of duplicates. As the
"no hardcopies" generation ages, this is a recurring, real problem.

The product is a four-stage pipeline; the roadmap phases map onto it:

```
 Stage 1: REDUCE     eliminate duplicates within each disk           (exists)
 Stage 2: SANITIZE   copy unique content to a curated dir, then      (primitives exist,
                     diff every next disk against it                  workflow missing)
 Stage 3: REFINE     drop media that exists elsewhere in better       (similar mode exists,
                     quality — needs fast human review                 review tooling missing)
 Stage 4: ORDER      organize survivors by time/importance; flag      (missing)
                     wallets, keys, vital documents
```

Priorities (decided 2026-07-08): **Phase 1 Review tooling → Phase 2 Sanitize workflow →
Phase 3 Content coverage → Phase 4 Forensic layer.** Media strategy is **hybrid**: in-app
image zoom, in-app audio playback, video as scrub-able frame strips; one click hands any
file to the system's external app for full fidelity.

## Current state (baseline, 2026-07)

- **Store** (`crates/dedup-core/src/store.rs`): per-repo redb index; `FileEntry` =
  size, blake3 hash, mtime, mime, perceptual fingerprints, `img_size`. Entry values carry a
  version byte (`ENTRY_VERSION`); old versions decode gracefully and are flagged `stale` in
  `ScanEntry`, which forces re-hashing on the next update — **this is the template for any
  future `FileEntry` field addition**.
- **Fingerprints** (`crates/dedup-core/src/fingerprint.rs`): images = 512-bit dual-gradient
  hash (17×17 grid, rotation/mirror invariant via brightness-moment canonicalization);
  video = 3×64-bit frame dHashes via external `ffmpeg`; PDF = blake3 of normalized text
  (`lopdf`); audio = duration + post-ID3 chunk hash (`symphonia`). MIME via `infer` +
  `mime_guess`.
- **Dedup/similar** (`dupes.rs`, `similar.rs`): exact groups streamed as plans; similar
  groups via banded Hamming grouping. GUI review with thumbnails, marking, quick delete,
  auto-resolve, per-file read-only unlock.
- **Diff/sync** (`crates/dedup-core/src/diff.rs`): `diff_print`, `diff_copy` (copy/move
  content-unique files to an **arbitrary directory**), `diff_delete`, `diff_sync` — all
  content-identity based (size + blake3), all filterable. CLI: `dedup diff print|cp|mv|rm|sync`.
  **Limitation: one reference repo at a time.**
- **GUI** (`crates/dedup-gui`): egui 0.35 LCARS theme; Repos / Duplicates / Files tabs;
  `ThumbCache` (`thumbs.rs`) = background decode workers + LRU of ≤200 GPU textures keyed by
  content hash; disk thumbs at `~/.cache/dedup/thumbs/<hash>.jpg` (≤512 px). Files tab is a
  diff-driven transfer panel (copy/move/delete vs a reference repo), not a browser.
- **Filters** (`crates/dedup-core/src/filter.rs`): `mime:`/`name:`/`size:` only.
- **Testing**: headless `egui_kittest` UI tests (geometric asserts + `--ignored` PNG render
  tests via lavapipe); core integration tests with temp repos.
- **Not present**: any preview beyond thumbnails, EXIF, office/CSV/archive/email handling,
  date organization, keyword/wallet scanning, settings persistence.

Spec template used below: **Purpose · UX · Data/arch · Crates · Touchpoints · Acceptance ·
Effort (S/M/L) · Risks**.

---

## Phase 1 — Review tooling

The similar-review pass is the human bottleneck: judging "is the RO copy really worse?"
today means squinting at 160-px thumbnails. Everything in this phase shortens that loop.

### 1.1 Image lightbox (click → zoom viewer)

- **Purpose**: judge image quality at pixel level without leaving the app.
- **UX**: clicking a thumbnail opens a full-window modal:

  ```
  ┌──────────────────────────────────────────────────────────┐
  │  ← 3/7 →                                        ✕ close  │
  │                                                          │
  │                   [ image, zoom/pan ]                    │
  │                                                          │
  │  PXL_20230830.jpg · 4.78 MB · 3072×4080 · 2023-08-30     │
  │  [FIT] [1:1]   zoom: wheel · pan: drag · next: ←/→       │
  └──────────────────────────────────────────────────────────┘
  ```

  Wheel zooms around the cursor; drag pans; `F` fit / `1` 100 %; `←`/`→` steps through the
  group's members (stays open, so flipping between two copies is already a crude compare);
  `Esc` closes. `Delete`/`K` toggles the mark on the shown file (respecting read-only).
- **Data/arch**: new `dedup-gui/src/lightbox.rs`. Full-resolution decode must not block the
  UI: reuse the `ThumbCache` worker pattern (`thumbs.rs`) with a separate tiny cache
  (2–3 full-res textures, current + neighbors, hard-capped: a 50 MP photo is ~200 MB RGBA —
  evict aggressively, downscale anything above a max texture edge, e.g. 8192 px). While the
  full-res decode is in flight, show the existing 512-px thumb scaled up.
  State lives in `DupesView` (`lightbox: Option<LightboxState { group: usize, index: usize, zoom, pan }>`)
  so marking integrates with the existing `Act::ToggleMark` flow.
- **Crates**: none new (`image` already in core; expose a `thumbnail::load_full_rgba(path, max_edge)`).
- **Touchpoints**: `dupes_view.rs::thumbnail()` (make the image clickable → open),
  new `lightbox.rs`, `thumbs.rs` (generalize worker pool), `dedup-core/src/thumbnail.rs`.
- **Acceptance**: open from any image card; 1:1 shows true pixels; navigation covers the
  whole group including off-screen members; marking from the lightbox updates the card;
  no UI freeze on a 100 MP file; kittest test for open/navigate/mark; render test PNG.
- **Effort**: M.
- **Risks**: GPU memory on huge panoramas (mitigate with max-edge downscale); egui modal
  focus handling with the existing confirm modals.

### 1.2 Side-by-side / flicker compare

- **Purpose**: "which of these two is the better copy?" answered in seconds. This is the
  core Stage-3 decision.
- **UX**: in the lightbox, `C` enters compare mode against the group's BEST member (or a
  chosen second file via a "compare" button on cards):

  ```
  ┌───────────────────────────┬──────────────────────────────┐
  │  A: best (4.78 MB, 4080²) │  B: candidate (1.96 MB, 3072²)│
  │   [ synced zoom / pan ]   │    [ synced zoom / pan ]     │
  ├───────────────────────────┴──────────────────────────────┤
  │  [SIDE-BY-SIDE] [FLICKER]      A/B: space   mark B: Del  │
  └───────────────────────────────────────────────────────────┘
  ```

  Both panes share one zoom/pan (compensating for resolution difference by normalizing to
  image fraction, not pixels). FLICKER mode overlays them full-window and `space` swaps —
  the fastest way to see compression artifacts. Metadata strip shows dims/size/mtime (and
  camera once 3.1 lands) with the better value highlighted.
- **Data/arch**: extension of `lightbox.rs` (`CompareState { other: (group, index), flicker: bool }`).
  Audio compare = two players (1.3); video compare = two frame strips (1.4) — same layout,
  media-specific panes.
- **Crates**: none new.
- **Touchpoints**: `lightbox.rs`, `dupes_view.rs` (entry points on cards).
- **Acceptance**: synced zoom at differing resolutions; flicker swap < 1 frame lag (both
  textures resident); marking either side works; kittest coverage.
- **Effort**: M (on top of 1.1).
- **Risks**: normalizing zoom across aspect-ratio mismatches (crop-vs-original pairs) —
  define as "align top-left of the shared fraction" for v1.

### 1.3 Audio preview

- **Purpose**: confirm two "similar" audio files are the same recording (and which sounds
  better) without opening a player.
- **UX**: audio cards get `▶` / `⏸` and a thin seek bar with elapsed/total (total from the
  stored `AudioFp.duration_ms`). One file plays at a time; starting another stops the first.
  In compare mode (1.2), two players with a "swap" button that keeps the seek position —
  the audio analog of flicker.
- **Data/arch**: new `dedup-gui/src/player.rs`: one background thread owning a `rodio`
  `OutputStream`; commands via `crossbeam_channel` (`Play(path)`, `Seek(f32)`, `Stop`);
  decode via `rodio`'s symphonia backend (symphonia is already the workspace decoder).
  Position reported back via shared atomic for the seek bar.
- **Crates**: add `rodio` (gui) — use its `symphonia-all` feature to match core's format
  coverage.
- **Touchpoints**: `dupes_view.rs::file_card` (audio branch currently renders only the
  placeholder), new `player.rs`, `app.rs` (stop playback on tab switch).
- **Acceptance**: play/pause/seek mp3+flac+ogg+m4a; no UI thread stalls; only one stream at
  a time; player state survives scrolling (card virtualization must not kill playback).
- **Effort**: M.
- **Risks**: ALSA/PipeWire quirks (rodio handles most); virtualized cards — keep the player
  global (in `DupesView`), rendered independently of card visibility.

### 1.4 Video preview (frame strip + hover scrub)

- **Purpose**: identify a video and roughly compare quality without full playback.
- **UX**: video cards show a 1-frame thumb (new — today they show the icon placeholder).
  In the lightbox, a video renders as a strip of ~10 stills with hover-scrub (mouse x →
  frame) plus dims/duration/bitrate line. Real playback is explicitly out of scope —
  that's the external-open path (1.5).
- **Data/arch**: reuse `fingerprint.rs::extract_frame` (ffmpeg → PNG pipe) generalized to
  `extract_frames(path, n)`; cache stills through the existing disk thumb cache keyed as
  `<hash>-v<idx>.jpg` (extend `thumbnail::thumb_path` keying). Generated lazily by the
  thumb worker pool; absent ffmpeg → placeholder (same degradation as fingerprinting).
- **Crates**: none new (external ffmpeg, already a soft dependency).
- **Touchpoints**: `dedup-core/src/thumbnail.rs` (+ video thumb entry point),
  `dedup-core/src/fingerprint.rs` (share frame extraction), `thumbs.rs`, `dupes_view.rs`,
  `lightbox.rs`.
- **Acceptance**: video card shows a real frame when ffmpeg exists; strip scrubs smoothly
  from cache (no ffmpeg call per hover); graceful without ffmpeg.
- **Effort**: M.
- **Risks**: first-open latency (extract 10 frames ≈ seconds for large files) — extract
  progressively, show frames as they land.

### 1.5 Open externally / reveal in file manager

- **Purpose**: full-fidelity escape hatch for every type; also the designated video player.
- **UX**: every file card (and the lightbox) gets a context menu — same pattern as the
  read-only unlock menu in `dupes_view.rs::file_card` — with "Open" and "Show in folder".
- **Data/arch**: small `dedup-gui/src/external.rs`: `open(path)` and `reveal(path)` via the
  `open` crate (`open::that`, `open::that_in_background`); reveal = open parent dir
  (portable lowest common denominator).
- **Crates**: add `open` (gui).
- **Touchpoints**: `dupes_view.rs::file_card`, `lightbox.rs`, later `files_view.rs`.
- **Acceptance**: works for image/audio/video/pdf/unknown; non-blocking; kittest asserts
  menu entries exist.
- **Effort**: S.
- **Risks**: none significant.

---

## Phase 2 — Sanitize workflow

Turns the CLI primitives into the actual Stage-1/2 disk-triage loop.

### 2.1 Multi-reference diff

- **Purpose**: "unique" must mean *unique vs the sanitized dir AND every already-processed
  disk*, not vs a single repo.
- **Data/arch**: `diff.rs` currently builds one reference content index
  (`store::read_content_index`). Change the operations to take `references: &[String]` and
  merge the indexes (content key → merged `ContentState`). `diff_print`'s `Equal` gains
  which reference matched (first hit is enough). CLI: `--ref` becomes repeatable
  (`dedup diff cp <src> --ref sanitized --ref disk1 <target>`); keep the positional
  single-reference form as sugar.
- **Touchpoints**: `crates/dedup-core/src/diff.rs` (all four ops + `DiffItem`),
  `crates/dedup-cli/src/main.rs` (`DiffCommands`), `files_view.rs` (multi-select reference
  repos — the repo-pill row already exists in the dupes view to copy from).
- **Acceptance**: integration test — file unique vs ref A but present in ref B is not
  copied; CLI help documents repeatable `--ref`; Files tab allows multiple references.
- **Effort**: M.
- **Risks**: memory of merged indexes at millions of entries (it's a `HashMap<(u64,[u8;32]), _>`
  — fine to ~10⁷; note it, don't engineer around it yet).

### 2.2 Guided triage flow ("Sanitize" tab or wizard)

- **Purpose**: make the disk loop a checklist instead of tribal CLI knowledge.
- **UX**: a wizard-style panel driving the existing machinery:

  ```
  1. SOURCE  [ disk_2024_hddred ▾ ]  (register + update if stale)
  2. TARGET  [ ~/sanitized  (folder picker) ]  reference: [sanitized ✓] [disk1 ✓] …
  3. REVIEW  1 234 new · 88 402 already known · preview list (diff_print)
  4. RUN     [ COPY UNIQUES ]  → progress → summary → "mark disk done"
  ```

  Each step gates the next; "already known" is expandable for spot checks. After a run, the
  disk repo gets a done marker so the repo bar shows triage status.
- **Data/arch**: build on `files_view.rs` (it already has preview/confirm/background-run
  scaffolding around `diff_copy`); add the multi-reference picker (2.1) and an arbitrary
  target directory via `rfd` (already used for folder pickers in `app.rs`). "Done" flag: a
  `META` key in the repo db (`store.rs` `META` table, string→u64 — add a `triage_done_ms`).
  CLI sugar: `dedup sanitize <source> --into <dir> --ref <repo>...` = update + diff cp +
  summary.
- **Touchpoints**: `files_view.rs` (or new `sanitize_view.rs` + tab in `app.rs`),
  `store.rs` (meta flag), `main.rs` (CLI subcommand).
- **Acceptance**: end-to-end integration test with three temp repos (disk → sanitized with
  one prior disk as extra ref); GUI kittest walk through the four steps; done marker shows
  in Repos view.
- **Effort**: L.
- **Risks**: the sanitize dir should itself be a registered repo (so it can be a reference
  and is indexed after copies) — the wizard should create/update it automatically; decide
  copy-then-index vs `diff_sync`-style indexed copy (prefer the latter, `sync_copy` already
  indexes with real mtime).

### 2.3 Provenance

- **Purpose**: months later you must answer "which disk did this file come from?".
- **Data/arch**: new optional `FileEntry.origin: Option<String>` ("repo-name" of the source
  at copy time), written by `diff_copy`/`sync_copy` when the target is a repo. Bump
  `ENTRY_VERSION` and reuse the existing v-decode + `stale` machinery (only entries that
  need new data rescan; `origin` defaults to `None` for old entries — no rescan needed,
  so this is a decode-compat-only bump). Shown on file cards and in the timeline (4.2);
  filterable (`origin:` prefix in `filter.rs`).
- **Touchpoints**: `store.rs` (FileEntry + `FileEntryV2` legacy decode), `diff.rs`,
  `filter.rs`, `dupes_view.rs`/`files_view.rs` display.
- **Acceptance**: copy from disk repo → sanitized entry carries origin; old dbs decode
  unchanged; store unit test mirrors `v1_entries_decode_and_flag_images_stale`.
- **Effort**: M.
- **Risks**: keep it a display/filter hint, not an identity input.

---

## Phase 3 — Content coverage

Widen what dedup/similarity/ordering understands. Each fingerprint addition follows the
established pattern: extend `fingerprint::compute` dispatch, add an optional `FileEntry`
field, bump `ENTRY_VERSION`, mark affected mimes stale for rescan (exactly how the image
hash upgrade shipped).

### 3.1 EXIF metadata (images)

- **Purpose**: real capture dates beat file mtimes (backups clobber mtimes constantly);
  camera model helps best-copy ranking and the timeline (4.2).
- **Data/arch**: `kamadak-exif` parse on the image path in `fingerprint::compute`; store
  `exif: Option<ExifInfo { taken_ms: Option<i64>, camera: Option<String> }>`. Rank in
  `dupes.rs::sort_group_members` after image area (original beats re-save when pixel-equal).
  Filter: `taken:<year>` later.
- **Touchpoints**: `fingerprint.rs`, `store.rs`, `dupes.rs`, card display.
- **Acceptance**: JPEG/HEIF-with-EXIF gets taken date; corrupt EXIF → `None` (best-effort
  like all fingerprints); best-copy test with mtime-vs-EXIF conflict.
- **Effort**: S/M.
- **Risks**: none major; timezone ambiguity — store as naive local ms, document it.

### 3.2 Office documents

- **Purpose**: the same thesis exists as `report.docx`, `report(1).docx`, and inside three
  backups — text-identity groups them like PDFs already group.
- **Data/arch**: `doc_text_hash` mirroring `pdf_text_hash` (blake3 of lowercased,
  whitespace-stripped text). Extractors: docx/xlsx/pptx/odt/ods = `zip` + `quick-xml` text
  node walk; legacy xls via `calamine`; legacy .doc = out of scope (note it). Store in the
  existing `pdf_hash`-like field — rename concept to `doc_hash` (keep the `pdf_hash` store
  field name for compat or migrate with the version bump). Grouped in `similar.rs` by exact
  hash equality like PDFs today.
- **Crates**: `zip`, `quick-xml`, `calamine` (core).
- **Touchpoints**: `fingerprint.rs`, `similar.rs` (pdf group_by becomes doc group_by),
  `store.rs`.
- **Acceptance**: same text saved as .docx and .odt groups at 100 %; xlsx with one changed
  cell does not; encrypted docs → `None`.
- **Effort**: M.
- **Risks**: text extraction fidelity varies; acceptable — false negatives only (content
  hash still catches byte dupes).

### 3.3 CSV / plain text

- **Purpose**: exports and logs duplicated across backups with BOM/line-ending/encoding
  drift.
- **Data/arch**: for `text/*` + `text/csv`: normalized text hash (strip BOM, normalize
  CRLF→LF, trim trailing whitespace; blake3). Same exact-match grouping. Near-duplicate
  text (MinHash/simhash over shingles) is explicitly **deferred** — note as future work.
- **Touchpoints**: `fingerprint.rs`, `similar.rs`.
- **Acceptance**: same CSV with CRLF vs LF groups; one-row difference doesn't.
- **Effort**: S.
- **Risks**: huge text files — cap normalization at N MB, hash raw beyond.

### 3.4 Archives

- **Purpose**: "backup_2019.zip" that contains nothing you don't already have loose is the
  single biggest redundancy class on inherited disks.
- **Data/arch**: index archive members (zip, tar, tar.gz) during update: per member, size +
  blake3 (streamed). New store table `ARCHIVE_MEMBERS` (rel_path → member list) rather than
  fake FileEntries. New report: **archive coverage** — % of member content present in
  selected repos (reuses content indexes from `diff.rs`); an archive at 100 % coverage is
  safe to delete and appears in a "fully redundant archives" list in the GUI.
- **Crates**: `zip`, `tar`, `flate2` (core).
- **Touchpoints**: `update.rs` (opt-in flag — it's expensive), `store.rs` (new table),
  new `archive.rs` in core, GUI report entry point.
- **Acceptance**: zip whose members all exist loose reports 100 %; nested archives handled
  one level deep (deeper = future); update without the flag unchanged in speed.
- **Effort**: L.
- **Risks**: cost and encrypted archives (skip, report as unknown); scope-creep — keep it
  report-only, no in-archive dedup actions.

### 3.5 Email stores

- **Purpose**: mbox/eml hoards hold the paper trail (contracts, statements, account
  notices) that inheritance triage is actually looking for.
- **Data/arch**: v1 = treat `.eml` as documents: hash of normalized `(Message-ID)` or, if
  absent, normalized headers+body — dedups the same mail exported twice. mbox = index
  per-message like archive members (3.4 machinery). PST is a stretch goal (note
  `readpst` external-tool route, don't build a parser).
- **Crates**: `mail-parser` (core).
- **Touchpoints**: `fingerprint.rs`, `archive.rs` (mbox), `similar.rs`.
- **Acceptance**: same message exported as two .eml files groups; different mails don't.
- **Effort**: M (eml) / L (mbox).
- **Risks**: encodings; keep best-effort.

### 3.6 Video similarity upgrade (note)

The per-frame video hash is still the 64-bit lexicographic dHash and has the same
degenerate-collision weakness the image hash had before the 512-bit upgrade (smooth/dark
frames, fades). Fix = same recipe: 512-bit moment-canonicalized hash per frame,
`video_hash: [[u64; 8]; 3]`, `ENTRY_VERSION` bump with video mimes flagged stale
(requires ffmpeg re-extraction — schedule with a coverage phase, it re-reads every video).
Keep `dhash_from_image` semantics intact until then — stored hashes depend on it
(documented in `fingerprint.rs`). **Effort M; do it opportunistically with 3.4/3.5's
rescan.**

---

## Phase 4 — Forensic layer

"Find the important stuff" — the payoff stage. Framed for legitimate caretaking of an
estate: surfacing assets and vital records the deceased can no longer point you to.

### 4.1 Important-file scanner

- **Purpose**: automatically flag likely-critical files so they're reviewed first, not
  discovered after the disks are wiped.
- **UX**: a "Flagged" view: category chips (Wallets · Keys · Vaults · Identity ·
  Financial), each flagged file with the *reason* ("filename matches wallet.dat",
  "JSON with `cipher`+`kdfparams` (ethereum keystore)"), open/reveal/copy-to-sanitized
  actions. Everything is advisory — no automation touches flagged files.
- **Data/arch**: rule engine in core (`scan.rs`): rules = filename/extension patterns +
  cheap content probes on small files. Initial ruleset:
  - *Crypto wallets*: `wallet.dat` (+ Berkeley DB magic), Electrum wallet JSON, Ethereum
    keystore JSON (`crypto/cipher/kdfparams` keys), `*.wallet`, hardware-wallet backup
    naming; BIP-39 seed-phrase heuristic for small text files (≥12 consecutive words from
    the BIP-39 wordlist, embedded ~2 k-word list).
  - *Key material*: `id_rsa*`/OpenSSH key headers, `*.pem`/`*.p12`/`*.gpg`/`*.asc`,
    `.ssh/`, `.gnupg/` paths.
  - *Vaults*: `*.kdbx`/`*.kdb` (KeePass), `*.1pif`, bitwarden exports.
  - *Identity/financial*: filename keywords (multilingual list incl. German: Testament,
    Vollmacht, Steuer, Versicherung, Kontoauszug, passport/Ausweis…).
  Results stored per repo (new table or recomputed on demand — start recompute-on-demand,
  it's a filename-index scan plus small-file probes). Ruleset in code with a user-extensible
  keyword list in settings later.
- **Touchpoints**: new `dedup-core/src/scan.rs`, GUI new view + tab, CLI `dedup scan <repos…>`.
- **Acceptance**: fixture files per category are flagged with correct reasons; a photo
  library scan yields (near-)zero false positives; scanning 60 k files < a few seconds
  (index-driven, content probes gated by size+mime).
- **Effort**: L.
- **Risks**: false-positive fatigue — every rule must state its reason and be tuned toward
  precision; seed-phrase heuristic needs the 12-consecutive-words bar to stay useful.

### 4.2 Timeline organization

- **Purpose**: Stage 4 ordering — browse the sanitized corpus by *when it happened*, and
  export it into a dated folder structure.
- **UX**: a Timeline view over selected repos: year → month buckets with counts and thumb
  strips; a date-range filter; an EXPORT action that copies (never moves, v1) into
  `<target>/<year>/<month>/…` preserving filenames, using best-known date.
- **Data/arch**: `best_date(entry) = exif.taken_ms → modified_ms` (3.1 dependency; works
  degraded without EXIF). Add `date:`/`before:`/`after:` to `filter.rs` (parseable
  `YYYY[-MM[-DD]]`). Bucketing = streaming pass over `for_each_file_entry` (no new index
  until proven slow). Export reuses `diff.rs::transfer_file`.
- **Touchpoints**: `filter.rs`, new `timeline_view.rs` + tab, core helper in `dupes.rs` or
  new `organize.rs`.
- **Acceptance**: buckets match a fixture set; export creates the dated tree without
  overwrites (collision → suffix); date filter composes with mime filter.
- **Effort**: L.
- **Risks**: none structural; UI density (60 k photos) — reuse the pager/virtualization
  patterns from `dupes_view.rs`.

### 4.3 Triage report

- **Purpose**: the caretaker's audit trail — what was reduced, what remains, what's flagged.
- **Data/arch**: `dedup report <repos…>` (CLI first, GUI panel second): per repo —
  files/bytes, duplicate groups and reclaimable bytes (existing plan machinery), triage
  status (2.2 flag), flagged-file counts by category (4.1), mime histogram (`MIME_STATS`
  table already exists). Output: markdown to stdout/file.
- **Touchpoints**: `main.rs`, `store.rs` accessors (mostly existing), `scan.rs`.
- **Acceptance**: report over the integration-test repos matches known counts; runs
  read-only.
- **Effort**: S/M.
- **Risks**: none.

---

## Cross-cutting debts

- **Settings persistence** (S): threshold, quick-delete, repo read-only choices, UI scale,
  future theme — persist via `eframe` storage or a small config in the store dir. Today
  everything resets per launch.
- **Repo bar overflow** (S): with 11 repos the pill row exceeds the window; wrap it or make
  it a horizontal scroll region (solid scrollbars already in the theme).
- **Light theme toggle** (M/L, deferred by decision 2026-07-08): requires converting
  `theme.rs` constants to a runtime palette across all views; revisit after Phase 1 since
  the hairline border addressed the dark-photo pain.
- **Similar-view thresholds** (S): 100 % now means bit-identical (512-bit hash); consider
  defaulting the slider to 99 % and labeling ≥99.5 % as "identical".
- **Performance**: banded grouping and staged pipelines are fine at ~10⁵ files; revisit
  content-index memory (2.1) and timeline streaming (4.2) at 10⁷.
- **Testing discipline**: every GUI feature ships with kittest geometric tests + an
  `--ignored` render snapshot; every core feature with temp-repo integration tests; store
  format changes must include a legacy-decode test (pattern:
  `store.rs::v1_entries_decode_and_flag_images_stale`).

## Suggested sequencing

| Order | Item | Effort | Unblocks |
|-------|------|--------|----------|
| 1 | 1.5 open/reveal | S | immediate QoL, video playback story |
| 2 | 1.1 lightbox | M | 1.2, faster similar review now |
| 3 | 1.2 compare | M | Stage-3 quality decisions |
| 4 | 1.3 audio player | M | audio review |
| 5 | 1.4 video strips | M | video review |
| 6 | 2.1 multi-ref diff | M | 2.2 |
| 7 | 2.2 sanitize wizard (+CLI) | L | the core workflow |
| 8 | 2.3 provenance | M | 4.2/4.3 context |
| 9 | 3.1 EXIF | S/M | 4.2 |
| 10 | 3.2–3.5 coverage (+3.6 rescan) | M–L each | broader dedup |
| 11 | 4.1 scanner | L | the payoff |
| 12 | 4.2 timeline, 4.3 report | L, S/M | final ordering |

Cross-cutting S-items (settings, repo bar, threshold default) slot in whenever a release
touches their area.

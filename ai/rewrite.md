# Dedup Rewrite Plan — Rust + egui + redb

Decision record (2026-07-04): the Java/Javalin/React implementation is replaced by a single native Rust binary with an egui desktop UI. No web server, no browser, no JS toolchain.

**Greenfield:** the rewrite lives in a **new repository** (Cargo workspace at the root). There are no existing users and no data to migrate — the old `~/.config/dedup` contents and `.idx` format are ignored entirely. The Java repo serves only as a behavioral reference (command semantics, test scenarios, image fixtures).

- **Language:** Rust (stable toolchain, edition 2024)
- **UI:** egui via `eframe` (native window; immediate-mode — ideal for large thumbnail grids and live progress)
- **Store:** `redb` (pure-Rust embedded KV store, ACID, MVCC single-writer/multi-reader, zero C dependencies)
- **CLI:** retained and first-class (`clap`), same command surface as today where sensible
- **Result:** one static-ish binary `dedup`; `dedup` opens the GUI, `dedup <subcommand>` runs headless

Audience: an AI agent implementing one phase at a time. Each phase has acceptance criteria. Do not start a phase before the previous one's criteria pass.


### layout
3 main views: repository management. duplicate management. file management. switch tabs on top. settings cog top right for a settings modal popup

#### repository management

* add, delete, rename, duplicate, relocate, update/scan etc
* overview of the existing repositories

#### duplicate management

* top small toggleable repos with name and minimalistic description or meta data
* below duplicate selectors duplicates, similars. similars section having a threshold value selector aand a filter button for a modal of the different filters
* below that the scrollable panel with groups of similar equals with mark / delete buttons to either mark for deletion or delete immediately preselecting the duplicates / worse options. also the repo selector could mark selected repos as RO and are not deletable / never selected for deletion

#### file management

* top small toggleable repos with name and minimalistic description or meta data
* below command selection copy, delete 
* below selection of the unselected repos from first for one or more  target repos depending on command
* below that filter selection for specifying what to copy, like duplicates, uniques, mime types, etc
* below that more future adapters
* below that the first x  examples from -> to ( for delete show if deleted in source or target)

center top command selector copy, move, delete


---

## 1. Why these choices (context for implementers)

Lessons from the Java version that MUST be engineered out (see `ai/improvements.md` for the full autopsy):

| Java flaw | Rewrite answer |
| --- | --- |
| Whole index loaded into RAM on every open (`IndexManager` "MVP: we load everything to memory") | redb: indexed lookups + range scans; never materialize the full index |
| Stats recomputed by full scan on every repo list | Stats maintained incrementally in a `meta` table, updated inside the same write transaction |
| Progress flood: one event per file → UI freeze | Worker→UI channel with coalescing; UI repaints at its own frame rate and drains the channel |
| O(n²) BigInteger Hamming comparison | `u64`/`[u64;3]` fingerprints, `count_ones()`, band-bucketed candidate pruning |
| Append-only `.idx` logs + `.bak` repair dance | redb copy-on-write B-tree; crash = previous consistent state, no repair code at all |
| Arbitrary-path file read/delete over HTTP | No HTTP. The GUI is in-process; all file ops validate paths against repo roots anyway |
| Full-size images sent as "previews" | On-disk thumbnail cache (≤512px JPEG) + LRU texture cache in the GUI |

Content hash: **BLAKE3**. With no legacy data there is no reason to keep SHA-1 — BLAKE3 is an order of magnitude faster, internally parallel for large files, and 32 bytes. Keep a `hash_algo` field in repo meta anyway so the format stays honest about what it stores.

---

## 2. Workspace layout

```
dedup/
├── Cargo.toml            # workspace
├── crates/
│   ├── dedup-core/       # domain: store, scan, hash, fingerprint, diff, dupes. NO ui, NO clap.
│   ├── dedup-cli/        # clap commands, terminal progress (indicatif)
│   └── dedup-gui/        # eframe app; depends only on dedup-core
│   └── ai/               # these docs
```

Rules (mirror the hexagonal spirit of the old code):
- `dedup-core` exposes operations as plain functions/structs taking a `&Store` and an `impl Progress` callback trait. It never prints, never draws.
- `Progress` trait: `fn on(&self, event: ProgressEvent)` where `ProgressEvent` is a small enum (`Scanning{files,dirs}`, `Hashing{done,total,current}`, `Deleted{..}`, `Finished{stats}`, `Error{..}`). CLI adapter renders indicatif bars; GUI adapter pushes into a `crossbeam_channel` — **coalescing is the consumer's job, emit freely**.
- Cancellation: every long operation takes a `CancellationToken` (an `Arc<AtomicBool>` newtype), checked per file.

### Key crates

| Purpose | Crate |
| --- | --- |
| CLI | `clap` (derive) |
| Store | `redb` |
| Value encoding | `serde` + `postcard` (schema: version byte prefix) |
| Hashing | `blake3`; per-file parallelism via `rayon`, large files via blake3's internal rayon feature |
| Dir walking | `walkdir` (follow the old ResilientFileWalker semantics: skip unreadable entries, report, continue) |
| MIME detection | `infer` (magic bytes) with `mime_guess` (extension) fallback |
| Image decode + dHash + thumbnails | `image` crate; implement 64-bit dHash directly (9x8 grayscale, ~15 lines) — simpler and more controllable than a perceptual-hash dependency |
| Video fingerprint | shell out to `ffmpeg`/`ffprobe` if on PATH (extract 3 frames → dHash each = 192-bit temporal hash, same scheme as Java); feature-degrade gracefully when absent |
| PDF text hash | `lopdf` text extraction → BLAKE3 of normalized text |
| Audio | `symphonia` decode → duration + chunk hashes (port the Java scheme) |
| GUI | `eframe`/`egui`, `egui_extras` (image support), thumbnails as `ColorImage` textures |
| Errors | `thiserror` in core, `anyhow` at binary edges |

---

## 3. Data model (redb)

One redb file per repo: `~/.config/dedup/repos/<name>/index.redb`, plus a global `~/.config/dedup/repos.redb` for the registry. Values are `postcard`-encoded structs with a leading schema-version byte.

```rust
// registry (repos.redb)
REPOS: TableDefinition<&str, RepoMeta>            // name → {abs_path, created, hash_algo, schema_ver}

// per-repo (index.redb)
FILES:        TableDefinition<&str, FileEntry>    // rel_path → entry
BY_SIZE_HASH: TableDefinition<(u64, &[u8;32]), &str>   // (size, blake3) → rel_path  [multimap]
BY_FPRINT:    TableDefinition<u64, &str>          // image dHash → rel_path        [multimap]
META:         TableDefinition<&str, u64>          // "file_count", "total_size", "missing_count", ...
MIME_STATS:   TableDefinition<&str, u64>          // mime → count
```

```rust
struct FileEntry {
    size: u64,
    hash: [u8; 32],                   // blake3
    modified_ms: i64,
    missing: bool,
    mime: Option<String>,
    img_fingerprint: Option<u64>,     // dHash
    video_hash: Option<[u64; 3]>,     // temporal hash
    pdf_hash: Option<[u8; 32]>,       // blake3 of normalized text
    audio: Option<AudioFp>,           // duration_ms + chunk hashes
    img_size: Option<(u32, u32)>,
}
```

Invariants (enforce in one place, `store.rs`, and unit-test them):
- Every mutation of `FILES` updates `BY_SIZE_HASH`, `BY_FPRINT`, `META`, `MIME_STATS` **in the same write transaction**.
- `missing=true` entries stay in `FILES` (history) but are excluded from `BY_*` index tables and stats.
- Duplicate lookup is a range scan of `BY_SIZE_HASH` over `(size, hash)` — size first, matching the old lookup rule.

No migration: there are no existing users. The old `~/.config/dedup` layout and `.idx` frame formats are dead; do not implement readers for them.

---

## 4. Phases

### Phase 0 — Skeleton ✅ (2026-07-05)
Workspace, CI (`cargo fmt --check`, `clippy -D warnings`, `cargo test`), `dedup --version`.
**Accept:** builds on stable; CI green.

### Phase 1 — Store + registry ✅ (2026-07-05)
`dedup-core::store` with the tables above; repo create/ls/rm/rename/relocate in core + CLI (`dedup repo create <name> <path>`, `ls`, `rm`, `mv`, `rel`).
**Accept:** unit tests cover invariants (index-table consistency, missing-exclusion); `repo ls` shows stats from `META` without scanning `FILES`. Covered by `store.rs` unit tests plus CLI integration tests (`crates/dedup-cli/tests/repo_cli.rs`).

### Phase 2 — Scan & update ✅ (2026-07-05)
`update` operation: walkdir → compare (path, size, mtime) against `FILES` → hash changed/new files with rayon (`-t` thread count) → batch writes (commit every ~1000 entries to keep write txns short) → mark vanished files missing. Progress events + cancellation. CLI: `dedup repo update <name>... | -a` with indicatif bars.
**Accept:** integration test on a tempdir (create/modify/delete files, assert index state); re-running update on unchanged tree does zero hashing; Ctrl-C cancels cleanly mid-hash. Covered by `crates/dedup-core/tests/update_repo.rs` (lifecycle, zero-rehash, mid-hash cancellation, unreadable files) and `crates/dedup-cli/tests/update_cli.rs`.

### Phase 3 — Dupes & diff (headless)
Exact dupes via `BY_SIZE_HASH` range scans (single repo and cross-repo). Diff print/cp/mv/rm/sync ported from `DiffProcess`/`FilesProcess` semantics (source vs reference by size+hash). Group sorting: image area desc, size desc, oldest first (the fixed B3 behavior from improvements.md — sorted from day one).
**Accept:** port the scenarios from `DiffProcessSyncTest`/`DiffProcessMoveTest`/`DuplicateRepoProcessTest` as Rust tests.

### Phase 4 — Fingerprints & similarity
dHash for images (+ dimensions), ffmpeg-based temporal hash, pdf text hash, audio chunk hash — computed during `update` per MIME. Similarity grouping: band-bucket `u64` fingerprints (4×16-bit bands; candidates share ≥1 band), then exact Hamming with `count_ones()`; threshold semantics identical to Java (`similarity % = (1 - dist/bits) * 100`).
**Accept:** criterion bench: 50k random fingerprints grouped < 1s; unit tests for known-similar image pairs (copy the fixtures from the Java repo's test resources).

### Phase 5 — GUI shell (eframe)
`dedup` (no args) opens the window. Left panel: repo list with cached stats, create/delete/update buttons. Worker thread runs core operations; progress via `crossbeam_channel`; UI drains the channel each frame and keeps only the latest progress per repo (coalescing), `ctx.request_repaint_after(100ms)` while workers are active.
**Accept:** updating a 100k-file repo keeps the UI at interactive frame rates (no per-event repaint); cancel button stops within one file.

### Phase 6 — Duplicate review UI (the core screen)
Grid of duplicate groups, lazily paged from the store (never hold all groups in memory — iterate the range scan and page by 50 groups). Per file card: thumbnail, path, size, dimensions, mtime, keep/delete toggle (keyboard: arrows + space + enter). Thumbnails: disk cache `~/.cache/dedup/thumbs/<blake3-hex>.jpg` (≤512px), generated on a background thread pool, LRU texture cache (~200 textures) in the app. Delete applies `missing=true` index updates batched per repo in one transaction (fixes Java L3). "Auto-resolve rest" uses the Phase-3 sort order and shows a count + confirmation first.
**Accept:** reviewing 10k groups scrolls smoothly; deleting a 200-file batch is a single write txn per repo; nothing is deleted without an explicit confirm.

### Phase 7 — File ops + similarity UI, polish
Diff/sync/copy/move screens (port the FileOperationsView concept: source repos, reference repo, filter mime/name/size). Similarity search with threshold slider reusing the review grid. `--ui-scale`, dark/light follow system.
**Accept:** feature-parity checklist against README command table complete; `verify-release`-style pass (fmt, clippy, tests, audit) green.

---

## 5. Explicit non-goals

- No web server, no remote access, no multi-user. If remote is ever needed, expose the core as a separate `dedup serve` JSON-RPC later — not now.
- No Windows-specific work in v1 (target Linux; keep code portable, `cfg` nothing gratuitously).
- No plugin system, no config beyond what the CLI flags cover.
- Do not port: EventBus replay, WebSocket throttling, the React app, Javalin routes — all obsolete by architecture.

## 6. Risks / open items

- **ffmpeg dependency** for video fingerprints is external; degrade to size+hash-only for video when absent (warn once). Revisit `ffmpeg-next` bindings only if shelling out proves painful.
- **redb file locking:** one process at a time per repo file. GUI + CLI concurrently on the same repo will contend — detect the lock error and print "repo is open in another dedup instance".
- Check current versions of redb/egui at implementation start (this plan was written against knowledge as of early 2026).

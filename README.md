# dedup-rs

A high-performance, native Rust file deduplication tool featuring both a rich desktop GUI and a fast command-line interface. 

This project is a greenfield rewrite of the legacy Java/Javalin/React `dedup` implementation. It replaces the heavy JVM runtime, browser interface, and JS toolchain with a single, self-contained native binary.

### Core Technologies
- **Language**: Rust (edition 2024)
- **Database/Store**: `redb` (pure-Rust embedded B-tree KV store, ACID, MVCC single-writer/multi-reader, zero C dependencies)
- **UI**: `egui` via `eframe` (native, lightweight immediate-mode GUI)
- **CLI**: `clap` (derive-based CLI arguments parser)
- **Hashing**: `blake3` (modern, ultra-fast parallel hashing)
- **Fingerprints**: image/video dHash (`image`), PDF text (`lopdf`), audio (`symphonia`), MIME via `infer`/`mime_guess`; video frames via `ffmpeg`
- **Audio preview**: `rodio` (GUI). On Linux this links against ALSA, so building `dedup-gui` needs the ALSA development headers — install `libasound2-dev` (Debian/Ubuntu) or `alsa-lib-devel` (Fedora). At runtime, playback degrades gracefully when no audio device is present.

---

## Workspace Layout

The project is structured as a Cargo workspace with distinct crates to enforce architectural boundaries:

```
dedup-rs/
├── Cargo.toml            # Workspace configuration
├── crates/
│   ├── dedup-core/       # Pure domain logic: store, scanning, hashing, fingerprints, duplicates detection
│   ├── dedup-cli/        # CLI binary target (clap integration, progress bars)
│   └── dedup-gui/        # GUI library target (egui/eframe desktop application)
├── ai/
│   └── rewrite.md        # The rewrite plan & criteria
├── README.md             # This file
└── AGENTS.md             # Development standards & rust guidelines
```

- **`dedup-core`** exposes operations as plain functions/structs taking a `&Store` and a `Progress` callback trait. It contains no CLI code and no UI code.
- **`dedup-cli`** provides command-line interaction and runs headless subcommands. If run without args, it delegates execution to `dedup-gui`.
- **`dedup-gui`** handles the desktop user interface. It depends strictly on `dedup-core` and does not run CLI commands directly.

---

## Quick Start & Verification

### Running the App
- **Run GUI**: `cargo run` (runs the CLI binary which boots up the GUI window)
- **Run CLI Subcommand**: `cargo run -- <subcommand>` (e.g. `cargo run -- --version`)

### Desktop GUI
Launching with no subcommand opens an LCARS-inspired (`eframe`/`egui`) desktop window with three tabs and a settings cog:

- **Repository management** (implemented) — an overview of registered repositories with cached stats (files, size, missing) and controls to add, delete, rename, relocate, duplicate, and update/scan. Scans run on a background thread with live, coalesced progress and a per-repo cancel button, so the UI stays responsive on large repositories.
- **Duplicate management** (implemented) — toggle which repos to search (each with a read-only flag whose files are never selected for deletion), choose exact duplicates or perceptual similars (threshold slider), and review paged groups (50 per page) of file cards with thumbnail, path, size, dimensions, mtime, and a KEEP/DELETE toggle (best copy starred). Thumbnails are cached on disk (`~/.cache/dedup/thumbs`) and decoded on background threads into an LRU texture cache. Deletions are batched per repo and always confirmed; "Auto-resolve rest" preselects every non-best copy in a deletable repo.
- **File management** (implemented) — pick a source repo and a target repo, choose copy / move / delete, narrow with a mime/name/size filter, preview the first `from → to` transfers, and run it on a background thread behind a confirmation (copy/move transfer content the target lacks; delete removes source files the target already has). Content is compared by size + hash, never path. Running an operation keeps both repo indexes in sync as it proceeds — copies/moves are recorded in the target repo index and moved/deleted files are marked missing in the source — and shows live progress (a spinner, the file currently being handled, the last actions, and a running count) styled like the scan progress. PREVIEW and RUN are mutually exclusive: pressing RUN clears the preview and pressing PREVIEW clears the run log. For copy/move you can also set a relative subfolder inside the target: type a path in the **INTO** field or use **BROWSE** to pick (or create) a folder inside the target repo. Files keep their source-relative path under that subfolder (e.g. `photos/2020/a.jpg` into `imports/batch1` lands at `<target>/imports/batch1/photos/2020/a.jpg`); an empty value places them at the target root.

Settings (cog, top-right) currently exposes the hashing thread count used by scans. Launch with `--ui-scale <0.5–3.0>` to scale the interface (e.g. `dedup --ui-scale 1.25`).

### CLI Commands

| Command | Description |
| ------- | ----------- |
| `dedup repo create <name> <path>` | Register a directory as a repository |
| `dedup repo ls` | List repositories with cached stats (files, size, missing) |
| `dedup repo rm <name>` | Remove a repository and its local index database |
| `dedup repo mv <name> <new-name>` | Rename a repository |
| `dedup repo rel <name> <new-path>` | Point a repository at a new directory |
| `dedup repo cp <source> <dest> <path>` | Copy a repository's index into a new one at a new path (source unchanged) |
| `dedup repo update <name>... \| --all [-t N]` | Scan directories, hash new/changed files (BLAKE3), mark vanished files missing |
| `dedup repo dupes <name>... \| --all [--delete]` | Find exact duplicate groups (also across repos); `--delete` keeps the best copy |
| `dedup repo dupes <name>... --threshold <1-100>` | Similarity search: group perceptually similar images/video/PDF/audio at the given percent |
| `dedup diff print <source> <reference>` | Classify source files as new / equal / deleted-in-reference (by content) |
| `dedup diff cp <source> <reference> <dir> [-i/--into <rel>]` | Copy files whose content the reference does not know into a directory; `--into` places them under a relative subfolder inside `<dir>` |
| `dedup diff mv <source> <reference> <dir> [-i/--into <rel>]` | Same as `cp` but moves and marks the source entries missing |
| `dedup diff rm <source> <reference>` | Delete source files whose content the reference already has |
| `dedup diff sync <source> <target>` | Copy new content into the target repo; `--delete-missing` / `--mirror` also delete |
| `dedup sanitize <disk> <sanitized> [--ref <repo>...]` | Scan a disk, copy its content unique against the sanitized repo and any extra references into the sanitized repo, then mark the disk triage-done |
| `dedup archive index <repo>` | Index each archive's members (zip/tar/tar.gz) by content identity (opt-in, reads each archive) |
| `dedup archive coverage <repo> [--ref <repo>...]` | Report how much of each archive already exists as loose content; `--redundant-only` lists fully-redundant archives |
| `dedup scan <repo>... | --all` | Flag likely-critical files (wallets, keys, vaults, identity/financial docs) with reasons; advisory and read-only |
| `dedup report <repo>... | --all` | Print a Markdown triage report: files/bytes, duplicate groups and reclaimable bytes, triage status, flagged-file counts, top MIME types |
| `dedup timeline <repo>... [--export <dir>]` | List files bucketed by year/month of their best-known date, or export into a dated tree; supports `date:`/`before:`/`after:` filters |

`diff print`/`cp`/`mv`/`rm` accept additional reference repos via a repeatable `--ref <repo>` (the positional reference stays as sugar): a source file counts as "new" only when *none* of the references already has its content, so `dedup diff cp A sanitized /out --ref disk1 --ref disk2` copies only what is unique against the sanitized dir and every already-processed disk. The primary (positional) reference is the copy-back target. In the GUI's File Management tab, the "ALSO REF" row toggles extra reference repos beyond the target.

All `diff` commands take `-f/--filter` with one or more of `mime:<substring>`, `name:<substring>`, `size:<op><bytes>` (e.g. `size:>=1000`), and `origin:<substring>` (the repo a file was copied from). Multiple space-separated fields are combined with AND (e.g. `-f "mime:image/ name:2020 size:>=1000"` matches images whose path contains `2020` and are at least 1000 bytes); a `name:`/`mime:` value is taken verbatim and may itself contain spaces, so keep other field prefixes out of a substring value. In the GUI the filter is an assisted pill builder over these same three fields: `+` adds a MIME / NAME / SIZE condition, MIME values get clickable suggestions from the source repo's actual MIME types, NAME values show a live count of matching files, previously entered values are offered as quick-picks (remembered across sessions in `filter_history.json`), and whole condition sets can be saved as named presets and exported/imported as JSON. Content equality is always size + BLAKE3 hash — paths never matter. `diff cp`/`diff mv` also accept `-i/--into <rel-path>` to place transferred files under a relative subfolder inside the target while preserving their source-relative paths (e.g. `dedup diff cp A B /backups --into imports/batch1`); the subfolder is created if missing, and paths escaping the target (absolute or containing `..`) are rejected.

`repo update` shows live scan/hash progress and can be cancelled with Ctrl-C; already-hashed files stay committed. Alongside the content hash it detects each file's MIME type and computes a perceptual fingerprint by kind: a 64-bit image dHash (rotation/mirror invariant, with pixel dimensions), a 192-bit video temporal hash (three frames via `ffmpeg`/`ffprobe` — install ffmpeg to enable it; video degrades to content hash only when absent), a normalized-text hash for PDFs and office documents (docx/xlsx/pptx/odt/ods/odp and legacy xls — same text in any container, or as a PDF, hashes identically), and a duration + chunk hash for audio. `repo dupes --threshold <1-100>` then groups perceptually similar files (`similarity % = (1 - hamming_distance / bits) * 100`).

### Quality Checks
Ensure the code passes all standards before committing:
```bash
# Format check
cargo fmt --check

# Clippy check (must have zero warnings)
cargo clippy -- -D warnings

# Run all tests
cargo test
```

---

## Documentation & Guidelines
- [**Agent Guidelines & Best Practices**](AGENTS.md) — Rust patterns, formatting rules, error-handling policies, and coding standards.
- [**Rewrite Plan**](ai/rewrite.md) — The phase-by-phase migration plan from the legacy Java codebase.
- [**Changelog**](CHANGELOG.md) — Record of user-facing changes and phases completed.

## License
Apache-2.0 License.

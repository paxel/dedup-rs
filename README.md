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

### CLI Commands

| Command | Description |
| ------- | ----------- |
| `dedup repo create <name> <path>` | Register a directory as a repository |
| `dedup repo ls` | List repositories with cached stats (files, size, missing) |
| `dedup repo rm <name>` | Remove a repository and its local index database |
| `dedup repo mv <name> <new-name>` | Rename a repository |
| `dedup repo rel <name> <new-path>` | Point a repository at a new directory |
| `dedup repo update <name>... \| --all [-t N]` | Scan directories, hash new/changed files (BLAKE3), mark vanished files missing |
| `dedup repo dupes <name>... \| --all [--delete]` | Find exact duplicate groups (also across repos); `--delete` keeps the best copy |
| `dedup repo dupes <name>... --threshold <1-100>` | Similarity search: group perceptually similar images/video/PDF/audio at the given percent |
| `dedup diff print <source> <reference>` | Classify source files as new / equal / deleted-in-reference (by content) |
| `dedup diff cp <source> <reference> <dir>` | Copy files whose content the reference does not know into a directory |
| `dedup diff mv <source> <reference> <dir>` | Same as `cp` but moves and marks the source entries missing |
| `dedup diff rm <source> <reference>` | Delete source files whose content the reference already has |
| `dedup diff sync <source> <target>` | Copy new content into the target repo; `--delete-missing` / `--mirror` also delete |

All `diff` commands take `-f/--filter` with `mime:<substring>`, `name:<substring>`, or `size:<op><bytes>` (e.g. `size:>=1000`). Content equality is always size + BLAKE3 hash — paths never matter.

`repo update` shows live scan/hash progress and can be cancelled with Ctrl-C; already-hashed files stay committed. Alongside the content hash it detects each file's MIME type and computes a perceptual fingerprint by kind: a 64-bit image dHash (rotation/mirror invariant, with pixel dimensions), a 192-bit video temporal hash (three frames via `ffmpeg`/`ffprobe` — install ffmpeg to enable it; video degrades to content hash only when absent), a normalized-text hash for PDFs, and a duration + chunk hash for audio. `repo dupes --threshold <1-100>` then groups perceptually similar files (`similarity % = (1 - hamming_distance / bits) * 100`).

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

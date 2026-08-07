# Dedup Rust — Agent Instructions

Dedup Rust (`dedup-rs`) is a high-performance, native Rust file deduplication tool featuring both a rich desktop GUI and a fast command-line interface. It is a greenfield rewrite of the legacy Java/Javalin/React `dedup` implementation.

## Core Technologies
- **Language**: Rust (edition 2024).
- **Store**: `redb` (pure-Rust B-tree KV store).
- **UI**: `egui` via `eframe` (native desktop GUI).
- **CLI**: `clap` (arguments parser).
- **Hashing**: `blake3` (parallel hashing).
- **Image Fingerprints**: dHash (64-bit/192-bit) via `image`.

## Architecture (Workspace Layout)
Strict modular layout to enforce architectural boundaries:
1. **Domain Layer (`crates/dedup-core`)**:
   - Contains pure domain logic (hashing, scanning, similarity, B-tree schema, and query methods).
   - **No framework dependencies**: MUST NOT import `egui`, `eframe`, or `clap`.
   - Communicates progress to CLI/GUI using callback traits (e.g. `Progress`).
2. **CLI Layer (`crates/dedup-cli`)**:
   - Implements CLI subcommands using `clap` and renders terminal progress via `indicatif`.
   - Delegates to `dedup-gui` if invoked without subcommands.
3. **GUI Layer (`crates/dedup-gui`)**:
   - Implements the desktop user interface using `egui`/`eframe`.
   - Communicates with worker threads running core operations via `crossbeam_channel` to avoid UI freezing.

## Rust Standards
- **`unwrap()` / `expect()` are forbidden.** Handle all fallible operations with `?`, `match`, or `if let`.
- **Errors**: Use `thiserror` for core/library errors; propagate with `?` and convert at boundaries. Use `anyhow` at the CLI/GUI entry points.
- **Lint**: `cargo clippy -- -D warnings` must be clean. `#[allow(...)]` is forbidden unless strictly necessary.
- **Style**: `rustfmt` defaults; run `cargo fmt` before considering work done.
- **Ownership**: Prefer borrowing over cloning; use `Arc<T>` for shared state.
- **Testing**: Write unit tests for core functionalities. Assert actual values/index state.

## Build, Test, Verify
```bash
cargo build
cargo run                    # Runs the CLI binary which boots up the GUI window
cargo test                   # Runs all tests
cargo fmt --check            # Formatter check
cargo clippy -- -D warnings  # Linting check
```

## Documentation Upkeep
Update these as part of the same change, not as a follow-up:
- **`README.md`**: Outlines features, workspace layout, and running instructions.
- **`CHANGELOG.md`**: Every user-facing change under Keep a Changelog format.
- **`ai/improvements.md`**: Check off/update task status when completing roadmap phases.

## Working Agreements
- Do not "fix everything" in one pass—one focused change, tested, matching the current layout.
- Don't guess: if a change's relation to the request is unclear, ask rather than assume.

# Dedup Rust — Agent Instructions

Dedup Rust (`dedup-rs`) is a high-performance, native Rust file deduplication tool featuring both a rich desktop GUI and a fast command-line interface. It is a greenfield rewrite of the legacy Java/Javalin/React `dedup` implementation.

## Core Technologies
- **Language**: Rust (edition 2024), toolchain pinned in `rust-toolchain.toml`.
- **Store**: `redb` (pure-Rust B-tree KV store).
- **UI**: `egui` via `eframe` (native desktop GUI).
- **CLI**: `clap` (arguments parser).
- **Hashing**: `blake3` (parallel hashing).
- **Image Fingerprints**: dHash (64-bit/192-bit) via `image`.

## Build, Test, Verify

```bash
cargo build
cargo run                        # GUI (the CLI binary delegates to dedup-gui when run without args)
cargo run -- <subcommand>        # CLI, e.g. cargo run -- repo ls
cargo test                       # all tests
cargo test -p dedup-rs-core --test diff_ops          # one integration test file
cargo test -p dedup-rs-gui filter_ui                 # GUI tests matching a name
cargo fmt --check                # must pass before work is done
cargo clippy --locked --all-features -- -D warnings  # shipped code: zero warnings
cargo clippy --locked --tests --benches --examples --all-features -- -D warnings \
  -A clippy::unwrap_used -A clippy::expect_used -A clippy::panic -A clippy::indexing_slicing \
  -A clippy::string_slice -A clippy::arithmetic_side_effects -A clippy::unreachable  # tests
cd fuzz && cargo fuzz run fingerprint   # fuzz a parser target (nightly, pinned in fuzz/)
```

The two clippy runs are the ones the shared Rust gate of [paxel/gates](https://github.com/paxel/gates) runs in CI: shipped code under the whole lint set, tests with the lints allowed that are how a test fails.

Building `dedup-gui` on Linux needs ALSA headers (`libasound2-dev` on Debian/Ubuntu) because of `rodio`. Video fingerprinting requires `ffmpeg`/`ffprobe` at runtime (degrades gracefully without).

GUI rendering is verified by `#[ignore]`d render tests that write a PNG for you to look at — `doc_screenshot_*` (into `docs/screenshots/`) and `render_*` (into `target/`). There is no pixel-diff baseline: it was renderer-specific, so it could only ever pass on the machine that generated it. Run them explicitly:
```bash
cargo test -p dedup-rs-gui doc_screenshot -- --ignored
```

### Coverage ratchet

CI measures line coverage per crate (`core`, `cli`, `gui`) and fails below the floors in `coverage-floors.toml`. After a rise, measure each part and let the ratchet of paxel/gates raise the floors and rewrite `.github/badges/coverage-<part>.json`; commit both with the change that earned them. Measure on a machine with lavapipe, ffmpeg and poppler, as CI does:
```bash
cargo tarpaulin --engine llvm --locked --all-features -p dedup-rs-core --timeout 300 --out Lcov --output-dir coverage && mv coverage/lcov.info coverage/core.lcov   # likewise cli, gui
python3 <gates>/scripts/coverage_ratchet.py --floors coverage-floors.toml
```

## Architecture

Three-crate workspace with strict boundaries. The **package names** are `dedup-rs-core` / `dedup-rs-cli` / `dedup-rs-gui` (the short names are taken on crates.io) — use those with `-p` — while directories, lib targets (`dedup_core`, `dedup_gui`) and the `dedup` binary keep the short names:

- **`crates/dedup-core`** — pure domain logic: redb store, scanning/hashing, perceptual fingerprints, duplicate/diff/similarity operations. **No framework dependencies**: MUST NOT import `egui`, `eframe`, or `clap`. Long-running operations are plain functions taking a `&Store` plus a `Progress` callback trait; errors use `thiserror`.
- **`crates/dedup-cli`** — `clap` subcommands with `indicatif` progress; `anyhow` at the entry point. Invoked without a subcommand it launches the GUI.
- **`crates/dedup-gui`** — `egui`/`eframe` **0.37** desktop app. Core operations run on worker threads (`worker.rs`) talking to the UI via `crossbeam_channel`; the UI thread never blocks.

### Data model (`dedup-core/src/store.rs`)

A registry database maps repo names to paths; each repo has its own redb database with a `files` table plus index tables (`by_size_hash`, `by_fprint2`, mime stats, archive members, tags). **Content identity is always size + BLAKE3 hash — paths never matter.** Serialized `FileEntry` values carry `ENTRY_VERSION`; bumping it (with migration notes in the doc comment) flags older entries stale so they re-hash on the next scan. Fingerprints are per-kind: rotation-invariant image dHash, 3-frame video temporal hash (ffmpeg), normalized-text hash for PDF/office/text/eml, duration + Chromaprint acoustic fingerprint of the first 120 s of decoded audio (`rusty-chromaprint`, symphonia decode with ffmpeg fallback).

### GUI conventions

- The UI follows an LCARS design system: build sections/buttons with `lcars.rs` (`section_lcars`, stadium buttons) and the condensed font — don't hand-roll egui widgets.
- Shared widgets are reused across tabs: repo selectors go through `repo_chip.rs`, filter editing through `filter_ui::FilterBuilder`. Don't build downgraded per-tab variants.
- GUI tests live in inline `#[cfg(test)]` modules using `egui_kittest` harnesses (headless wgpu/lavapipe): geometric asserts on real layout, plus the ignored render tests above. A label query passes even when the widget is clipped or off-screen — assert rects, and render anything whose layout matters.
- Tooltip text is end-user product copy — never mention implementation details in it.

### Testing style

Prefer integration tests over manual binary runs: core behavior is verified in `crates/dedup-core/tests/`, CLI behavior with `assert_cmd` in `crates/dedup-cli/tests/`. Write unit tests for core functionality. Assert actual values and resulting index state.

## Rust Standards

- **Shipped code may not panic.** The workspace lint set (`[workspace.lints]` in `Cargo.toml`, the paxel/gates set) denies `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, `unreachable!`, indexing and slicing that can go out of bounds, string slicing, arithmetic that can overflow and `dbg!`. Handle all fallible operations with `?`, `match`, or `if let`. `unsafe` is forbidden.
- **No `#[allow(...)]`.** A lint that must be silenced says so with `#[expect(<lint>, reason = "...")]`, which fails once the lint no longer fires.
- **Errors**: Use `thiserror` for core/library errors; propagate with `?` and convert at boundaries. Use `anyhow` at the CLI/GUI entry points.
- **Style**: `rustfmt` defaults; run `cargo fmt` before considering work done.
- **Ownership**: Prefer borrowing over cloning; use `Arc<T>` for shared state.

## Documentation Upkeep
Update these as part of the same change, not as a follow-up:
- **`README.md`**: Outlines features, workspace layout, and running instructions.
- **`CHANGELOG.md`**: Every user-facing change under Keep a Changelog format.
- **`ai/improvements.md`**: Check off/update task status when completing roadmap phases.

## Working Agreements
- Do not "fix everything" in one pass—one focused change, tested, matching the current layout.
- Don't guess: if a change's relation to the request is unclear, ask rather than assume.

## Agent skills

### Issue tracker

Issues and specs live as GitHub issues on `paxel/dedup-rs` (via the `gh` CLI); `.scratch/` holds the pre-GitHub archive. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, each label string equal to its name, recorded as a `Status:` line in each issue file. See `docs/agents/triage-labels.md`.

### Ticket IDs

Implementation tickets carry the ID `dedup-<issue number>` at the start of their title. See `docs/agents/ticket-ids.md`.

### Domain docs

Single-context — `CONTEXT.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

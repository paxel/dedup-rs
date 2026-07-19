# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build
cargo run                        # GUI (the CLI binary delegates to dedup-gui when run without args)
cargo run -- <subcommand>        # CLI, e.g. cargo run -- repo ls
cargo test                       # all tests
cargo test -p dedup-core --test diff_ops          # one integration test file
cargo test -p dedup-gui filter_ui                 # GUI tests matching a name
cargo fmt --check                # must pass before work is done
cargo clippy -- -D warnings      # must be clean (zero warnings)
```

Building `dedup-gui` on Linux needs ALSA headers (`libasound2-dev` on Debian/Ubuntu) because of `rodio`. Video fingerprinting requires `ffmpeg`/`ffprobe` at runtime (degrades gracefully without).

GUI image-snapshot tests are `#[ignore]`d (renderer-specific baselines in `crates/dedup-gui/tests/snapshots/`). Run explicitly and regenerate after an intentional visual change with:
```bash
UPDATE_SNAPSHOTS=1 cargo test -p dedup-gui dupes_view_snapshot -- --ignored
```

## Architecture

Three-crate workspace with strict boundaries (see `AGENTS.md`):

- **`crates/dedup-core`** — pure domain logic: redb store, scanning/hashing, perceptual fingerprints, duplicate/diff/similarity operations. MUST NOT import `egui`, `eframe`, or `clap`. Long-running operations are plain functions taking a `&Store` plus a `Progress` callback trait; errors use `thiserror`.
- **`crates/dedup-cli`** — `clap` subcommands with `indicatif` progress; `anyhow` at the entry point. Invoked without a subcommand it launches the GUI.
- **`crates/dedup-gui`** — `egui`/`eframe` **0.35** desktop app. Core operations run on worker threads (`worker.rs`) talking to the UI via `crossbeam_channel`; the UI thread never blocks.

### Data model (`dedup-core/src/store.rs`)

A registry database maps repo names to paths; each repo has its own redb database with a `files` table plus index tables (`by_size_hash`, `by_fprint2`, mime stats, archive members, tags). **Content identity is always size + BLAKE3 hash — paths never matter.** Serialized `FileEntry` values carry `ENTRY_VERSION`; bumping it (with migration notes in the doc comment) flags older entries stale so they re-hash on the next scan. Fingerprints are per-kind: rotation-invariant image dHash, 3-frame video temporal hash (ffmpeg), normalized-text hash for PDF/office/text/eml, duration+chunk hash for audio.

### GUI conventions

- The UI follows an LCARS design system: build sections/buttons with `lcars.rs` (`section_lcars`, stadium buttons) and the condensed font — don't hand-roll egui widgets.
- Shared widgets are reused across tabs: repo selectors go through `repo_chip.rs`, filter editing through `filter_ui::FilterBuilder`. Don't build downgraded per-tab variants.
- GUI tests live in inline `#[cfg(test)]` modules using `egui_kittest` harnesses (headless wgpu/lavapipe): geometric asserts on real layout, plus the ignored snapshot tests above.
- Tooltip text is end-user product copy — never mention implementation details in it.

### Testing style

Prefer integration tests over manual binary runs: core behavior is verified in `crates/dedup-core/tests/`, CLI behavior with `assert_cmd` in `crates/dedup-cli/tests/`. Assert actual values and resulting index state.

## Standards (from AGENTS.md — read it in full)

- `unwrap()` / `expect()` are forbidden; handle fallible operations with `?`, `match`, or `if let`. `#[allow(...)]` is forbidden unless strictly necessary.
- Prefer borrowing over cloning; `Arc<T>` for shared state.
- Documentation upkeep is part of the same change, not a follow-up: `README.md` (features/usage), `CHANGELOG.md` (Keep a Changelog format, user-facing changes), `ai/rewrite.md` (phase status).
- One focused change per pass; if a change's relation to the request is unclear, ask rather than assume.

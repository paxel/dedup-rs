# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.1.0] - 2026-07-05

### Added
- Phase 0 Skeleton: Created Cargo workspace with crates `dedup-core`, `dedup-cli`, and `dedup-gui`.
- Integrated `clap` for version reporting and basic command line arguments.
- Copied `rewrite.md` planning document into `ai/rewrite.md`.
- Added [README.md](README.md) and [AGENTS.md](AGENTS.md) matching Sanshain standards.
- Phase 1 Store + registry: `redb`-backed store with per-repo index databases and repo CLI commands `create`, `ls`, `rm`, `mv`, `rel`.
- Phase 2 Scan & update: `dedup repo update <name>... | --all` walks repository directories, hashes new/changed files with BLAKE3 (parallel via `rayon`, `-t/--threads`), batches index writes (~1000 entries per transaction), and marks vanished files missing. Terminal progress via `indicatif`; Ctrl-C cancels cleanly mid-hash.
- CLI integration test suites for all repo commands and update (`assert_cmd` against an isolated HOME), plus core integration tests covering the Phase 2 acceptance criteria (lifecycle, zero re-hashing on unchanged trees, cancellation, unreadable files).


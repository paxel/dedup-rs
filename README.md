# dedup-rs

A high-performance, native Rust file-deduplication tool with both a desktop GUI and a
fast command-line interface, packaged as a single self-contained binary.

Content identity is always size + BLAKE3 hash — paths never matter — with per-kind
perceptual fingerprints (image, video, PDF/office/text, audio) for near-duplicate search.
It is a three-crate workspace: `dedup-core` (domain logic), `dedup-cli` (`clap` CLI), and
`dedup-gui` (an `egui`/`eframe` LCARS-styled desktop app), backed by a `redb` embedded store.

## Building & running

```bash
cargo run                 # GUI (the CLI binary launches the GUI when given no subcommand)
cargo run -- <subcommand> # CLI, e.g. cargo run -- repo ls
```

Building the GUI on Linux needs ALSA headers (`libasound2-dev` on Debian/Ubuntu,
`alsa-lib-devel` on Fedora) for audio preview. Video fingerprinting uses `ffmpeg`/`ffprobe`
at runtime and degrades gracefully when they are absent.

## What it does

- **Repositories** — register directories, scan and hash them on background threads, track per-repo stats. Backup groups are framed together, with the group's **main** badged wherever it is named.
- **Duplicates** — find exact or perceptually-similar files across repos and delete the worse copies.
- **Transfer / DIFF** — copy, move, sync or mirror content between repos, reconcile two repos side by side, or push a backup group's main to its sinks (each ADD ONLY or MIRROR).
- **Grooming** — dedupe, purge by filter, prune missing records, reorganize by path templates.

Clicking any file anywhere — a duplicate card, a review-board row, a Browse listing, a DIFF
conflict — opens the **same full-window viewer**: two files side by side, tabbed by
representation (image, video, audio as spectrograms with gapless A/B playback, editable ID3
metadata, text/hex for everything else), with the actions of the surface you came from.

Most surfaces share one **filter** wizard: conditions like `mime:image`, `name:*.db` or
`size:>=1000` combine with AND, any condition can be inverted with `!` (`!name:*.mp3` —
everything that is not an MP3), and `case:insensitive` (the `Aa` toggle) makes text matching
ignore capitalisation.

Scanning is protective by default: a scan that finds **no files at all** in a repository whose
index still holds entries is refused rather than marking everything missing — an unmounted drive
is indistinguishable from an emptied one, and an emptied backup-group main would turn the next
MIRROR push into a wipe. Confirm it in the GUI, or pass `--force` on the CLI.

The same operations are available headless via `dedup <command>`.

## Documentation

- [**CLI Reference**](docs/cli.md) — every command, in depth, with examples.
- [**GUI Guide**](docs/gui/index.md) — a walkthrough of every tab and control, with screenshots.
- [**Releasing & hosting**](docs/hosting.md) — packaging and the per-release workflow.
- [**Agent Guidelines**](AGENTS.md) — Rust patterns, error-handling policy, and coding standards.
- [**Changelog**](CHANGELOG.md) — record of user-facing changes.

## License

Apache-2.0.

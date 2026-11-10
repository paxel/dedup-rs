# dedup-rs

[![CI](https://github.com/paxel/dedup-rs/actions/workflows/ci.yml/badge.svg?branch=summer)](https://github.com/paxel/dedup-rs/actions/workflows/ci.yml)
[![Security](https://github.com/paxel/dedup-rs/actions/workflows/security.yml/badge.svg?branch=summer)](https://github.com/paxel/dedup-rs/actions/workflows/security.yml)
[![CodeQL](https://github.com/paxel/dedup-rs/actions/workflows/codeql.yml/badge.svg?branch=summer)](https://github.com/paxel/dedup-rs/actions/workflows/codeql.yml)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=paxel_dedup-rs&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=paxel_dedup-rs)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/paxel/dedup-rs/badge)](https://scorecard.dev/viewer/?uri=github.com/paxel/dedup-rs)
[![Coverage](https://coveralls.io/repos/github/paxel/dedup-rs/badge.svg?branch=summer)](https://coveralls.io/github/paxel/dedup-rs?branch=summer)
[![crates.io](https://img.shields.io/crates/v/dedup-rs-cli.svg)](https://crates.io/crates/dedup-rs-cli)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success)](https://github.com/paxel/dedup-rs/blob/summer/crates/dedup-core/src/lib.rs)
[![panic-free](https://img.shields.io/badge/panic--free-linted-success)](https://github.com/paxel/dedup-rs/blob/summer/crates/dedup-core/src/lib.rs)
[![Release](https://img.shields.io/github/v/release/paxel/dedup-rs)](https://github.com/paxel/dedup-rs/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

A high-performance, native Rust file-deduplication tool with both a desktop GUI and a
fast command-line interface, packaged as a single self-contained binary.

Content identity is always size + BLAKE3 hash — paths never matter — with per-kind
perceptual fingerprints (image, video, PDF/office/text, and Chromaprint acoustic fingerprints
for audio, so the same recording matches across codecs) for near-duplicate search.
It is a three-crate workspace: `dedup-core` (domain logic), `dedup-cli` (`clap` CLI), and
`dedup-gui` (an `egui`/`eframe` LCARS-styled desktop app), backed by a `redb` embedded store.

## Installing

Every release ships prebuilt binaries on the
[releases page](https://github.com/paxel/dedup-rs/releases); the package channels below
track them automatically.

| OS      | Channel                                                                                    |
| ------- | ------------------------------------------------------------------------------------------ |
| Linux   | `brew install paxel/tap/dedup`, or the `.deb`, AppImage or plain tarball (x86_64 + arm64)  |
| macOS   | `brew install paxel/tap/dedup`, or the `.dmg` (a drag-to-Applications app bundle)          |
| Windows | `scoop bucket add paxel https://github.com/paxel/scoop-bucket` then `scoop install dedup`  |
| Any     | `cargo install dedup-rs-cli` (builds from source; binary is `dedup`)                       |

The AppImage runs on any distribution after a one-time `chmod +x`. On Linux the GUI
registers its own application-menu entry and icon the first time it runs, whichever way
it was installed.
The macOS builds are **unsigned**: on first launch, right-click the app → **Open** to get
past Gatekeeper (once; afterwards it opens normally). The Windows zip carries two
executables — `dedup.exe` (the CLI, console) and `dedup-gui.exe` (the app, no console
window; the double-click target).

Every release file carries a signed build provenance attestation: it proves the file was
built by this repository's release workflow from the tagged commit. Check a download with
the GitHub CLI:

```bash
gh attestation verify dedup-4.0.0-linux-x86_64.tar.gz -R paxel/dedup-rs
```

Optional tools unlock extra features at runtime and the app degrades gracefully without
them: `ffmpeg` (video), `poppler` (PDF page rendering), LibreOffice (office/legacy document
rendering).

## Building & running

```bash
cargo run                 # GUI (the CLI binary launches the GUI when given no subcommand)
cargo run -- <subcommand> # CLI, e.g. cargo run -- repo ls
```

Building the GUI on Linux needs ALSA headers (`libasound2-dev` on Debian/Ubuntu,
`alsa-lib-devel` on Fedora) for audio preview. Video fingerprinting — and the viewer's video
filmstrip, frame scrub, soundtrack extraction and pitch-preserving playback rates — use
`ffmpeg`/`ffprobe` at runtime and degrade gracefully when they are absent.

On a Wayland desktop the GUI runs on the X11 backend (via XWayland) so that dropping
folders onto the window works — the windowing library has no Wayland drag-and-drop yet.
Set `DEDUP_WAYLAND=1` to keep the native Wayland backend (crisper fractional scaling)
at the cost of drag-and-drop.

## What it does

- **Repository page and navigation bar** — the app opens on your repositories as cards, with a navigation bar along the top (**REPOSITORIES · DUPLICATES · SYNC · COPY · DELETE**) that switches between pages; each page chooses its own repositories. On the repository page you register directories, filter the cards by name or to LOCAL / MAINS (the rest are greyed out and left alone), select them, lock or unlock them in bulk, and run **UPDATE** or **DELETE EMPTY DIRS** on the selection — each shows what it would change first, and only OK changes anything. A backup group is framed together, main first, with the group's **main** badged wherever it is named; groups are built from each card's right-click menu.
- **Duplicates** — drag repositories into a read-only card (safe from changes) and a read-write card (copies there get deleted), choose exact or similar, and PREVIEW: the result opens in its own window with the best copy of each group kept and the rest marked, and GO deletes them. A copy's right-click menu can jump straight to it in Browse (**SHOW IN BROWSE**), to judge it by the folder it lives in, or **ACCEPT** its content as allowed to repeat inside that repo (a podcast's per-folder cover): accepted copies are protected from every duplicate delete and their groups stay out of the way.
- **Sync** — drag backup groups into one card and push each main to its backups (each ADD ONLY, APPLY CHANGES — which also carries the main's own deletions over — or MIRROR), after a preview of what every backup gains and loses.
- **Copy** — copy into one repository what it and the reference repositories lack, from several sources, each content once, optionally under a subfolder and a path template; a taken name gets a numeric suffix, nothing is overwritten.
- **Delete** — delete from several repositories every file a reference repository already holds; content only those repositories share stays for DUPLICATES, and a switch decides whether a file deleted in the reference goes too.
- **One thing at a time, always visible** — long work (a repository UPDATE or DELETE EMPTY DIRS, a DUPLICATES,
  SYNC, COPY or DELETE PREVIEW or GO, or a bulk board action)
  runs in one central activity window that shows each phase, its progress and
  problems, offers CANCEL, and ends as the result report. Quick actions answer with a notification
  card, and every file the app changes is written to an event log (**LOG**, top right) that survives
  restarts. See [ADR 0003](docs/adr/0003-every-action-reports-and-long-work-runs-alone.md).

Clicking any file — a duplicate card or a Browse listing — opens the **same full-window viewer**: two files side by side, tabbed by
representation (image, video, audio as spectrograms with gapless A/B playback, editable ID3
metadata and the full EXIF listing, text/hex for everything else), with the actions of the
surface you came from. A video shows an **aligned filmstrip** per side — frames sampled
across each clip, so you see its whole shape — and clicking the strip drops a **shared
proportional playhead**: both sides decode the frame at the same *relative* moment of their
own duration (shown enlarged, A@t | B@t), so a trimmed or re-encoded copy stays aligned. A
clip that carries a soundtrack also offers the **Audio** tab: its track is extracted once to
a cached WAV and compared exactly like a bare audio file, spectrogram and gapless A/B flicker
included. The audio transport steps through **pitch-preserving speeds** (0.25×–2×), so a
slowed track still sounds like the recording you are trying to identify. A wrongly rotated scan can be turned and **saved back to disk** —
in place or as a copy, keeping the file's date (or stamping it from the EXIF capture date).
Comparing two files, the **Text tab is a full-file, aligned hex diff**: equal bytes line up
even when one side has an inserted header, differences are marked (a green gap, an amber
substitution), and you page through and jump between them — so two near-duplicate scans that
differ only in embedded metadata read as "same payload, header inserted". The **Metadata tab**
highlights which EXIF/TIFF fields differ and can save a side's metadata to a sidecar file, so
capture details are rescued before a copy is deleted; when two pictures decode identically the
image view says so plainly. **Flicker** is single-file focus — one file's facts and tools at a
time, SWAP flips to the other.

**Archives** (zip/tar/tar.gz) are read-only containers you can see into: the viewer's Archive
tab lists their members, opens a member in place by its type, and extracts members into a repo
(indexed on the next scan) or a plain folder — never modifying the source. A password-protected
zip lists its member names and offers to unlock (supplied password, a built-in attempt at
common passwords, or export the hash for hashcat), for archives you hold and are entitled to.
When a loose file's content also lives inside an archive, the Duplicates view says so and warns
before a delete would leave content surviving only inside a zip.

Most surfaces share one **filter** wizard: conditions like `mime:image`, `name:*.db`,
`size:>=1000` or `accepted:yes` combine with AND, any condition can be inverted with `!` (`!name:*.mp3` —
everything that is not an MP3), and `case:insensitive` (the `Aa` toggle) makes text matching
ignore capitalisation.

The interface ships in its classic **dark** LCARS appearance, with an opt-in **light** one;
Settings → Appearance offers System / Light / Dark (System follows your desktop), applied
immediately and remembered. Dark stays the default so an update never repaints the semantic
delete/differ colours out from under you.

Scanning is protective by default: a scan that finds **no files at all** in a repository whose
index still holds entries is refused rather than marking everything missing — an unmounted drive
is indistinguishable from an emptied one, and an emptied backup-group main would turn the next
MIRROR push into a wipe. Confirm it in the GUI, or pass `--force` on the CLI.

The same operations are available headless via `dedup <command>`.

## Documentation

- [**CLI Reference**](docs/cli.md) — every command, in depth, with examples.
- [**GUI Guide**](docs/gui/index.md) — a walkthrough of every tab and control, with screenshots.
- [**Releasing**](docs/releasing.md) — the tag-driven release workflow and its one-time setup.
- [**Agent Guidelines**](AGENTS.md) — Rust patterns, error-handling policy, and coding standards.
- [**Changelog**](CHANGELOG.md) — record of user-facing changes.

## Security

How the project scans, gates and signs its releases is recorded in
[ADR 0005](docs/adr/0005-security-hardening.md); how to report a vulnerability is in
[SECURITY.md](SECURITY.md).

## License

MIT.

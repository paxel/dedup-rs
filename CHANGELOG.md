# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.1.0] - 2026-07-20

### Added

- CLI (`dedup <command>`) and LCARS desktop app (`dedup` with no arguments).
- **Repositories** tab: create, rename, relocate, duplicate, remove and scan repositories, with per-repo stats and MIME breakdown.
- **Duplicates** tab: find exact or perceptually-similar duplicates across selected repos, review as file cards with a best-copy pick, and delete the rest; image/video/audio previews and a zoom/pan lightbox with A/B compare.
- **Transfer** tab: copy, move or sync files between repos or into a dated folder, filtered by MIME/name/size; plus **DIFF**, a per-row side-by-side reconcile of two repositories.
- **Grooming** tab: dedupe against other repos, purge by filter, remove empty directories, reorganize by path templates, and prune missing records.
- **Sync Groups** tab: keep a main repository backed up to one or more sinks in ADD ONLY or MIRROR mode.
- **Browse** tab: directory-based index browser for one repo with tag annotations.
- `dedup repo <create|ls|rm|mv|rel|cp|update|dupes>`: manage repositories and run scans; `dupes` finds exact or `--threshold` perceptual duplicates.
- `dedup diff <print|cp|mv|rm|sync>`: compare a source repo against one or more reference repos by content and apply the differences.
- `dedup timeline <repos…> [--export <dir>]`: bucket files by date (EXIF, else mtime), optionally into a `<year>/<month>/` tree.
- `dedup report <repos…>`: Markdown triage report of counts, duplicates and flagged files.
- `dedup scan <repos…>`: flag likely-critical files (wallets, keys, vaults, documents).
- `dedup archive <index|coverage>`: index archive members by content and report archive redundancy.
- Content identity by size + BLAKE3 hash, with per-kind perceptual fingerprints (image, video, PDF/office/text/eml, audio) for similarity; scans record EXIF date and file origin.

# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [3.0.1]

### Changed

- Duplicates shows each group's copies side by side again, best copy on the left, instead of
  stacked rows.
- On a Wayland desktop, programs opened from dedup (viewers, the file manager) start on
  Wayland; only dedup's own window still uses X11 for drag-and-drop.

### Security

- Dependencies with known vulnerabilities are updated: zip, calamine (and its quick-xml), zstd,
  webbrowser and ctutils; yanked releases are replaced.
- Every release file carries a signed build provenance attestation, checked with
  `gh attestation verify <file> -R paxel/dedup-rs`.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

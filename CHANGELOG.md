# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [3.1.0]

### Changed

- With `DEDUP_WAYLAND=1` on a desktop that leaves window frames to the app (GNOME), the
  window gets a plain frame instead of the Adwaita one.

### Removed

- EXR images are no longer decoded, so the viewer shows no picture for them; they are
  still found as exact duplicates.

### Security

- The GUI toolkit is updated to egui 0.37, and a quick-xml with known vulnerabilities in its
  accessibility layer is replaced.
- The Adwaita window frame's unmaintained font parser (ttf-parser) is gone.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

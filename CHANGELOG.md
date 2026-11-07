# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [3.1.0]

### Removed

- EXR images are no longer decoded, so the viewer shows no picture for them; they are
  still found as exact duplicates.

### Security

- The GUI toolkit is updated to egui 0.37, and a quick-xml with known vulnerabilities in its
  accessibility layer is replaced.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

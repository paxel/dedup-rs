# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [3.1.0]

### Changed

- Duplicates shows each group's copies side by side again, best copy on the left, instead of
  stacked rows.
- On a Wayland desktop, programs opened from dedup (viewers, the file manager) start on
  Wayland; only dedup's own window still uses X11 for drag-and-drop.
- With `DEDUP_WAYLAND=1` on a desktop that leaves window frames to the app (GNOME), the
  window gets a plain frame instead of the Adwaita one.

### Removed

- EXR images are no longer decoded, so the viewer shows no picture for them; they are
  still found as exact duplicates.

### Security

- Dependencies with known vulnerabilities are updated: zip, calamine (and its quick-xml), zstd,
  webbrowser and ctutils; yanked releases are replaced.
- A scan stops reading an archive's members at a million members or once they unpack to a
  thousand times the archive's size (at least 4 GiB); the archive is still indexed and the
  scan report names the cap.
- PDFs, spreadsheets and e-mails over 256 MiB, and office documents whose text unpacks past
  64 MiB, get no text fingerprint or text view instead of being read into memory whole.
- ffmpeg, ffprobe and poppler run with a deadline and an output cap and are stopped past
  either, so a crafted file can no longer stall a scan; ffmpeg may only open local files.
- A media file that crashes its audio decoder no longer takes the scan, the waveform view or
  the player with it; the file is indexed without a fingerprint.
- Every release file carries a signed build provenance attestation, checked with
  `gh attestation verify <file> -R paxel/dedup-rs`.
- The GUI toolkit is updated to egui 0.37, and a quick-xml with known vulnerabilities in its
  accessibility layer is replaced.
- The Adwaita window frame's unmaintained font parser (ttf-parser) is gone.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

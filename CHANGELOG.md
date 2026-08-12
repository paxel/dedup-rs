# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.2.0]

### Added

- **Repositories can be marked remote — and skipped by the everyday rescan.** Each repository
  can be **marked remote** (your own judgement that it lives on a slow NFS or cloud mount —
  never auto-detected), and a new **UPDATE LOCAL** button rescans everything *except* the
  marked ones: the quick everyday rescan. The finished-scan summary states the hash work
  explicitly ("hashed N file(s), X MB"), so a slow scan shows whether the time went to
  hashing or just to walking a slow mount. Adding a backup to a group opens its editor
  directly under the ADD REPO button that summoned it.
- **Folder pickers remember where you were.** Every "choose a folder" dialog opens at the
  parent of the last folder you picked — never back at the home directory — and the memory
  survives restarts. Dialogs with an obvious anchor (exporting into a chosen target) start
  there instead.
- **Linux arm64 builds.** Releases now also ship a Debian `.deb` and a binary tarball for
  arm64 Linux, alongside the existing x86_64 artifacts.

### Fixed

- On a **GROUP SYNC** preview with several sinks, clicking a deletion row now opens that
  row's own sink's file; it used to open the file from the wrong repository.
- Returning to the **Browse** tab no longer re-warms the folder you were in: the read-ahead
  used to re-read gigabytes from a cloud mount on every tab return; now only entering a
  different folder starts a new warm-up.
- Durations past an hour read `1:15:03` instead of `75:03`, in the audio transport and on
  the video filmstrip's frame labels alike.
- A document opened in the first seconds after launch no longer silently misses its
  **Render** tab while LibreOffice is still being probed — the tab is offered, and a
  conversion that turns out impossible says so instead.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

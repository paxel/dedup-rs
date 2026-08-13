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
- **The app puts itself in the Linux application menu.** However dedup was installed —
  brew, tarball, AppImage, or a plain binary — launching the GUI once registers its
  launcher and icon in your user menu (and gives the window its proper Wayland icon).
  The entry follows the binary if it moves, and a launcher you wrote yourself is never
  touched.
- **Linux arm64 builds, and an AppImage.** Releases now also ship arm64 Linux artifacts
  alongside the existing x86_64 ones, and both architectures gain a distribution-independent
  **AppImage** download — `chmod +x`, run, no install — next to the `.deb` and the plain
  tarball.

### Fixed

- **Losing the audio device no longer floods the terminal or pins a core — and playback
  comes back.** When the output device disappears under a live stream (suspend/resume,
  switching outputs, an audio-server restart), the app used to print an ALSA error line
  endlessly, keep one CPU core at 100% retrying the dead stream, and stay silent until
  restart — even in a session that never played audio, since the output stream was held
  open from launch. Now the device is only opened when something actually plays, a loss
  is stated once, the dead stream is released, and the next play reconnects.
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

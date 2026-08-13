# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.2.1]

### Added

- **The app puts itself in the Linux application menu.** However dedup was installed —
  brew, tarball, AppImage, or a plain binary — launching the GUI once registers its
  launcher and icon in your user menu (and gives the window its proper Wayland icon).
  The entry follows the binary if it moves, and a launcher you wrote yourself is never
  touched.

### Fixed

- **Losing the audio device no longer floods the terminal or pins a core — and playback
  comes back.** When the output device disappears under a live stream (suspend/resume,
  switching outputs, an audio-server restart), the app used to print an ALSA error line
  endlessly, keep one CPU core at 100% retrying the dead stream, and stay silent until
  restart — even in a session that never played audio, since the output stream was held
  open from launch. Now the device is only opened when something actually plays, a loss
  is stated once, the dead stream is released, and the next play reconnects.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.2.1]

### Added

- **The app puts itself in the Linux application menu.** However dedup was installed —
  brew, tarball, AppImage, or a plain binary — launching the GUI once registers its
  launcher and icon in your user menu (and gives the window its proper Wayland icon).
  The entry follows the binary if it moves, and a launcher you wrote yourself is never
  touched.

### Changed

- **REFRESH STATUS only checks reachability now.** It used to also queue a freshness
  check (a full directory walk) on every reachable repository — pressing it after
  reconnecting a drive buried you in scans to cancel, exactly the uninvited walking
  the remote flag exists to prevent. Now it just re-probes every repository's
  location/reachability and clears stale OFFLINE/MISSING states; staleness checking
  stays with CHECK and UPDATE.

### Fixed

- **A path collision is shown as a conflict, and gets an explicit OVERWRITE.** When a
  GROUP SYNC BACK candidate's path in the main is occupied by a *different* file, the
  row now shows it: the main's cell displays its own occupying file (amber DIFFERS on
  both sides) and offers **`< OVERWRITE`** — replace the main's file with the sink's,
  available only while the main is unlocked — instead of a `< COPY` the engine would
  silently refuse. The status line and RUN confirmation state the conflict count, and
  if a batch still skips such files, the result dialog now shows the skipped count and
  explains the collision — a promote that did nothing visible used to look like a bug.
- **A row's picture is clickable everywhere.** On every review board, clicking a
  thumbnail now opens the row in the viewer like clicking anywhere else on the row —
  the picture used to be the one dead spot.
- **Losing the audio device no longer floods the terminal or pins a core — and playback
  comes back.** When the output device disappears under a live stream (suspend/resume,
  switching outputs, an audio-server restart), the app used to print an ALSA error line
  endlessly, keep one CPU core at 100% retrying the dead stream, and stay silent until
  restart — even in a session that never played audio, since the output stream was held
  open from launch. Now the device is only opened when something actually plays, a loss
  is stated once, the dead stream is released, and the next play reconnects.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

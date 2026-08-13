# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.3.0]

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

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

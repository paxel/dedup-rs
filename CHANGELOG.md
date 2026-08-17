# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.4.0]

### Fixed

- **Dropping folders onto the window now works on Wayland desktops.** The
  windowing library implements no drag-and-drop on Wayland at all, so dropped
  folders never reached the app. On a Wayland session that also offers an
  XWayland display the GUI now uses the X11 backend, where drag-and-drop
  works; set `DEDUP_WAYLAND=1` to keep the native Wayland backend (crisper
  fractional scaling) without drag-and-drop.
- **A review row with a real file on both sides always opens the side-by-side
  compare.** Clicking a GROUP SYNC BACK path-conflict row used to open only the
  sink's copy, leaving the main's occupying file unseen — exactly the pair the
  conflict asks you to judge. Now every review-board click resolves both sides:
  two existing files open the comparison (path conflicts, unchanged pairs),
  and only when the other side's file exists solely in the plan does the click
  fall back to the single-file view.
- **The menu entry survives a `brew upgrade`.** The self-registered launcher
  used to pin the versioned Cellar path (`…/Cellar/dedup/0.3.0/bin/dedup`),
  which the next upgrade deletes — the menu entry then failed until the new
  binary was started by hand once. A brew-installed binary now registers the
  stable `<prefix>/bin/dedup` symlink instead, which brew repoints on every
  upgrade.
- **One-sided grooming rows open the viewer too.** Clicking a PURGE or PRUNE
  row did nothing, and an ORGANIZE row could show a spurious "could not read
  both copies" error; both now open the file in the single-file viewer (an
  ORGANIZE row's right side is the same file's future home — there is no
  second file to compare).

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

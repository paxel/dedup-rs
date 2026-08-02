# 03 — The viewer's Archive representation: look inside a zip

Status: resolved
Spec: ../spec.md
Blocked by: 01

## Problem

You cannot look inside an archive from the GUI. Clicking a zip in Browse or
Duplicates opens the shared viewer, which today has no idea what an archive is — it
falls through to the Text/hex representation and shows the zip's raw bytes.

**Decided in session:** the shared viewer (one-lightbox effort) gains an **Archive**
representation. Opening a zip lists its members; opening a member renders it *by
type* in the same viewer — an image as an image, audio as audio, text as text.

## Approach

- Add an `Archive` representation to the viewer's tab set, offered when the file
  `archive::is_archive`. It lists members (name, size, and — once ticket 02/06 land —
  a LOCKED / redundant marker per member). The list can come live from the archive
  on-demand (browsing one archive is cheap and needs no prior index).
- Opening a member is **ephemeral**: decompress that member to a temp file, build
  `FileFacts` for it, and hand it to the existing viewer machinery so it dispatches by
  representation. The temp file is cleaned up when the viewer moves on. This is
  distinct from durable extraction (ticket 05).
- A LOCKED member shows as locked and cannot be opened until the archive is unlocked
  (ticket 06); until then the Archive tab offers an UNLOCK affordance that 06 wires up.
- Never write to the source archive.

## Seam and tests

GUI seam — inline `egui_kittest` tests in the viewer, plus an `--ignored` render test:

- opening a zip shows the Archive tab with its member list (names + sizes)
- opening an image member renders it through the Image representation (assert the tab
  the opened member lands on, geometrically that a pane is drawn)
- a non-openable/locked member is shown as such and is not clickable-to-open
- the source zip is byte-identical after browsing

## Done

Standing gate green. `CHANGELOG.md`: you can now look inside an archive and open its
members. GUI docs for the viewer (Duplicates/Browse) gain the Archive representation.
Render check: a screenshot of the Archive tab with a member list, looked at.

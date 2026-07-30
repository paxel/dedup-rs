# 03 — Refresh repository statistics when the Repositories tab is shown

Status: ready-for-agent
Spec: ../spec.md

## Problem

After deleting duplicates, returning to the Repositories tab still shows the old file counts
and free space. The numbers only update after a manual reload, which makes it look as though
the deletion did not happen.

Verified in the application shell: the tab-switch match arm for the Repositories tab is
empty, so nothing re-reads the store when that tab becomes visible.

## Approach

When the Repositories tab becomes the active tab, trigger the same statistics refresh a
manual reload performs.

Two constraints:

- **The UI thread must not block.** Reading counts and free space for every repository is
  store work; route it through the existing worker channel pattern rather than reading
  synchronously in the frame.
- **Do not refresh on every frame** — only on the transition into the tab. A repeated refresh
  while the tab is merely visible would hammer the store.

This matches the established convention that repository selectors refresh on tab show rather
than offering a reload button.

## Seam and tests

GUI seam — inline `ui_tests` in the application shell, prior art the existing repository-tab
tests:

- switching to the Repositories tab after the store's contents changed results in the
  displayed figures matching the store
- staying on the tab across several frames does not re-trigger the refresh

## Done

Standing gate green. `CHANGELOG.md` and `ai/improvements.md` updated.

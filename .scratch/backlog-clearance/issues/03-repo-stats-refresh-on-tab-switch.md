# 03 — Refresh repository statistics when the Repositories tab is shown

Status: resolved
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

## Comments

**Implemented 2026-07-31.** Gate green: fmt clean, clippy clean, `cargo test --workspace`
24 suites / 0 failures.

The transition hook already existed — `ui()` had a `synced_tab != Some(tab)` block that
re-syncs the newly-shown view, with `Tab::Repositories => {}` explicitly opted out ("manages
its own cards"). That opt-out was the bug. Extracted the block into
`DedupApp::sync_shown_tab` and filled in the Repositories arm.

**Deviation from this ticket's suggested approach, deliberate.** The ticket said to route the
refresh through the worker channel rather than reading synchronously. I called `reload_all()`
directly instead, because it is already the app's convention: ~10 existing call sites invoke
it synchronously after add/scan/delete completions, and its own doc comment states the rule
("safe only when no update is running; callers gate on that"). Introducing an async path for
this one caller would add a second mechanism for the same job, against the repo's
"don't build downgraded per-tab variants" principle. It is gated on
`worker.active_count() == 0` like every other site; a skipped busy frame is harmless because
the running job's completion handler reloads.

Extracting the method was needed for testability: `App::ui` takes `&mut eframe::Frame`, which
a headless kittest harness cannot readily supply, whereas `sync_shown_tab` is the actual unit
of behaviour and asserts an observable outcome (the rows match the store).

Two tests: `switching_to_the_repositories_tab_refreshes_its_stats` removes an entry behind the
app's back and asserts the count drops from 5 to 4 after leaving and returning (it would fail
against the old empty match arm), and `staying_on_the_repositories_tab_does_not_re_read_each_frame`
pins that the re-read is per transition, not per frame.

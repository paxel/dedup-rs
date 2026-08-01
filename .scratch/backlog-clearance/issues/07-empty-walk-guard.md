# 07 — Require authorisation before a scan marks every indexed entry missing

Status: resolved
Spec: ../spec.md

## Problem

A scan that walks a repository and finds **zero** files, where the index currently holds
entries, marks every one of those entries missing. Today this is warn-only: a log line and a
notification, with nothing stopping it.

An unmounted mount point is still a directory, so the existing is-a-directory check lets it
through. The scan path's own comment states the consequence: an emptied main is what turns a
MIRROR push into a wipe of every sink.

**Decided in session:** add a real gate — a confirmation in the GUI, a `--force` flag on the
CLI. Emptying a repository on purpose stays supported; it now costs one explicit
authorisation.

## Approach

The core scan already computes the condition (a completed, uncancelled walk that saw no files
at all over a non-empty set of remaining indexed entries). Turn that from a notification into
a decision point:

- The scan entry point takes a caller-supplied authorisation for the destructive-empty case.
  Without it, the scan **refuses** — it does not mark the entries missing, and reports clearly
  why, naming the repository and the number of entries that would have been affected.
- **CLI**: a `--force` flag supplies the authorisation. Without it the command exits with a
  non-zero status and a message that names `--force` as the way to proceed.
- **GUI**: a confirmation dialog. Declining leaves the index untouched.

Only a complete, uncancelled walk can trigger this; a cancelled scan must never reach the
condition. Preserve the existing warning text — the gate is added to it, not a replacement.

## Seam and tests

Core seam — `crates/dedup-core/tests/`, prior art `update_repo.rs`:

- scanning an emptied repository **without** authorisation leaves every entry intact and
  reports the refusal
- scanning it **with** authorisation marks the entries missing, as today
- a repository that still has files is unaffected either way
- a cancelled walk never triggers the condition

CLI seam — `crates/dedup-cli/tests/`, prior art `update_cli.rs`: the command fails without
`--force` and names it; it succeeds with `--force`.

GUI seam — inline `ui_tests`: declining the confirmation leaves the index intact.

## Done

Standing gate green. `README.md` (the `--force` flag), `CHANGELOG.md`, and
`ai/improvements.md` updated. This decision meets the bar for an ADR — note it, but creating
`docs/adr/` is not part of this ticket.

## Comments

**Implemented 2026-07-31.** Gate green: fmt clean, clippy 0 warnings, `cargo test --workspace`
24 suites / 0 failures.

Core: `update_repo_authorized(store, name, threads, progress, cancel, allow_empty)` holds the
real implementation; `update_repo` keeps its exact signature and delegates with `false`. That
mattered — there are ~58 `update_repo` call sites, and a signature change would have touched
every one. New `UpdateError::WouldEmptyIndex { repo, entries }`, raised **before** anything is
marked missing, so a refused scan leaves the index exactly as it was (the walk found no files,
so nothing had been written yet either).

CLI: `dedup repo update --force`. Without it the command exits non-zero and the message names
`--force` as the way through.

GUI: the worker maps the refusal to a new `JobOutcome::UpdateWouldEmpty(entries)` rather than a
generic error string, so it is a *question*, not a failure. `empty_scan_modal` states the repo,
the entry count, and that nothing has changed yet; SCAN ANYWAY re-queues as
`JobKind::UpdateForced`, CANCEL dismisses. Detecting this by matching on error text was
deliberately avoided — the typed variant survives message edits.

**The guard immediately caught two existing tests that empty a repo on purpose**, which is the
change working as intended rather than a regression:
- `diff_ops::sync_deletes_when_marked_missing_in_a_and_updates_index` — removes A's only file;
  now scans through a new `update_emptying` helper.
- `sync_groups::an_empty_walk_is_flagged_and_disarms_mirror` — simulates the unmounted
  mountpoint. Rewritten to assert **both** halves: unauthorised is refused and leaves
  `["a.txt", "b.txt"]` intact, then authorised proceeds and still disarms the MIRROR.

Tests: the two above, two CLI tests (`assert_cmd`) covering refused-without-`--force` (with the
index still holding its entries afterwards) and accepted-with, and a GUI test that the refusal
becomes a pending confirmation and that declining leaves all five entries indexed.

This decision meets the ADR bar (hard to reverse — `--force` becomes a public CLI contract;
surprising without context; a real trade-off against the friction of legitimate emptying), but
`docs/adr/` still does not exist and creating it was out of scope here.

# 07 — Require authorisation before a scan marks every indexed entry missing

Status: ready-for-agent
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

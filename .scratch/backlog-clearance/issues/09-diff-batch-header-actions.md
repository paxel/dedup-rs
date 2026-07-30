# 09 — Bulk actions for the remaining DIFF rows

Status: ready-for-agent
Spec: ../spec.md

## Problem

Reported from real use: "in case we have thousands of files we should have buttons for doing
the actions for all remaining files? rename all left, rename all right?"

Every DIFF command acts on one row. A diff between two large repositories can list thousands
of rows that all want the same treatment, and there is no way to express that.

## Approach

Add bulk actions above the board that apply a command to **every row currently listed**, then
re-plan the diff so the board reflects the result.

Decisions to make and document:

- **"Remaining" means the rows currently on the board** — that is, after the active sort,
  after the show/hide-unchanged toggle, and excluding hidden rows. It does not mean every row
  the plan could ever produce. Hiding a row is how the user excludes it from a bulk action.
- **Only offer a bulk action where it is meaningful for the current mode.** BY PATH and BY
  HASH yield mutually exclusive relations, so the offered set differs; do not offer a bulk
  rename in a mode that never produces a rename.
- **A bulk action is destructive and must confirm**, stating the exact count and what will
  happen. Reuse the existing confirmation modals rather than inventing a new one.
- **Run off the UI thread**, through the same worker path single-row actions already use, with
  the existing cancel honoured. A thousand file operations must not block the frame.
- **Partial failure is reported, not swallowed**: if some operations fail, say how many
  succeeded and how many did not.

## Seam and tests

GUI seam — inline `ui_tests` in the Transfer view, prior art its existing DIFF board tests
including the end-to-end copy test that lands a real file on disk:

- a bulk action applies to every listed row and the board re-plans to reflect it
- a hidden row is **not** touched by a bulk action
- the confirmation states the correct count, and declining changes nothing on disk
- a mode that cannot produce a relation does not offer its bulk action
- at least one bulk action asserted end-to-end against real files in a temp repository

Follow the existing pattern of waiting for the worker to finish rather than polling a fixed
budget — a previous fixed-budget poll was a known flake.

## Done

Standing gate green. `CHANGELOG.md`, `ai/improvements.md`, and the GUI documentation page for
the Transfer tab updated.

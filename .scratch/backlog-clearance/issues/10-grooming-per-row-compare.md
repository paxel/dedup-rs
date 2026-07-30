# 10 — Compare button on Grooming preview rows

Status: ready-for-agent
Spec: ../spec.md

## Problem

A Grooming preview row offers only apply and hide. There is no way to look at what a plan is
about to delete before running it — the user must trust the plan or abandon the preview and go
find the files elsewhere. DIFF rows already carry a compare command; planned surfaces do not.

This was deferred once already, as the follow-on to the compare unification.

Verified: the Grooming view builds its rows with only the apply and hide commands.

## Approach

Offer a compare command on Grooming preview rows, opening the same comparison surface DIFF
uses.

- **Not every row can compare.** A row with only one side has nothing to compare against. Offer
  the command only where a comparison is possible, exactly as the board already gates
  side-specific commands — a row that cannot compare must not show a disabled button.
- **PURGE rows are the deliberate exception** noted in the original backlog: a purge row has no
  counterpart, so no compare command.
- The command acts on the row as a whole, so it is centred in the command grid, alongside the
  existing row-level commands.

**Ordering note:** ticket 11 replaces the comparison surface DIFF opens. This ticket should
call whatever entry point DIFF calls, so that ticket 11 improves both at once. If 11 has
already landed when this is implemented, use the shared lightbox directly.

## Seam and tests

GUI seam — inline `ui_tests` in the Grooming view, prior art its existing board tests and
`doc_screenshot_groom_purge_board`:

- a two-sided planned row offers compare, and activating it opens the comparison
- a one-sided row does not offer it
- a PURGE row does not offer it
- **geometric assertion**: the added command does not push a row's other commands outside the
  centre region, and row height still matches what the row model measured

Render check: re-render the Grooming board screenshot and look at it.

## Done

Standing gate green. `CHANGELOG.md`, `ai/improvements.md`, and the GUI documentation page for
the Grooming tab updated.

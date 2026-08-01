# 10 — Compare button on Grooming preview rows

Status: resolved
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

## Comments

**Partially implemented 2026-07-31 — the compare button is blocked on ticket `11`.** Status
left at `needs-triage` rather than `resolved`, because the headline deliverable is not there.

**Delivered: the counterpart the rows were missing.** Investigating turned up that
DEDUPE's preview already had the data and was throwing it away — `diff_print` returns
`DiffItem::Equal { rel_path, reference_path }` and the builder matched `{ rel_path, .. }`. So a
DEDUPE row now shows the surviving copy on the right, with the pool repos named in the right
header. That is most of the value the ticket was after: seeing *what makes this file
redundant* before deleting it, instead of trusting a bare list. PURGE and PRUNE genuinely have
no counterpart and keep their one-sided shape (`GroomingView::one_sided`), which also satisfies
this ticket's rule that PURGE is the deliberate exception.

**Not delivered: `Cmd::Compare` on the row.** The ticket says to "call whatever entry point DIFF
calls". There isn't one that Grooming can reach: DIFF's compare is `DiffCompare`, a private type
inside `transfer_view`, driven by that view's own state — and ticket `11` deletes it outright
and routes DIFF through the shared lightbox. Wiring a button to a surface that is about to be
removed would be work thrown away, and offering a button that opens nothing is worse than not
offering it.

Two findings the ticket did not anticipate, worth carrying into `11`:

1. **ORGANIZE rows are not compare candidates.** They are two-sided, but both sides are the
   *same file* at its old and new path — comparing it with itself is meaningless. Only DEDUPE
   has a genuine A-vs-B pair.
2. **`DeletedInReference` rows stay one-sided.** The reference knows the content but no longer
   holds it, so there is no live file to compare against.

Gate green as it stands: fmt clean, clippy 0 warnings, `cargo test --workspace` 24 suites / 0
failures, plus `a_dedupe_row_shows_the_copy_that_makes_it_redundant`.

---

## Comments (completed)

**Resolved 2026-07-31.** The block was ticket `11`'s doing, and it cleared the moment
`DiffCompare` moved out of `transfer_view` into the shared `compare_view` module.

DEDUPE rows now offer `Cmd::Compare`, which opens that shared surface with the doomed file on
the left and the surviving pool copy on the right. Finding the right side needs one lookup the
preview cannot do up front: `diff_print` returns the reference *path* but not which pool repo
holds it, so the click resolves it by asking each pool repo in turn — cheap, because it happens
once per click rather than once per row.

Gating is as this ticket required, and follows from the row model rather than a special case:
a row offers COMPARE only when it has a counterpart, so PURGE and PRUNE (one-sided) do not.
ORGANIZE is excluded for the reason recorded earlier — its two sides are the same file at its
old and new path.

Actions stayed caller-supplied: any pick in the viewer just closes it, because Grooming's own
APPLY / HIDE are how a row is acted on.

Gate green: fmt clean, clippy 0 warnings, `cargo test --workspace` 24 suites / 0 failures, plus
`only_rows_with_a_counterpart_offer_compare`.

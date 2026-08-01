# 04 — Evidence rows in Duplicates + tiered delete-safety

Status: ready-for-agent
Spec: ../spec.md
Blocked by: 01

## Problem

Coverage — which archives are redundant — is a separate CLI report, disconnected from
the Duplicates flow where deletion decisions actually happen. And the tool's
delete-safety (do not delete the last copy of content) does not know that a "copy"
inside an archive is a weaker thing than a loose file.

**Decided in session:** surface a member's redundancy inline as a read-only
**evidence row** in the Duplicates grid, and make delete-safety **tiered** — warning
(not blocking) when the only remaining copy would be inside an archive, and guarding
the circular double-delete.

## Approach

- **Evidence rows:** when a loose file's content (size + BLAKE3) also matches a member
  recorded in the archive index, show a read-only row in that duplicate group:
  `backup_2019.zip › photo.jpg — in archive`. It carries no KEEP/DELETE affordance;
  only members that match a loose file appear (not the whole member list).
- **Tiered safety, two rules, both warn-not-block** (consistent with confirm-don't-forbid):
  1. Deleting a loose file whose only other copy is an archive member → warn: "this
     content will survive only inside `<zip>` (needs extraction / a password to read)."
  2. A single triage pass that would delete both a loose file and the archive that is
     its only backup → stop-until-confirmed as a pair. This dissolves the circularity
     where coverage calls the zip redundant *because* the file is loose, while
     delete-safety calls the loose file safe *because* it is in the zip.
- Locked members never produce evidence rows (their content is unknown).

## Seam and tests

Core seam for the matching/safety logic; GUI seam (`egui_kittest`) for the rows:

- a loose file duplicated by a member shows an evidence row naming the archive
- the evidence row offers no delete action
- deleting a loose file whose only other copy is archived raises the tier warning
- marking both a loose file and its only-backup archive in one pass triggers the
  paired confirmation
- a loose file with another *loose* copy is unaffected (no tier warning)

## Done

Standing gate green. `CHANGELOG.md`: archive redundancy now shows inline in Duplicates,
and delete-safety understands the loose-vs-archived tier. GUI docs for Duplicates.

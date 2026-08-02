# 05 — Extract into a repo, then index

Status: resolved
Spec: ../spec.md
Blocked by: 03

## Problem

There is no way to get files out of an archive. For a recovery tool that is the whole
point once you have decided a zip is worth keeping the contents of.

**Decided in session:** extract the whole archive or selected members **into a repo,
then index** them, so recovered content re-enters triage. Offer a lighter
"extract to a plain folder, no indexing" path too. The source archive is never
touched.

## Approach

- From the Archive representation (ticket 03), offer EXTRACT — whole archive or a
  selection of members.
- **Primary path:** the destination is a folder under a known repo (or a new repo on a
  folder the user picks); write the members there, then let the normal scan index them
  so they become loose, hashed, triageable content. Because indexing is scan-integrated
  (ticket 01), extraction just needs to write files and trigger/queue a scan of the
  destination.
- **Lighter path:** extract to any plain folder and stop — no indexing. Re-enters the
  model only if the user later points a repo at that folder.
- Name collisions on extract must not overwrite existing files (mirror the
  never-lose-data rule the image-save copy path uses).
- Encrypted members require the archive unlocked first (ticket 06); until then extract
  is offered only for readable members.
- Never write to the source archive.

## Seam and tests

Core seam for the extract-and-write logic; GUI seam for the flow:

- extracting a member into a repo writes the bytes and, after a scan, the content is
  indexed as loose (assert the index gains the member's content key)
- extracting to a plain folder writes bytes and does **not** index
- a name collision does not overwrite an existing file
- the source archive is byte-identical after extraction
- extracting a whole archive writes every readable member

## Done

Standing gate green. `CHANGELOG.md`: you can extract from an archive into a repo (and
have it indexed) or to a plain folder. GUI docs for the Archive representation.

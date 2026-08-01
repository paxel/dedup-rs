# 02 — Locked archives index shallowly instead of vanishing

Status: ready-for-agent
Spec: ../spec.md
Blocked by: 01

## Problem

Today an encrypted member throws inside `zip_members`, is caught, and skipped — so a
password-protected zip produces an empty or partial member list and no coverage. The
archive effectively disappears from the model.

**Decided in session:** a locked archive indexes **shallowly**. A zip's central
directory carries member names and sizes unencrypted even when the contents are
encrypted, so record those, mark the archive and its members **LOCKED** (contents
unknown, no content hash), and leave a hook for filling in the real content identity
once the archive is unlocked (ticket 06).

## Approach

- Extend member reading to detect encryption per member (the `zip` crate exposes
  whether an entry is encrypted) rather than silently skipping.
- Record a locked member as name + size + a LOCKED marker with no content hash. The
  `ArchiveMember` store type and its coverage matching must tolerate "no hash yet"
  (a locked member never matches loose content — it cannot, until unlocked).
- Mark the archive itself as locked (has ≥1 locked member) so the GUI/CLI can show it
  as LOCKED and later offer unlock.
- Central-directory-encrypted archives (names also hidden) cannot be listed at all —
  record them as a fully-opaque locked archive with an unknown member set.

## Seam and tests

Core seam — `crates/dedup-core/tests/`:

- an AES-encrypted zip is indexed as LOCKED with its member **names and sizes**
  present and no content hashes (build the fixture with the `zip` crate's writer +
  a password)
- a locked member never counts toward coverage (its content is unknown)
- a mixed archive (some members encrypted, some not) records real hashes for the
  readable members and LOCKED for the rest
- the source archive is byte-identical after indexing

## Done

Standing gate green. No GUI yet. `CHANGELOG.md`: encrypted archives are now seen
(as locked) rather than silently dropped.

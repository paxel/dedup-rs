# 06 — Supplied-password unlock, working-password persistence, session spray-book

Status: ready-for-human
Spec: ../spec.md
Blocked by: 02

**Security-sensitive: crypto handling and secrets-at-rest. A human should review the
design (ADR-0002) before an AFK agent builds it.**

## Problem

A locked archive is a total blind spot even when the user knows the password — the
`zip` 8.6 crate can decrypt (AES/PBKDF2/HMAC are compiled in), but nothing calls it.

**Decided in session:** supplying a password unlocks everything (browse, extract,
coverage) for that archive. A confirmed **working password** is persisted with the
archive's index entry, **encrypted behind an app passphrase** entered once per
session, so an archive is never re-cracked. A session **spray-book** of passwords is
auto-tried against other locked archives and never written to disk. See ADR-0002.

## Approach

- **Unlock:** on a LOCKED archive, accept a password, decrypt-verify it (the WinZip-AES
  2-byte verifier / a successful member open), and on success hash the archive's member
  contents into the index — filling in the content identity that was unknown at scan
  time (ticket 02), so its evidence rows (ticket 04) appear and extract (ticket 05)
  works.
- **Working password (persisted, encrypted):** store the confirmed password with the
  archive's index entry, encrypted under a key derived from an app passphrase the user
  enters once per session. A stolen index DB must not yield plaintext passwords.
  Reopening an archive in a later session decrypts its stored password (after the
  passphrase) and unlocks without re-prompting.
- **Spray-book (session-only):** hold entered/recovered passwords in memory for the
  session; when a new locked archive is encountered, auto-try the spray-book before
  prompting. Never persist the spray-book. The spray-book also seeds recovery
  (ticket 07).
- Scope: local archives the user possesses and is entitled to. No mass/bulk behaviour.

## Seam and tests

Core seam for unlock + the encrypted-at-rest store; GUI seam for the passphrase/unlock
flow. Security assertions are the point here:

- supplying the correct password unlocks browse/extract/coverage; the wrong one does not
- a persisted working password is unreadable from the store without the app passphrase
  (assert the stored bytes are not the plaintext)
- the spray-book never reaches disk (assert nothing password-shaped is written)
- one shared password entered on archive A auto-unlocks archive B via the spray-book
- the source archive is byte-identical after unlock

## Done

Standing gate green, **with the security assertions passing**. `CHANGELOG.md`:
password-protected archives can be unlocked; passwords are remembered encrypted, never
in the clear. `README.md` for the app-passphrase concept. GUI docs. ADR-0002 referenced.

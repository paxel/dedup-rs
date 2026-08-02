# 07 — Built-in recovery: wordlist + masks, and the ZipCrypto known-plaintext shortcut

Status: resolved
Spec: ../spec.md
Blocked by: 06

**Security-sensitive and dual-use: password recovery. A human should review scope and
framing before an AFK agent builds it. Scoped to local archives the user possesses and
is entitled to (inherited estates, own backups) — not mass targeting.**

## Problem

Many inherited archives have a weak, human-chosen password, or use legacy ZipCrypto
that is cryptographically broken. These should open without dragging in external
tooling — but the tool must be honest that strong AES will not fall to a built-in
CPU attempt.

**Decided in session:** a modest built-in RECOVER for the easy wins — a wordlist plus
simple masks against weak passwords, and the ZipCrypto known-plaintext shortcut — with
an honest ceiling. Anything serious hands off to hashcat (ticket 08).

## Approach

- **Weak-password attempt:** try, in order, the session spray-book (ticket 06), a
  bundled/small user-supplied wordlist, and simple mask/rule expansions (case, digit
  suffixes, common substitutions). Each candidate is verified cheaply against the
  archive (the WinZip-AES 2-byte verifier). Report progress and let the user cancel.
  Be explicit in the UI that this is CPU-bound (PBKDF2) and will not exhaust a strong
  password's space — it is for weak passwords only.
- **ZipCrypto known-plaintext shortcut:** for legacy-encrypted (PKZIP/ZipCrypto)
  archives, a member often gives ~12 bytes of known plaintext for free (a known file
  type's magic bytes, or another member's known header). Recover the internal keys
  from that (bkcrack-style) and decrypt without guessing the password at all.
- On success, feed the recovered password into the unlock + working-password path
  (ticket 06) so it persists (encrypted) and joins the spray-book.
- No GPU, no exhaustive AES search. The UI must not imply strength the attempt does
  not have.

## Seam and tests

Core seam for the recovery engine:

- a weak-password AES zip whose password is in the wordlist is recovered and unlocked
- a strong-password AES zip is reported as not-recovered (bounded attempt, honest give-up)
- a ZipCrypto zip with a member of known plaintext is decrypted via the known-plaintext
  shortcut (fixture built with a legacy-encrypted zip)
- a recovered password persists via ticket 06 (encrypted) and joins the spray-book
- the source archive is byte-identical after recovery

## Done

Standing gate green. `CHANGELOG.md`: weak-password and legacy-encrypted archives can be
recovered in-tool, with an honest ceiling. GUI docs, stating the ceiling plainly.

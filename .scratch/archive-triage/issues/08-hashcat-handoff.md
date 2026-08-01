# 08 — Hashcat handoff for serious recovery

Status: ready-for-human
Spec: ../spec.md
Blocked by: 06

**Security-sensitive and dual-use: drives an external password cracker. A human should
review scope and framing before an AFK agent builds it. Scoped to local archives the
user possesses and is entitled to — not mass targeting.**

## Problem

The built-in attempt (ticket 07) is honestly capped: a strong AES password needs
GPU-scale cracking, which is hashcat's / John's job. Re-implementing that in a
single-binary tool is a losing race — the same reasoning that makes ffmpeg an optional
external tool here.

**Decided in session:** export the archive's hash in the format hashcat expects and
hand off; ingest the found password back into unlock. Graceful when hashcat is absent
(the feature is simply unavailable, like ffmpeg-dependent features).

## Approach

- **Export the hash:** from a locked archive, produce the `$zip2$…` (WinZip-AES,
  hashcat mode 13600) or `$pkzip2$…` (legacy PKZIP) string. Offer it two ways: copy to
  clipboard / write to a file for the user to run hashcat themselves, and — if hashcat
  is found on PATH — an in-tool "launch hashcat" that runs it against a user-chosen
  wordlist/mask and streams progress.
- **Ingest:** when hashcat (or the user, pasting back a found password) yields the
  password, feed it into the unlock + working-password path (ticket 06) so it persists
  encrypted and joins the spray-book.
- **Graceful absence:** with no hashcat on PATH, the launch option is hidden and only
  the export-the-hash path remains (the user runs hashcat elsewhere). Mirror how the
  tool degrades without ffmpeg.
- Never write to the source archive.

## Seam and tests

Core seam for the hash export (deterministic, unit-testable); GUI/integration for the
handoff:

- the exported hash for a known AES zip matches the expected `$zip2$` shape (assert
  against a fixture; the found password is verifiable by unlock)
- the exported hash for a legacy zip is the `$pkzip2$` shape
- with hashcat absent, only the export path is offered (no launch); the tool does not error
- a password fed back in unlocks and persists via ticket 06
- the source archive is byte-identical throughout

## Done

Standing gate green. `CHANGELOG.md`: serious recovery via hashcat handoff (export the
hash, or launch hashcat when present). `README.md` noting hashcat as an optional
external tool alongside ffmpeg. GUI docs.

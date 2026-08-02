# Archive triage: see into, pull from, and unlock inherited archives

Status: resolved

Spec produced by a grilling + domain-modeling session on 2026-08-02, driven by the
user noticing that a zip on an inherited disk is a black box. Every decision below
was put to the user and chosen by them; the rationale recorded is the reason given
at the time. Facts were verified against source. Glossary in `/CONTEXT.md`; the two
load-bearing decisions have ADRs (`docs/adr/0001`, `docs/adr/0002`).

## Problem Statement

An inherited disk is full of archives, and today the tool treats them as opaque.

What exists: `dedup-core/src/archive.rs` opens zip/tar/tar.gz one level deep, hashes
each member, and computes **coverage** — what fraction of an archive already exists
as loose content elsewhere, so a fully-redundant `backup_2019.zip` reads as "safe to
delete." But this is **CLI-only** (`dedup archive index`, `dedup archive coverage`),
**report-only** (never opens a member, never extracts), it runs as a **separate
opt-in bulk pass** that re-reads every archive, and it **silently skips encrypted
archives** — an encrypted member throws, is caught, and dropped.

So the biggest redundancy class on an old disk is half-handled and invisible in the
GUI, you cannot look inside a zip, you cannot pull anything out, and a
password-protected archive is a total blind spot — even though the `zip` 8.6 crate
already compiles in AES/PBKDF2/HMAC and can decrypt with a known password; the tool
just never calls it.

The archive is stuck as *an opaque redundancy candidate*. It should be a container
you can **see into**, **pull from**, and **unlock**.

## Solution

Promote the archive to a first-class, **read-only** container.

Its members are content: once readable they are hashed like any loose file, they
surface as read-only **evidence rows** in Duplicates when they duplicate a loose
file, and they open in the shared viewer by type (image, audio, text). The archive
as a whole stays the unit of deletion — the tool never rewrites a zip. Reading an
archive folds into the normal incremental scan (a changed archive is opened once, an
unchanged one skipped), replacing the opt-in bulk pass. A locked archive indexes
shallowly (member names and sizes, marked LOCKED) until it is unlocked, at which
point its contents are hashed and its evidence rows fill in.

Extraction is recovery into triage: pull the whole archive or selected members into
a repo and index them, so inherited content re-enters dedup. Unlocking is layered
and honest: a supplied password always works; a built-in attempt handles the easy
wins (a wordlist and simple masks against weak passwords, plus the ZipCrypto
known-plaintext shortcut); anything serious exports a hash for hashcat. A confirmed
working password is remembered (persisted, encrypted) so an archive is never
re-cracked; the session spray-book of candidate passwords is never written to disk.

## User Stories

1. As someone triaging an inherited disk, I want a changed archive read as part of the normal scan, so that I never run a separate expensive pass and unchanged archives are never re-read.
2. As someone triaging, I want an archive that is entirely redundant to be flagged safe to delete, so that I can clear backup zips I already have loose.
3. As someone judging a loose file, I want to see when its content also lives inside a named archive, so that I can decide whether the loose copy or the archive is the one to keep.
4. As someone who cannot delete the last copy of content, I want a warning when my only remaining copy would sit inside an archive, so that I do not orphan content behind a lock.
5. As someone triaging, I want to be stopped-until-I-confirm before deleting both a loose file and the archive that is its only backup in one pass, so that a pair of individually-safe deletes cannot combine into a loss.
6. As someone who inherited a zip, I want to look inside it, so that I can see what is there before deciding anything.
7. As someone looking inside a zip, I want a member to open in the same viewer as everything else — an image as an image, audio as audio — so that I inspect it the way I inspect loose files.
8. As someone who wants my files back, I want to extract the whole archive, or just some members, into a repository and have them indexed, so that recovered content immediately joins triage.
9. As someone triaging, I want the archive I extracted from left exactly as it was, so that I never risk the original.
10. As someone with a password-protected inherited zip, I want to type the password and have everything — browse, extract, coverage — work, so that a known password is all it takes.
11. As someone who does not know the password, I want the tool to try the easy possibilities, so that a weak or legacy-encrypted archive opens without external tooling.
12. As someone facing a strongly-encrypted archive, I want to export its hash for hashcat, so that I can bring real cracking power to bear and have the found password flow back in.
13. As someone triaging a family's disks, I want a password I have already found tried automatically against every other locked archive, so that one reused password opens the whole set.
14. As someone triaging over several days, I want an archive I have already unlocked to stay unlocked without re-cracking, so that I never repeat work — but I do not want my archive passwords sitting in a file in the clear.
15. As someone handling sensitive inherited data, I want candidate password guesses to never touch disk, so that closing the app leaves no trail of them.

## Implementation Decisions

**Decided in session, in order:**

1. **Full ambition: browse + extract + unlock.** Not just surfacing the existing report — the archive becomes a container you see into, pull from, and unlock.
2. **Archives are read-only.** The tool never writes to a zip. The whole archive is the unit of deletion; per-member deletion (repack) is out, permanently. See ADR-0001. Manipulation of archive contents is explicitly deferred indefinitely.
3. **A member is content, first-class for seeing and comparing** — hashed like loose content, comparable in the viewer — but never marked or deleted individually.
4. **Redundant members surface as read-only evidence rows** in the Duplicates grid (only members that match a loose file appear; not all members). Full member browsing lives in the shared viewer's Archive representation, not the grid — which also avoids exploding hundreds of thousands of member rows.
5. **Extract into a repo, then index.** Whole archive or selected members; the destination is a repo so recovered content re-enters triage. A lighter "extract to a plain folder, no indexing" path is offered alongside. The source archive is never touched.
6. **Unlock is layered.** (a) Supplied password always. (b) Built-in easy wins: a wordlist + simple masks against weak passwords, and the ZipCrypto known-plaintext shortcut where a member gives free plaintext. (c) Serious recovery: export the hash (`$zip2$` / `$pkzip2$`, hashcat modes) and hand off to hashcat if present. No built-in GPU cracker; honest that strong AES will not fall; graceful when hashcat is absent. Scoped as forensic recovery on archives the user possesses and is entitled to.
7. **Two password memories, persisted differently.** The candidate **spray-book** is session-only, auto-tried across locked archives, seeds the wordlist, never written to disk. The confirmed **working password** of an unlocked archive is persisted with its index entry, encrypted behind an app passphrase entered once per session, so an archive is never re-cracked. See ADR-0002.
8. **Orphan safety is tiered and warns.** Loose and archived are different tiers. Delete-safety warns (never hard-blocks) when the only remaining copy would be inside an archive, and specifically guards the circular double-delete of a loose file and the archive that is its only backup. Warnings, not blocks — consistent with the tool's confirm-don't-forbid stance.
9. **Indexing is scan-integrated and incremental.** A changed archive (its file hash differs) is opened and its members read into the index once; an unchanged archive is skipped — the same change-detection loose files use. This replaces today's opt-in bulk `archive index` command. A locked archive indexes shallowly (member names and sizes, marked LOCKED) until unlocked.

**Interfaces:** an archive's readable members are content keyed by size + BLAKE3, matched against the same content index loose files use. The archive carries a lock state and, when unlocked, an encrypted working password. Extraction writes to a repo path and lets the normal scan index it.

## Testing Decisions

Core behaviour is tested with temp-repo integration tests (prior art:
`crates/dedup-core/tests/update_repo.rs`, `archive_test.rs`); GUI behaviour with
`egui_kittest` geometric tests plus an `--ignored` render test. Assert observable
state — index contents, coverage numbers, what a member opens as, what a delete
touches — not internal shape.

The security-sensitive behaviours get their own assertions and deserve human review
before an AFK agent builds them: a persisted working password is unreadable without
the app passphrase; the spray-book never reaches disk; the source archive is
byte-identical after any browse/extract/unlock.

## Out of Scope

- **Modifying archives in any way** — repack, strip a member, re-encrypt. Deferred indefinitely (ADR-0001).
- **A built-in GPU password cracker.** Serious recovery is hashcat's job; we export the hash and hand off.
- **Nested archives.** A zip inside a zip stays an opaque member, one level deep, as today. Recursing is a future depth knob.
- **Central-directory-encrypted archives** (the rare strong-encryption variant that hides member names too) are treated as fully opaque locked blobs — no member list until unlocked.
- **Persisting the candidate spray-book.** Session-only, by decision.
- **Bulk cracking / mass targeting.** This is local recovery on archives the user holds; nothing here targets archives at scale or that the user does not possess.

## Further Notes

- The existing opt-in CLI (`dedup archive index` / `coverage`) is superseded by scan-integrated indexing; the coverage *report* stays available (now fed by the incremental index) but `archive index` as a separate bulk read goes away.
- Member content-hashing during a scan reads each changed archive fully, so the change-detection skip on unchanged archives is what keeps repeat scans cheap — the same economics as loose-file hashing.
- The shared viewer (one-lightbox effort, landed 2026-08-01) is the natural home for the Archive representation and for opening a member by type; opening a member is an ephemeral decompress-to-temp fed to that viewer, distinct from durable extraction.

## Comments

**Implemented 2026-08-02, TDD, tickets 01–08 all `resolved`.** Standing gate green:
`cargo fmt --check` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean,
`cargo test --workspace` 24 suites / 0 failures.

**What landed:**

- **01** Archive member reading folds into `update_repo`, gated by the same change-detection
  as loose files; the opt-in bulk `dedup archive index` command and `index_repo_archives`
  are removed. `dedup archive coverage` stays, fed by the incremental index.
- **02** `ArchiveMember.hash` became `Option`; encrypted members index shallowly (name + size,
  `locked: true`, no hash) via `by_index_raw` instead of being dropped. Coverage gained a
  `locked` count and never calls a locked archive redundant.
- **03** The shared viewer gained an `Archive` representation (`RepresentationKind::Archive`,
  `FileFacts::is_archive`): a zip opens on its member list; clicking a member extracts it to a
  temp dir and shows it by type; BACK/Esc returns. Core `list_entries` / `extract_member`.
- **04** `archive::members_by_content` + `ArchiveOccurrence` power read-only evidence rows in
  the Duplicates grid, and a tiered delete-safety **warning** (not block) when a delete would
  leave content surviving only inside an archive.
- **05** `archive::extract_all` (collision-safe, path-sanitised, source untouched) + a GUI
  EXTRACT ALL / per-member extract via an rfd folder picker; extract into a repo then scan to
  index — proven at the core level.
- **06** `verify_password` + `unlock_and_reindex` (re-hash contents once unlocked); a new
  `secret` module (Argon2 + XChaCha20-Poly1305) and `ARCHIVE_PASSWORDS` store table for
  working passwords encrypted at rest; GUI supplied-password UNLOCK holding the password
  in-session so locked members open and extract.
- **07** `recover_password` (session candidates first, then wordlist + simple mutations) +
  `builtin_wordlist`; a GUI RECOVER button running a background attempt. Honest ceiling —
  strong passwords do not fall.
- **08** `export_hashcat_hash` produces the WinZip-AES `$zip2$` (mode 13600) string by parsing
  the local header's AES extra + the salt/verifier/data/auth blob; a GUI EXPORT HASH button
  copies it to the clipboard. `hashcat_available` helper for the (future) launch affordance.

**Deliberately scoped down — flagged for the human review these security tickets called for:**

1. **ZipCrypto known-plaintext shortcut (bkcrack-style)** — not implemented. Recovery covers
   the supplied-password path, the wordlist/mutation attempt (AES + ZipCrypto alike via
   `verify_password`), and the hashcat handoff. The keyless known-plaintext attack on legacy
   ZipCrypto is the remaining piece.
2. **`$pkzip2$` export** — only the WinZip-AES `$zip2$` format is produced; legacy ZipCrypto
   hash export returns `None`.
3. **The app-passphrase gate + persist-on-unlock wiring** — the crypto and the store table are
   done and tested (`secret::encrypt/decrypt`, `set/get_archive_password`), but the GUI flow
   that prompts for the app passphrase once per session and calls `unlock_and_reindex` +
   persists the working password on a successful unlock (through a caller with store access,
   like the `Edited`/refresh pattern) is not wired end-to-end. The viewer holds the verified
   password in-session for browse/extract.
4. **Cross-archive spray-book auto-try** — `recover_password` accepts an `extra` candidate list
   (the spray-book seam), but the app-level session set auto-tried across every locked archive
   is not held at the app level yet.
5. **hashcat launch** — export-to-clipboard is wired; driving the hashcat process and ingesting
   the found password is not (the `hashcat_available` probe exists for it).

The `$zip2$` export's exact acceptance by hashcat is pinned only structurally (field shapes and
lengths) — verifying a real hashcat run against a known password is the human step.

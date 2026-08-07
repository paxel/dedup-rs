# dedup-rs

A forensic data-inheritance triage tool: it helps someone make sense of a pile
of inherited or accumulated storage — deciding what is redundant, what is
unique, and what is safe to delete — where **content identity (size + BLAKE3)
is everything and paths never matter**.

This glossary was seeded from the archive-triage design (see
`.scratch/archive-triage/spec.md`) and grows as terms are resolved.

## Language

**Loose content**:
Content that exists as a plain file directly in a repository, as opposed to
inside an archive. The counterpart to a member.
_Avoid_: file (ambiguous — a member is also a file), plain file.

**Archive**:
A read-only container file (zip, tar, tar.gz) that holds members. The tool
never writes to an archive. The whole archive is the unit of deletion.
_Avoid_: zip (only one of the kinds), container.

**Member**:
A file inside an archive. It is content — viewable and comparable like loose
content once readable — but it is never marked or deleted individually.
_Avoid_: entry, archived file.

**Tier**:
The distinction between loose content and archived content, which matters for
delete-safety: an archived copy is a weaker tier because reading it needs
unlocking and extraction, and it may sit inside an archive that is itself about
to be deleted.

**Coverage**:
The fraction of an archive's members whose content is present elsewhere as loose
content. Full coverage means the archive is redundant and the whole thing is
safe to delete.
_Avoid_: redundancy score, overlap.

**Evidence row**:
A read-only row in the Duplicates view showing that a loose file's content also
lives inside a named archive. It informs a keep-or-delete decision and carries
no action of its own.

**Locked archive**:
An encrypted archive whose member names and sizes are known (they live
unencrypted in the archive's directory) but whose member contents cannot be
hashed or read until it is unlocked.
_Avoid_: encrypted archive (use for the property; "locked" for the state).

**Unlock**:
To make a locked archive's contents readable by supplying or recovering its
password.

**Working password**:
The confirmed password that unlocks a specific archive. It is persisted with
that archive's index entry, encrypted, so the archive is never re-cracked.
_Avoid_: saved password.

**Spray-book**:
The session-only set of candidate passwords the tool auto-tries against locked
archives and seeds into a recovery attempt. It is never written to disk.
_Avoid_: password list, wordlist (a wordlist is an external input to recovery;
the spray-book is the in-session set of already-seen passwords).

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


**Long-running operation**:
Work the user starts that may take longer than a moment — a repository scan
or check, a Duplicates FIND or APPLY MARKED, a Transfer or Grooming PREVIEW or
APPLY ALL, a GROUP SYNC, a DIFF PREVIEW. It runs alone behind the activity
modal.
_Avoid_: job, task, background operation (nothing long-running runs in the
background any more).

**Row action**:
A single, quick change the user makes from a result — a board's DELETE, COPY,
RENAME, OVERWRITE, a group's DELETE NOW, an accept — answered by a
notification card, never by a modal.
_Avoid_: quick action, inline action.

**Activity modal**:
The one blocking window in which a long-running operation shows its title,
repositories, phase, progress, problems and CANCEL, and which becomes the run
report when the operation ends.
_Avoid_: progress dialog, busy overlay.

**Notification card**:
The animated card in the top-right stack that reports a row action: what
happened, to which file, in which repository, with what result.
_Avoid_: toast, status line.

**Event log**:
The persistent, append-only record of every change the app made to the
filesystem, kept under the configuration directory and opened from the
notification area. Marks, scans and searches are not in it.
_Avoid_: history, audit trail, session log (the session log is the diagnostic
log file, not this).

**Start page**:
The one page the app opens on: the repo cards, the command list, the
favorites and the totals. It replaces the tab bar.
_Avoid_: dashboard, home tab, repositories tab.

**Repo card**:
A repository's card on the start page: identicon, name, path, counts, last
update age, lock, main badge. Cards are multi-select and together form the
selection.
_Avoid_: repo chip (the chip is the compact selector widget inside a view),
tile.

**Selection**:
The set of repo cards currently selected on the start page. It is the only
place repositories are chosen; every command reads its repositories from it.
_Avoid_: picked repos, included repos.

**Command**:
One operation the app offers, listed on the start page by intent. A command is
enabled or greyed by the selection, reveals its remaining inputs once chosen,
and runs in place.
_Avoid_: tab, tool, mode (a mode is an input of a command, such as exact or
similar).

**Header strip**:
The one-line summary a running command folds the start page into: selection,
command, inputs. Clicking it returns to the start page.
_Avoid_: breadcrumb, summary bar.

**Board**:
The one result surface: a table of file rows with thumbnail, name,
repository, size, planned action and status. Every command's result is a
board except a duplicates find, whose groups show their copies side by side as
cards; a click on a row opens the viewer.
_Avoid_: review board, grid, listing, preview table.

**Favorite**:
A fully specified command (selection, command, inputs) saved under a name on
the start page, reopened in one click. It saves the specification, never the
confirmation: it still lands on the preview.
_Avoid_: preset, saved action, task.

**Preview**:
The board filled with a writing command's proposed rows before anything on
disk changes. APPLY runs them.
_Avoid_: review, plan, dry run.

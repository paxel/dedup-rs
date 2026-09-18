# Every action reports, and long work runs alone

Two rules for the desktop app, decided together on 2026-09-18 after a release
whose DIFF board answered a DELETE with nothing but a redraw and whose FIND sat
on "0 %" for minutes over a large audio library.

**Feedback.** Every action the user takes owes three things: an acknowledgement
that the click registered, progress while it runs, and an outcome that says what
happened. Nothing may end in a silent redraw.

- An action expected to finish within about 300 ms is a **row action** (a
  board's DELETE, COPY, RENAME, OVERWRITE, KEEP ONE, a group's DELETE NOW, an
  accept or un-accept). It merges acknowledgement and outcome into one
  **notification card**: an animated card in the top-right stack naming the
  action, the file, the repository and the result.
- Anything longer is a **long-running operation**: a repository UPDATE or
  CHECK, a Duplicates FIND or DELETE MARKED, a Transfer RUN or GROUP SYNC, a
  Grooming RUN, a DIFF REVIEW. It runs behind the **activity modal**, one at a
  time, which blocks the rest of the app and shows what runs, on which
  repositories, the current phase, how far, elapsed time, ETA where known, and
  problems as they occur. CANCEL is its only control and cancels the whole
  batch. When the operation ends the same modal becomes the run report.
  Escape closes a finished report, never a running operation.
- Viewer work (decoding, thumbnails, spectrograms, waveforms, playback, frame
  extraction) is neither: it never touches an index and never takes the modal.
- Every change the app makes to the filesystem — delete, copy, move, rename,
  overwrite, an in-place rotation save, a tag rewrite, a directory purge — is
  appended to the **event log**, a persistent file the user can open from the
  notification area. Marks, ticks and other in-memory choices get a card but no
  log line: they change nothing on disk.

**One at a time.** A long-running operation cannot start while another one or
a row action is in flight, and a row action cannot start while the modal is
up. The refusal names what is still running.

**Layout.** Reading order is the workflow. Every tab asks, top to bottom and
left to right: WHAT (the mode or command), WITH WHICH (the repositories, as two
side-by-side panels where there is a source and a target), HOW (filter,
threshold, options), and only then shows the run button. A section appears only
once the section before it has an answer, and keeps its answer when an earlier
section changes unless that change makes the answer invalid. Controls that fit
side by side share a row. Selection controls and run buttons have two distinct
looks: a run button is the thing that opens the activity modal. Once a run
starts, the sections above the result collapse into a one-line summary that
reopens on click.

## Why

- The app is a triage tool that deletes files. A deletion the user cannot see
  happen is one they cannot verify, and a search that looks frozen is one they
  will kill — both happened, on a released version, in one afternoon.
- Two operations rewriting indexes at once is the way a MIRROR sync and a scan
  race each other. Blocking the app for the duration is the simplest rule that
  makes the race impossible, and it is also honest: while an index is being
  rewritten there is nothing else safe to do with it.
- Five tabs grew five layouts. A user who has learned one still has to hunt on
  the next. One reading order, revealed step by step, means the tab teaches
  itself and a fresh screen shows one decision, not twenty buttons.
- Monitors are wider than they are tall; stacking every control in a column
  wastes the dimension there is plenty of.

## Consequences

- One owner in the application root (`Activity`) starts or refuses
  long-running operations, draws the modal and the report, owns the
  notification stack and writes the event log. Views hand it their long work
  and lose their own threads for it; a view still receives its own result over
  its own channel.
- Core operations that a view runs behind the modal must report phases; a
  search or plan that reports nothing is a defect of the operation, not of the
  modal.
- Tabs are converted one per change, each fully — layout, modal, notifications
  — with Duplicates as the reference. A half-converted tab is not shipped.
- `docs/agents/ui-standard.md` is the checklist every new control and every
  converted tab is held to.
- Undo is out of scope: the event log records, it does not revert.

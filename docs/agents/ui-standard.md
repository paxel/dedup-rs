# UI standard checklist

The questions every new control, every new operation and every converted tab
must answer. The rules and their reasons are in
[ADR 0003](../adr/0003-every-action-reports-and-long-work-runs-alone.md);
this page is the checklist to run through before a change is done.

## For every action the user can take

- Which kind is it: a **row action** (finishes within ~300 ms), a
  **long-running operation**, or **viewer work**?
- Row action: does it produce a notification card in the same frame it
  finishes, naming the action, the file, the repository and the outcome? If it
  changed the filesystem, does it also write an event-log line?
- Long-running operation: does it start through `Activity`, with a title, the
  repositories involved and a progress source that reports phases with done
  and total? Does CANCEL stop the whole batch? Does the finished modal show a
  run report with counts and every individual failure?
- Does it refuse to start while something else is running, and does the
  refusal name what is running?
- Viewer work: does it stay off the modal and off the log?

## For every tab

- Is the order top-down then left-right: WHAT, WITH WHICH, HOW, RUN?
- Does each section appear only once the section before it has an answer?
- Does a section keep its answer when an earlier section changes, unless the
  change makes the answer invalid?
- Do siblings that fit side by side share a row?
- Are selection controls drawn as chips and run buttons drawn in the run look,
  never mixed?
- After RUN, do the selection sections collapse into one summary line that
  reopens on click?
- Do the tab's shortcuts stay silent while the activity modal is up?

## For every core operation a view runs behind the modal

- Does it take a progress callback and a cancellation token?
- Does it report each phase with done and total, so the modal can show a real
  percentage?
- Is there a core test with a recording progress implementation asserting the
  phases arrive in order and reach their totals?

## Proof

- App harness: modal on screen with its phase line; second start refused;
  CANCEL fires the token and the modal becomes a cancelled report; a row action
  puts a card on screen and a line in the event log under the test's
  configuration directory.
- Tab harness: geometric assertions for the section order and the reveal
  rules, not label presence.
- Doc screenshots regenerated and looked at.

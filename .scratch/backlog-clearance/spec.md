# Backlog clearance — verified batch of 11

Status: ready-for-agent

Spec produced by a grilling session on 2026-07-30/31. Every decision below was put to the
user and chosen by them; the rationale recorded here is the reason given at the time, not a
reconstruction. Facts were verified against source, not against the previous docs.

## Problem Statement

Someone triaging an inherited pile of drives uses dedup-rs across three surfaces — Duplicates,
Grooming and Transfer — and expects them to behave like one application. Today they do not:

- **Comparing two audio files works in the Duplicates tab and silently fails everywhere else.**
  Opening COMPARE on a Transfer DIFF row for two MP3s shows `no preview for audio/mpeg`. The
  same is true for documents and for any file whose interesting content is its metadata or its
  text. The Duplicates lightbox grew tabbed representations (Overview / Metadata / Text /
  Image / Audio / Video), and DIFF cannot reach any of them.
- **The backlog documents claimed work was finished that was not.** `ai/roadmap.md` recorded the
  "Unified lightbox & compare" epic as complete on the grounds that `diff_inspect.rs` had been
  deleted. The file was deleted; its logic was re-created as a private compare type inside the
  Transfer view, which decodes only images and video frames. Meanwhile several items still
  carrying an open checkbox — the DELETE A / DELETE B mark pills, protected-repo strikethrough,
  per-side repo badges — had in fact been implemented. Anyone planning from those documents was
  planning against fiction in both directions.
- **Real complaints from real use went unfixed.** Playback does not remember that it was paused
  when stepping to the next file. Filters cannot express "everything that is *not* an MP3", nor
  match case-insensitively. Repository counts do not refresh when returning to the Repositories
  tab after a deletion. A DIFF listing thousands of differing names offers no way to act on all
  of them, and no indication of *which characters* in two near-identical names differ.
- **A scan of an unmounted drive quietly destroys the index.** A walk that finds zero files where
  the index held entries marks every entry missing. It logs a warning and shows a notification,
  but nothing stops it — and if that repo is a sync group's main, the next MIRROR push propagates
  the emptiness to every sink.

## Solution

One ordered batch of eleven tickets, run unattended overnight by a single agent, that closes the
verified gaps and removes the duplicated compare surface. Ordering is deliberate: the small,
isolated changes run first so they are banked, and the one large architectural rework runs last
so that a failure there cannot contaminate the ten tickets ahead of it.

After this batch: any two files that offer the same representation can be compared from any
surface; filters can express negation and case-sensitivity; playback, repository counts and DIFF
bulk actions behave the way the reporter expected; and a scan cannot empty an index without an
explicit confirmation or `--force`.

## User Stories

1. As someone triaging inherited drives, I want to compare two MP3s from a Transfer DIFF row, so that I can decide which copy to keep without switching to the Duplicates tab.
2. As someone triaging inherited drives, I want to see a spectrogram for both sides when comparing audio anywhere, so that I can spot a re-encode or a truncated file visually.
3. As someone comparing two audio files, I want DELETE A and DELETE B pills in the compare header itself, so that I can mark a copy without leaving the comparison.
4. As someone comparing two documents from a DIFF row, I want the Text representation, so that I can see how their contents differ rather than being told there is no preview.
5. As someone comparing two tagged audio files from a DIFF row, I want the Metadata representation, so that I can compare ID3 fields side by side.
6. As someone stepping through several similar audio files, I want playback to stay paused when I move to the next one, so that the application does not start playing music I deliberately stopped.
7. As someone stepping through more than two audio files in the lightbox, I want the switcher to keep working after I press Compare, so that I can reach the third and fourth file.
8. As someone who edited an ID3 tag on one of four similar files, I want that edit to appear on that file only, so that I can trust what the tag editor shows me.
9. As someone filtering a large repository, I want to express "everything that is not an MP3", so that I can exclude a format instead of enumerating every format I want.
10. As someone filtering a repository, I want to choose whether matching is case-sensitive, so that `*.JPG` and `*.jpg` behave the way I intend on a drive that mixes conventions.
11. As someone who has just deleted duplicates, I want the Repositories tab to show updated counts when I return to it, so that I can confirm the deletion took effect without a manual reload.
12. As someone reviewing a DIFF between two repositories, I want the differing characters in two near-identical filenames highlighted, so that I can see at a glance whether the difference is a suffix, a counter, or a different extension.
13. As someone reviewing a DIFF with thousands of rows, I want to apply an action to all remaining rows, so that I do not have to click the same command a thousand times.
14. As someone reviewing a Grooming preview, I want a compare button on a row, so that I can inspect what a plan is about to delete before running it.
15. As someone pushing a sync group to several sinks, I want the push to read the main repository once, so that a group with many sinks does not re-read the same index repeatedly.
16. As someone scanning a repository on an external drive, I want a confirmation before every indexed file is marked missing, so that forgetting to mount the drive does not destroy the index.
17. As someone scripting dedup-rs, I want a `--force` flag that authorises that same destructive scan, so that automation can proceed deliberately without an interactive prompt.
18. As the maintainer of a sync group, I want that guard to exist specifically because an emptied main turns a MIRROR push into a wipe of every sink, so that one unmounted drive cannot cascade.
19. As a developer picking up this repository, I want one backlog document that matches the code, so that I do not plan work that is already done or assume work is done that is not.
20. As a developer, I want the second compare implementation deleted rather than extended, so that a new representation has one place to be added.
21. As a developer, I want each of these behaviours pinned by a test at an existing seam, so that the next refactor does not silently undo them.
22. As a developer, I want layout-sensitive changes verified by a rendered screenshot, so that a clipped or overlapping widget is caught the way previous label-query tests failed to catch it.

## Implementation Decisions

**Decided in session, in order:**

1. **Audio comparison uses the spectrogram, always.** The amplitude waveform is painter-drawn
   rather than produced as a texture, and comparison operates on textures. Rather than add a
   waveform-to-texture renderer, comparison standardises on the spectrogram. The amplitude
   waveform remains available in the native audio view, where its existing toggle is unchanged.
   Consequence, accepted: entering comparison from the amplitude view changes the visual.
2. **The native audio compare header gains its own DELETE A / DELETE B mark pills**, in addition
   to the marking already available on the Overview screen. Chosen for consistency with the image
   compare header, which already carries inline pills, so marking does not differ by media type.
3. **A scan that would mark every indexed entry missing must be authorised**: a confirmation in
   the GUI, a `--force` flag on the CLI. The existing warning stays; it is no longer the only
   defence. Emptying a repository on purpose remains supported and now costs one confirmation.
4. **The Transfer DIFF comparison is routed through the shared lightbox, and the Transfer view's
   private compare type is deleted.** This is the step the previous epic claimed to have taken.
   It requires generalising the comparison's B-side from "another member of this duplicate group"
   into an abstract source, so the two sides may come from different repositories — the piece the
   original epic listed first and never built. Audio, Metadata and Text in DIFF fall out of this
   rather than being implemented separately.
5. **Execution model: one agent, one working tree, tickets in the fixed order below.** Parallel
   worktrees were considered and rejected: three of these tickets want the Transfer view, four
   want the Duplicates view, and three want the board, so concurrent lanes would collide in files
   of 5–8 thousand lines.
6. **The agent cannot use git.** Destructive and staging git commands are denied at user scope, so
   the batch neither commits nor reverts. All work accumulates in the working tree for review as
   a single diff. A ticket that cannot be made to pass the gate is recorded in its own issue file
   under `## Comments`, its status set back to `needs-triage`, and the agent moves on rather than
   continuing to fight it.
7. **Ordering is safe-first, risky-last** — a direct consequence of 5 and 6. With no revert
   available and one shared tree, the large rework sits at position 11 so that a failure there
   leaves ten completed tickets intact ahead of it.

**Modules affected:** the core filter and its GUI builder; the sync-group planning and running
functions; the repository update/scan path and its CLI command; the application shell's tab
switch; the Duplicates view's audio player, compare header and lightbox; the shared board widget;
the Transfer view's DIFF board integration and comparison entry point; the Grooming view's row
command set.

**Interfaces changed:** the core file filter gains negation and a case-sensitivity option; the
scan entry point gains a caller-supplied authorisation for the destructive-empty case; the
comparison state's B side becomes an abstract source rather than a group index; the board's row
command vocabulary gains a compare command for planned surfaces and bulk header actions for DIFF.

**Explicitly unchanged:** content identity remains size plus BLAKE3, and no ticket alters the
stored entry format, so no `ENTRY_VERSION` bump and no migration are in scope.

## Testing Decisions

A good test here asserts externally observable behaviour — what a user of the module sees — not
the shape of the implementation behind it. This matters most for ticket 11, whose test must be
written against the Transfer view's observable outcome ("comparing a DIFF row offers these
representations") rather than against the private compare type, because that type is deleted by
the same ticket.

**This batch introduces no new seams.** All four seams below already exist and are used as-is:

| Seam | Used for | Prior art |
| --- | --- | --- |
| `crates/dedup-core/tests/*.rs` — temp repositories, asserting real index state | filter negation and case-sensitivity; single main read in group sync; the destructive-empty authorisation | `sync_groups.rs`, `update_repo.rs`, `repo_diff.rs`, `diff_ops.rs` |
| `crates/dedup-cli/tests/*.rs` — `assert_cmd` | the scan `--force` flag, both refused and accepted | `update_cli.rs` |
| inline `#[cfg(test)] mod ui_tests` — `egui_kittest`, geometric assertions on real layout | every GUI ticket | `board.rs` tests, `dupes_view.rs::ui_tests`, `transfer_view.rs::ui_tests` |
| `#[ignore]`d render tests writing a PNG for a human to look at | any ticket whose layout can clip or overlap | `doc_screenshot_board`, `doc_screenshot_lightbox_overview`, `doc_screenshot_groom_purge_board` |

**Standing rule, carried from the lightbox and board work:** a label query passes even when the
widget is clipped or off-screen. Layout claims must be asserted geometrically (rectangles inside
their region, regions not overlapping) and, where layout is the point, rendered to a PNG. The two
layout bugs found by rendering during the board work were both missed by passing label queries.

**Two tickets are tests only.** The audio switcher freeze and the ID3 tag-sync glitch were
plausibly fixed as a side effect of the Overview cycler's index mapping, but neither has ever been
run. Their tickets write the regression tests first and only then decide whether a fix is still
needed; a green test on first run closes the ticket as verified rather than fixed.

## Out of Scope

- **The light theme toggle.** Deferred by an explicit decision on 2026-07-08; it requires
  converting the theme constants into a runtime palette across every view.
- **Performance at the 10⁷-file scale.** Banded grouping, staged pipelines and the merged content
  index are adequate at 10⁵; revisiting content-index memory and timeline streaming needs test
  data this batch does not have.
- **Extraction of text from PDF and office documents.** The Text representation reads the head of
  a file; real extraction needs the core extractor and worker plumbing.
- **Writing EXIF.** EXIF remains display-only; there is no writer.
- **The ID3 comment field**, which the tag structure does not carry.
- **Cross-type comparison as a reachable feature.** The resolver is type-agnostic, but after this
  batch no surface deliberately compares an image against a spectrogram; nothing here adds a
  caller for it.
- **The Browse view's table.** A different kind of view, deliberately excluded from the board
  unification.
- **Committing, branching or pushing.** The batch produces a working-tree diff and nothing else.

## Further Notes

- `ai/improvements.md` is now the single backlog document; `ai/roadmap.md` and `ai/qa.md` were
  consolidated into it and deleted on 2026-07-30. Its claim that the Transfer DIFF comparison had
  been unified is corrected by ticket 11, which is the work that claim described.
- Three decisions in this spec meet the bar for an architecture decision record — spectrogram-only
  comparison, the destructive-scan authorisation, and routing DIFF through the shared lightbox —
  being hard to reverse, surprising without context, and the result of a genuine trade-off. This
  repository has no `docs/adr/` yet; creating it was not part of this spec.
- Each ticket's definition of done includes the standing gate: `cargo fmt --check` clean,
  `cargo clippy -- -D warnings` clean, `cargo test` green, and documentation updated in the same
  change (`README.md`, `CHANGELOG.md`, `ai/improvements.md`) rather than as a follow-up.

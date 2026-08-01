# 05 — Every caller opens it, and the other viewers are deleted

Status: resolved
Spec: ../spec.md
Blocked by: 04

## Problem

The law — clicking any file anywhere opens the viewer — is not true until every caller uses it.
Today: review board rows do nothing when clicked; Browse has its own single-image viewer with no
tabs and no comparison; Duplicates has its own tabbed one.

And the point of the whole effort is not reached until the old viewers are **gone**. The user
was explicit about why: *"we do this for the xths time, keeping variants is not helping."* This
session exists because a previous epic left a variant behind.

## Approach

Point every caller at the one viewer, then delete the others in the same change.

**Folded in from ticket `04`:** moving the audio player, spectrogram, transport and gapless
flip into the surviving viewer happens *here*, not before. Moving them while the old viewer
still runs would leave two owners of one audio device; the move and the deletion are the same
act. The five audio regression tests are migrated to drive the new viewer — their **assertions
must not change**, only which viewer they point at. Needing to weaken an assertion means the
move lost something.

- **Review board rows** become clickable and open it, with the row's two sides as the pair and
  no pool (one or two candidates, so no switcher). The board's own commands stay the row's.
- **Browse** opens it for any file, with the current listing as the pool.
- **Duplicates** opens it with the group as the pool.
- **The comparison command comes off review rows** — with every row opening the viewer, a
  row-level command is a second door to the same place. This undoes part of a ticket completed
  on 2026-07-31, deliberately.
- **Delete the Duplicates viewer, the separate audio viewer, and the Browse single-image
  viewer**, and migrate their remaining tests.

Actions stay caller-supplied throughout: Duplicates offers marks, review rows offer their own
commands, Browse offers tags. The viewer is shared; the decisions are not.

## Seam and tests

Each caller's existing inline tests, asserting the caller's observable behaviour:

- clicking a review row opens the viewer with that row's two sides
- a review row no longer offers a comparison command
- clicking a file in Browse opens it, with the listing as the pool, for a **non-image** file too
- opening from Duplicates offers the group as the pool and the marks as actions
- each caller gets its own actions back and no other caller's

**A completeness check rather than a behaviour test:** nothing in the crate references the
deleted viewers. That is the assertion that the variant is really gone, and it is the one this
whole effort exists to be able to make.

## Done

Standing gate green. The old viewers are deleted and nothing references them.

`CHANGELOG.md`, `README.md` if the law is worth a line, `ai/improvements.md`, and the GUI
documentation for Duplicates, Files and Browse. The light-theme tickets `02`–`05` are unblocked
by this landing.

## Comments

**Partly implemented 2026-08-01. Status stays `ready-for-agent` — the law holds for the review
boards, not yet for Duplicates or Browse, and no viewer has been deleted.** Gate green: fmt
clean, clippy 0 warnings, `cargo test --workspace` 24 suites / 0 failures. The tree builds and
the app works.

**Done: review board rows open the viewer.**

- `board::Cmd::OpenRow`, synthesised when the row *body* is clicked. It is never listed in a
  row's commands, so it can never be laid out as a button competing for row space — asserted by
  `a_row_body_click_reports_open_rather_than_a_command`.
- Grooming routes it to the shared viewer; Transfer/DIFF routes it to the same place COMPARE
  used to reach.
- **COMPARE is gone from rows**, which deliberately undoes part of ticket `10` of the
  2026-07-31 batch, exactly as this ticket specified. Rows are shorter for it.

**Also done: Browse is migrated and its viewer is deleted.** One of the three is gone.

- Clicking a file in Browse opens the shared viewer — for **any** file, not only pictures,
  which is the law reaching its third caller.
- `OpenLightbox`, `BrowseView::full_res` and the private `lightbox_modal` body are deleted;
  `lightbox_modal` is now a four-line delegation.
- Its test was updated deliberately: it asserted `FIT` and `1:1`, controls that belonged to the
  deleted single-image viewer. The behaviour under test — opens, Esc closes — is unchanged and
  still asserted; the file's own title now identifies what is open.
- Browse passes an **empty pool**, so no switcher renders. Stepping the whole listing wants the
  file list, which the preview dock does not hold. Noted, not faked.

**Not done, and the tree is deliberately left working rather than half-migrated:**

1. **Duplicates still uses its own viewer** — ~2,400 lines across `draw_lightbox_overview`,
   `lightbox_modal` and `audio_lightbox`.
2. **The audio move**, folded here from ticket `04`: the player, spectrogram, transport and
   gapless flip still live in the Duplicates viewer. Moving them is the same act as deleting
   that viewer — do them together or not at all, or two things own one audio device.
3. **No viewer has been deleted**, so the completeness check this ticket exists to make —
   *nothing references the old viewers* — cannot yet be made.
4. The five audio regression tests still drive the old viewer. They pass **unmodified**; when
   they are repointed, their assertions must not change.

**Why it stopped here rather than pressing on.** The remaining work is a single indivisible
step: move ~1700 lines of viewer, repoint two callers, migrate their tests, delete three
viewers. Started and not finished, it leaves a tree that does not build — and with the old
viewer half-gutted, the five regression tests that are the only proof the audio behaviour
survived would be the first thing to break. Stopping at a boundary where everything compiles
and every test passes is worth more than partial progress into that.

**Blocker cleared: the shared viewer now drives audio.** `view` takes the caller's
`Option<&Player>` — the caller owns the device, because the Duplicates tab also drives the
inline play buttons on the cards behind the viewer, and two owners of one audio device is the
state to avoid. The Audio tab renders a transport per side (PLAY A / PLAY B / PAUSE) plus the
cycling SPEED control. Pinned by `the_audio_tab_plays_the_side_asked_for`, which asserts the
side asked for is the one loaded, not merely that a control exists.

Callers that have no audio pass `None`, so Browse, Grooming and DIFF are unaffected.

**What remains is now only the Duplicates migration**, in this order: repoint Duplicates at the
shared viewer with the group as its pool and its own player passed in; delete
`draw_lightbox_overview`, `lightbox_modal` and `audio_lightbox` (~2,400 lines); migrate their
tests — the five audio regressions among them, whose **assertions must not change**, only which
viewer they drive; then make the completeness check that nothing references the deleted
viewers.

Browse being done first was the right call: it proved the shared viewer works as a caller's
only viewer, and cost nothing to reverse if it had not.

**Resolved 2026-08-01 — the Duplicates migration landed and the old viewers are gone.** Gate
green: fmt clean, clippy (`--all-targets -D warnings`) clean, `cargo test --workspace` 24
suites / 0 failures, and the suite was run five times consecutively to shake out
repaint-timing flakes (found two, fixed by stepping fixed frames instead of running to a
settled state wherever a background decode wakes the UI).

**What the shared viewer gained for its last caller, TDD one slice at a time:**

- **Caller-supplied marks** (`MarkPill` / `set_marks` / `DiffPick::ToggleMark`): a side given
  mark state shows the shared `mark_pill` (DELETE / DELETE A / DELETE B, protected =
  disabled + struck through) instead of the DIFF commands, and toggling reports to the caller
  without closing the viewer.
- **The gapless audio transport**: `P` toggles; playing a two-sided audio pair loads
  `Player::play_pair`, arrows flip the audible copy on the loaded pair (`audio_active` is the
  cursor side), a flicker swap flips the audio with the picture, and keep-pair maintenance
  re-pairs after a side steps. Single-view arrows step the pool and follow the transport —
  paused stays paused with the new copy loaded.
- **Tag editing on the Metadata tab** for a writable audio side (`DiffSide.read_only` is
  new; every existing caller passes `true`): T / EDIT TAGS / SAVE TAGS / CANCEL, adopt-values
  gathered from the pool, Esc cancels the editor before it closes the viewer.
- **The switcher position label** (`<1 / 3>`), counting that side's candidates with the other
  side's file masked — and pool identity switched from `rel_path` to **`abs_path`**, because
  cross-repo duplicates routinely share their relative path (a self-comparison bug the old
  index-based pool never had).
- **`refresh_side`**: a stepped or revealed side forgets its texture/text/tags and re-decodes
  — without this a stepped side kept showing its predecessor (caught by a migrated test).
- SHOW B lands on another candidate, never the file A shows; the Overview tab button is gone
  from the tab bar (spec decision 7); the title is caller-supplied (Duplicates says
  `COMPARE — DUPLICATE GROUP`, not DIFF's "SAME PATH, DIFFERENT CONTENT"); and the per-side
  control rows stack vertically instead of chaining past the window edge (the render check
  caught MIRROR B clipped at 1000px — the exact defect class the spec complained about).

**Duplicates as a caller** (`dupes_view`): `Act::OpenLightbox` builds a `DiffCompare` with
the clicked member alone (B hidden), the whole group as the pool and per-side `read_only`
from the repo; `viewer_modal` supplies marks per frame from the tab's own
`marked`/`unlocked`, routes `ToggleMark` to `Act::ToggleMark`, and stops the player on close
only when the viewer started the audio. ~2,900 lines of old viewer code deleted
(`draw_lightbox_overview`/`_metadata`/`_text`, `lightbox_modal`, the audio viewer,
`lightbox_shell`, the edit-save modal, `previewable_texture`, `LightboxState`,
`FullResCache`, `single_view`, `full_texture`, the waves/spec caches).

**The five audio regressions pass with their assertions unchanged** — only the drive changed
(open through `Act::OpenLightbox`, SHOW B instead of `C`, NEXT B instead of NEXT OTHER):
gapless arrow flip (`audio_active == Some(1)` still asserted verbatim — the field kept its
name and meaning), paused stepping, per-copy tags, the four-copy `<i / 3>` cycler labels, and
independent DELETE A/B marks. Tests whose *subject* was deleted went with it (Overview tests,
the edit-save test, the space-flicker Esc-chain that headless can't drive without decoded
textures, `previewable_texture` dispatch) — each superseded or consciously dropped.

**The completeness check exists**:
`compare_view::tests::nothing_in_the_crate_references_the_deleted_viewers` scans every crate
source file for the deleted viewers' identifiers (assembled at runtime so its own source
cannot trip it). It is the assertion this effort existed to make, and it will fail the build
the day a variant creeps back.

**Consciously dropped, recorded in `CHANGELOG.md` (Removed):** the in-viewer image edit save
(rotate/mirror stay as viewing aids; spec decision 10 lists no save), the audio
waveform/spectrogram toggle, and the video filmstrip scrubber (one still per side).

Docs updated in the same change: `docs/gui/duplicates.md` (viewer section rewritten),
`docs/gui/files.md` (row-click replaces COMPARE), `README.md` (the law got its line),
`CHANGELOG.md`, `ai/improvements.md`. The light-theme tickets `02`–`05` are unblocked.

# 03 — Picture tools, and metadata that survives comparison

Status: resolved
Spec: ../spec.md
Blocked by: 02

## Problem

Rotating an image and then comparing loses the rotation, and the comparison offers no way to
rotate. That defeats the case the user named as the whole point: *"exactly here you want to
check if a file is the same even if someone flipped it."*

Metadata, meanwhile, is unreachable while comparing, because comparing used to be a different
screen. With the shell from ticket 01 that is no longer structural — it just needs wiring.

## Approach

Move the picture capabilities into the surviving viewer and give each tab its own tools.

- **Tools belong to the tab and vary by type.** Pictures get rotate, mirror, fit and zoom/pan.
- **Rotation and mirroring carry into the comparison** and remain available inside it, applied
  per side so one can be aligned to the other.
- **Metadata is reachable while comparing** — it is a tab like any other now.
- Metadata is **read-only for images** (there is no EXIF writer, so no edit control is offered)
  and **editable for audio**, as today.

Nothing here should reintroduce a second screen: every tool is a control within its tab.

## Seam and tests

The viewer's own inline tests:

- rotating a picture and then comparing keeps the rotation
- rotating inside the comparison affects only the side it was applied to
- mirroring likewise — the flipped-copy case, asserted directly
- the metadata tab is reachable while comparing, and shows each side's own values
- an image's metadata offers no edit control; audio's does
- a read-only repository disables the deletion controls and shows them struck through

Render check: a screenshot of a comparison with one side rotated, looked at — the alignment is
the point and it is a visual claim.

## Done

Standing gate green. `CHANGELOG.md`, and the GUI documentation for the Duplicates tab.

## Comments

**Implemented 2026-08-01, TDD.** Gate green: fmt clean, clippy 0 warnings, `cargo test
--workspace` 24 suites / 0 failures.

**Rotation is per side and survives comparison**, which was the point: *"exactly here you want
to check if a file is the same even if someone flipped it."* Orientation operations are held
on the viewer per side rather than baked into the texture, so entering or leaving the
comparison cannot lose them. Turning a side re-applies its operations to the **decoded pixels**
— kept for this purpose — and re-uploads, so nothing is decoded twice, and it routes through
the same shared `imgedit::apply_ops` the single-file editor uses rather than a second
implementation.

`oriented_size` feeds the layout, so a quarter-turned side lays out at its new aspect instead
of its stored one. Tested as a pure function first (635×465 becomes 465×635, and the other side
is untouched), then through the UI (ROTATE A turns A and not B).

**Metadata is reachable while comparing** — the user's report that it vanished on comparing was
a symptom of comparing being a separate screen, which ticket `01` removed. It needed no wiring
beyond the tab row now always being present; the test pins it so it cannot regress. Image
metadata offers no edit control, since there is no EXIF writer.

Two helpers are `#[cfg(test)]` rather than carrying an `#[allow(dead_code)]`, which `AGENTS.md`
forbids: `set_base_size` (simulates a decode without one) and `ops_len`.

**Not done, deliberately:** a render check of a rotated comparison. It is a visual claim and
deserves looking at, but the piece it would show — alignment of a flipped pair — is most
meaningful once the Duplicates callers arrive in ticket `05` with real scans. Noted there.

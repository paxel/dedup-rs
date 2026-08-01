# 05 — Make repository identicons legible on light

Status: ready-for-agent
Spec: ../spec.md
Blocked by: 04

## Problem

Two known defects, identified while specifying the light palette rather than discovered
afterwards:

1. The identicon paints a **hardcoded dark tile** behind its cells. Its own comment explains
   why — *"a dark tile makes the pastel cells legible on any chip fill"* — which was sound while
   every chip fill was dark. On a light chip it is a black square.
2. Its cell hues are high-lightness pastels chosen to glow against black, and will be
   low-contrast on a light background.

Identicons are how repositories are told apart at a glance across every tab, so this is
legibility, not polish.

## Approach

Make both the tile and the cell lightness follow the active palette rather than being fixed for
dark.

The identicon's **identity must not change**: the same repository name must keep producing the
same glyph and the same hue. Only lightness and the tile behind it adapt. A repository that
looks different after switching appearance would defeat the purpose of having a stable
identicon at all.

Judge the result on the both-palettes image from ticket 02, extended to show several chips.

## Seam and tests

Repo-chip inline tests, prior art `identicon_is_deterministic_and_distinct`:

- the same name yields the same glyph and hue in **both** palettes — determinism survives the
  change
- different names still yield different glyphs
- cell colours meet a contrast threshold against the tile, in both palettes — the actual defect,
  asserted rather than eyeballed

Render check: the both-palettes image with several repo chips, looked at.

## Done

Standing gate green. `CHANGELOG.md` only if a user would notice the dark appearance changed —
if the dark identicon is untouched, this is part of the light-theme entry rather than its own.

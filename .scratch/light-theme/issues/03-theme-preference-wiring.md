# 03 — Resolve the active appearance from egui's theme preference

Status: resolved
Spec: ../spec.md
Blocked by: 02

## Problem

Two palettes exist but only the dark one is ever installed. Something has to decide which is
active, follow the operating system when asked to, and switch without a restart.

## Approach

**Use egui's own preference model — do not invent one.** egui 0.35 already provides a three-way
theme preference, reports the operating system's setting, resolves which theme is active, and
supports registering a separate style per theme. This ticket supplies the two palettes and lets
egui answer "which one".

- The style-application entry point registers a style for **each** theme instead of installing
  one dark visual set.
- Each frame, the thread-local palette follows the resolved active theme, so a change takes
  effect on the next repaint.
- Switching is **live**: nothing bakes a theme colour into a cached texture — the repo identicon
  is painter-drawn every frame — so installing a palette and repainting is the whole operation.
  A restart requirement was offered by the user and declined for buying no simplification.

Nothing persists yet and there is no control yet; those are ticket 04. Until then the
preference can only be set programmatically, which is what the tests do.

## Seam and tests

Theme-module unit tests:

- resolving to the light theme installs the light palette; resolving to dark installs the dark
  one
- changing the resolved theme changes what the accessors return, with no restart and no cache to
  invalidate

GUI inline test, at the highest seam that shows the whole thing works:

- rendering a surface under each theme produces the corresponding palette's colours — one
  assertion that the resolution, installation and reading all connect

Do **not** test that egui reports the operating system's setting correctly; that is egui's
behaviour, not this application's, and there is no way to set a real OS preference from a test.

## Done

Standing gate green. Still no user-facing control, so `CHANGELOG.md` waits for ticket 04.

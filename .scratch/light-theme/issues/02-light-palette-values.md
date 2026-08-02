# 02 — The light palette, and the image it is judged from

Status: resolved
Spec: ../spec.md
Blocked by: 01

## Problem

With the palette expressible as data, a light one has to be designed. It cannot be derived by
inverting the dark one: four of the twelve colours carry meaning on the review board — grey
unchanged, green only-on-this-side, red will-be-deleted, amber conflict-or-renamed — and amber
and tan are already near-neighbours that converge further when darkened. A mechanical flip
would leave "will be deleted" and "differs" indistinguishable on a screen where the
consequence is deleted files.

## Approach

Add a second palette whose four semantic colours are **dark variants** of grey, green, red and
amber, chosen by hand against a light background, with the remaining colours picked to suit.

The user is the customer for this appearance and will choose the final values by eye. The job
here is to give them a good starting set and, more importantly, the artifact to judge it from:

- an `#[ignore]`d render test emitting **both palettes in a single image** — the review board
  with all four statuses, LCARS pills, an elbow rail, a repo chip and body text, dark beside
  light. One picture, judged as a pair.
- Expect to iterate the values against that image rather than getting them right first time.

The LCARS pills and elbow rails are tried as they are on a light background and adjusted only
where they actually fail; they may need less work than expected.

## Seam and tests

Theme-module unit tests, in both palettes:

- the four semantic colours are pairwise separated by a **minimum perceptual distance**, not
  merely unequal — two colours differing by one channel value are "distinct" and still
  indistinguishable on screen
- body text meets a contrast threshold against the panel background
- the dark palette's values are unchanged from ticket 01

Board inline tests — extend `the_four_statuses_have_distinct_colours` into a **loop over both
palettes** rather than adding a second test, so a future palette cannot be added without being
checked.

Render check: the both-palettes image, generated and **looked at**. Per this repo's standing
lesson, a colour that passes a numeric threshold can still look wrong.

## Done

Standing gate green. The both-palettes image exists and has been looked at. No user-visible
change yet — the palette is not reachable until ticket 03 — so no `CHANGELOG.md` entry.

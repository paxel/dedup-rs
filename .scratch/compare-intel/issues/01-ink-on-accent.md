# 01 — Ink-on-accent legibility for filled pills

**What to build:** On the light appearance, text on filled coloured "pills" (review-board and
viewer buttons — DELETE, OVERWRITE OTHER, and the rest) is dark ink on the light palette's
deliberately-dark accents, so it reads poorly (the "check the delete buttons" report). Introduce
a single palette-aware "ink on accent" colour — black on the dark appearance (unchanged), a light
near-white ink on the light appearance — and route every filled pill's text through it, so any
filled button is legible on either appearance.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [ ] A theme accessor returns the correct ink for text drawn on an accent *fill*, per the active
      palette; the dark appearance is byte-identical to today.
- [ ] Every filled pill uses it instead of the fixed dark ink (audit the filled-fill sites, not
      the ink-on-plain-background ones).
- [ ] A test asserts the ink meets a contrast threshold against each accent used as a fill, in
      both palettes (prior art: the existing palette contrast tests).
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, and the tests are green.

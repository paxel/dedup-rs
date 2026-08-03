# 04 — Hex diff: jump-to-difference + honest degrade notice

**What to build:** On the aligned hex diff, controls to **jump to the next/previous differing
region** so long equal runs can be skipped, and an on-screen **notice** when the pair was too
large for exact alignment and fell back to block-level ("file too large for full alignment —
showing block-level"). Together these make a full-file diff of a mostly-equal large pair actually
navigable, and keep the tool honest about when it stopped looking exhaustively.

**Blocked by:** 03 — Hex tab: full-file, aligned, paginated diff.

**Status:** resolved

- [ ] Next/prev-difference controls move the view to the following/preceding differing region.
- [ ] With no differences, the controls are inert/absent and the view states the shown content is
      identical.
- [ ] When the engine degraded (from 02), the notice is shown; when the alignment was exact, it is
      not.
- [ ] A harness test covers jumping landing on a difference, and the degrade notice appearing.
- [ ] Gate green.

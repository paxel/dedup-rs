# 05 — Image view: state pixel-identity

**What to build:** When the two images being compared decode to **byte-identical rasters**, the
image view says so plainly — a neutral line: *"Pixels identical — no visual difference;
differences are in metadata"* — and offers a **quiet pointer** to the tab that does differ
(metadata/hex) **without** switching the user's tab for them. This explains why flicker appears
to do nothing on such a pair (the only thing changing was the corner A/B label). It is a
comparison *result*, not a keep/delete verdict.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [ ] Identical rasters ⇒ the neutral banner appears in the image view.
- [ ] Differing rasters ⇒ no banner.
- [ ] The pointer to the differing tab does not auto-change the selected tab.
- [ ] Raster-equality is a tested predicate; a harness test covers the banner's presence/absence.
- [ ] Gate green.

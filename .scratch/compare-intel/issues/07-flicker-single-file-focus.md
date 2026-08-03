# 07 — Flicker: single-file focus

**What to build:** Flicker becomes a single-file mode. The top chrome shows only the
**currently-visible** file — its repo/size/path (with the green distinction highlight still
relative to the other side), its rotate/mirror/save, and its delete — never two sides. **SWAP
flips the image and all of that chrome to the other file together.** Flicker is image/media-only:
its controls (SWAP / SIDE BY SIDE) no longer appear on the Text/hex tab. This removes the
hidden-side control you could click by accident ("ROTATE B does nothing").

**Blocked by:** 06 — Lock semantics (the single-file chrome reuses its save-action policy).

**Status:** resolved

- [ ] In flicker, only the visible side's facts and tools are shown; SWAP flips image and chrome
      together.
- [ ] The green distinction highlight stays in the single-file strip and flips on SWAP.
- [ ] Save/delete in the single-file chrome obey the lock policy from 06.
- [ ] Flicker controls do not render on the hex/Text tab; flicker stays image/media-only.
- [ ] A harness test asserts SWAP changes which side's facts/tools are shown.
- [ ] Gate green.

# 08 — Metadata: compare and salvage to a sidecar

**What to build:** On the Metadata tab, **highlight which EXIF/TIFF fields differ** between the
two sides, and add a control to **save a side's metadata to a human-readable sidecar file**
(decoded text/JSON of the fields) in a folder the user picks — so the metadata can be salvaged
before a copy is deleted. Whole-blob per side. No per-field selection, and no merge/write-back
into the image (deferred).

**Blocked by:** None — can start immediately.

**Status:** resolved

- [ ] Fields that differ between the two sides are visibly marked on the Metadata tab.
- [ ] A per-side control writes that side's metadata to a human-readable sidecar in a picked
      folder (a new file — allowed even on a locked repo, since it only adds).
- [ ] The field-diff and the sidecar serialization are tested as pure functions.
- [ ] Gate green.

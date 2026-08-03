# 06 — Lock semantics: save-as-copy when locked

**What to build:** A locked ("Protected") repo protects the **existing** files but permits adding
**new** ones. In the viewer: DELETE and SAVE-overwrite-in-place stay blocked on a locked side, but
SAVE-**as-a-new-copy** is allowed (it only adds a suffixed sibling, indexed on the next scan). So
SAVE appears even on a locked side, offering only "save a copy"; the blocked overwrite and delete
show **disabled with the reason** ("locked — original protected; unlock in Duplicates") rather
than silently vanishing. The lock stops you *losing or altering* an inherited original, not
deriving a corrected copy alongside it.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [ ] A save-action policy decides, from read-only + pending edits, which of {save-copy,
      overwrite-in-place} are offered — a pure predicate, unit-tested directly.
- [ ] On a locked side with edits, SAVE appears and the dialog offers "save a copy" but not
      "overwrite in place".
- [ ] DELETE and overwrite-in-place on a locked side render **disabled with a reason**, not hidden.
- [ ] On an unlocked side, behaviour is unchanged (both save options; delete active).
- [ ] Gate green.

# 03 — Hex tab: full-file, aligned, paginated diff

**What to build:** The viewer's Text/hex tab stops showing a header-only preview and instead
renders the **whole file** as a two-column hex diff driven by the alignment engine: equal bytes
line up, a gap shows as padding on one side, a substitution shows the differing bytes on both —
with region background **bands** plus **per-byte** emphasis in the review-board vocabulary (gap =
green, substitution = amber, equal plain), legible in both appearances. The view **paginates**
through the entire file. So an inserted header no longer makes everything after it read as
different, and you can see more than the header.

**Blocked by:** 02 — Byte-alignment engine.

**Status:** resolved

- [ ] Both sides' full files are shown, aligned — not just the first bytes.
- [ ] Equal regions align; gaps pad one side; substitutions highlight the differing bytes on both.
- [ ] Region bands + per-byte colour (green / amber / plain), theme-aware for light and dark.
- [ ] The view paginates through the whole file.
- [ ] A harness test asserts the aligned rendering for a known inserted-header pair (prior art:
      the `compare_view` harness tests + an ignored render screenshot).
- [ ] Gate green.

# 02 — Byte-alignment engine (core)

**What to build:** A pure `dedup-core` capability that aligns two byte streams and reports, in
order, the runs where they are **equal**, where bytes exist on one side only (a **gap** —
insertion/deletion), and where they differ in place (a **substitution**) — repeating as often as
the content requires. Alignment is exact/optimal up to a size-or-complexity budget; beyond it, it
degrades to a faster anchor-based (rolling-hash) alignment and flags that it did so. It has no UI
of its own — it is the foundation the hex diff renders. Verifiable on its own via core tests.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [ ] Given two byte slices, returns an ordered run sequence (equal / gap-on-side / substitution)
      that reconstructs both sides.
- [ ] Equal spans are reported equal even when shifted by an insertion earlier in the stream (the
      inserted-header case).
- [ ] Interleaved differences (multiple equal and differing regions, repeated) are handled — not
      just a single inserted block.
- [ ] Over the budget it degrades to anchor-based alignment and the result carries a "degraded"
      indicator; under budget it is exact.
- [ ] Core tests cover identical inputs, pure insertion, pure deletion, substitution, an
      interleaved mix, and the degrade path (prior art: `tests/diff_ops.rs`).
- [ ] Gate green.

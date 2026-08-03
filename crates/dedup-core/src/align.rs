//! Byte-level alignment of two files for the compare viewer's hex diff.
//!
//! Given two byte streams, [`align`] reports — in order — the runs where they
//! are **equal**, and the runs where they differ. A difference run carries the
//! bytes on each side; either side may be empty, which is how a pure insertion
//! or deletion (a *gap*) is expressed, and both non-empty is a substitution.
//! Concatenating every segment's `a` ranges reconstructs the first stream, and
//! the `b` ranges the second — the alignment never drops or invents a byte.
//!
//! The common case for this tool is two near-duplicate files that share a long
//! identical payload and differ only in an inserted header (embedded metadata).
//! Trimming the common prefix and suffix collapses that to a tiny middle, which
//! is aligned exactly with an LCS. Only a large, pervasively-different middle
//! exceeds the exact budget; it then falls back to a fast, block-granular
//! anchor alignment and the result is flagged [`Alignment::degraded`] so the UI
//! can say so rather than pretending it looked exhaustively.

use std::collections::HashMap;
use std::ops::Range;

/// Whether a [`Segment`] is a matching run or a differing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Equal bytes on both sides (`a` and `b` have the same length and content).
    Equal,
    /// Differing bytes. Either side may be empty: `a` empty is an insertion in
    /// `b`, `b` empty is a deletion from `a`, both non-empty a substitution.
    Diff,
}

/// One aligned run, as byte ranges into the two inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegmentKind,
    pub a: Range<usize>,
    pub b: Range<usize>,
}

/// The full alignment of two byte streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alignment {
    /// Segments in order; together they cover both inputs exactly.
    pub segments: Vec<Segment>,
    /// True when the middle exceeded the exact budget and a coarse,
    /// block-granular alignment was used instead. The UI surfaces this.
    pub degraded: bool,
}

/// Product of the two middle lengths above which the exact LCS is skipped for
/// the block-granular fallback. 4M cells ≈ 16 MB of scratch — comfortably fast,
/// and only reached when the *middle* (after prefix/suffix trimming) is large,
/// which the near-duplicate case never is.
const EXACT_BUDGET: usize = 4_000_000;

/// Block size for the degraded, anchor-based alignment.
const BLOCK: usize = 64;

/// Align two byte streams. See the module docs for the guarantees.
pub fn align(a: &[u8], b: &[u8]) -> Alignment {
    // Trim the common prefix and the common suffix (not overlapping the prefix).
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    let mid_a = &a[pre..a.len() - suf];
    let mid_b = &b[pre..b.len() - suf];

    let mut segments = Vec::new();
    if pre > 0 {
        segments.push(Segment {
            kind: SegmentKind::Equal,
            a: 0..pre,
            b: 0..pre,
        });
    }
    let degraded = if mid_a
        .len()
        .checked_mul(mid_b.len())
        .is_some_and(|product| product <= EXACT_BUDGET)
    {
        segments.extend(lcs_segments(mid_a, mid_b, pre, pre));
        false
    } else {
        segments.extend(block_segments(mid_a, mid_b, pre, pre));
        true
    };
    if suf > 0 {
        segments.push(Segment {
            kind: SegmentKind::Equal,
            a: a.len() - suf..a.len(),
            b: b.len() - suf..b.len(),
        });
    }
    coalesce(&mut segments);
    Alignment { segments, degraded }
}

/// Exact alignment of a (small) middle via a longest-common-subsequence table,
/// backtracked into equal/diff runs. `base_a`/`base_b` shift the ranges back
/// into the whole-file coordinates.
fn lcs_segments(a: &[u8], b: &[u8], base_a: usize, base_b: usize) -> Vec<Segment> {
    let (n, m) = (a.len(), b.len());
    if n == 0 && m == 0 {
        return Vec::new();
    }
    if n == 0 || m == 0 {
        return vec![Segment {
            kind: SegmentKind::Diff,
            a: base_a..base_a + n,
            b: base_b..base_b + m,
        }];
    }
    // dp[i*w + j] = LCS length of a[i..] and b[j..].
    let w = m + 1;
    let mut dp = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * w + j] = if a[i] == b[j] {
                dp[(i + 1) * w + (j + 1)] + 1
            } else {
                dp[(i + 1) * w + j].max(dp[i * w + (j + 1)])
            };
        }
    }
    // Backtrack into a step list, then coalesce steps into segments.
    enum Step {
        Match,
        DelA,
        InsB,
    }
    let mut steps = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] && dp[i * w + j] == dp[(i + 1) * w + (j + 1)] + 1 {
            steps.push(Step::Match);
            i += 1;
            j += 1;
        } else if dp[(i + 1) * w + j] >= dp[i * w + (j + 1)] {
            steps.push(Step::DelA);
            i += 1;
        } else {
            steps.push(Step::InsB);
            j += 1;
        }
    }
    while i < n {
        steps.push(Step::DelA);
        i += 1;
    }
    while j < m {
        steps.push(Step::InsB);
        j += 1;
    }

    let mut segs = Vec::new();
    let (mut ai, mut bj) = (base_a, base_b);
    let mut k = 0;
    while k < steps.len() {
        if matches!(steps[k], Step::Match) {
            let (sa, sb) = (ai, bj);
            while k < steps.len() && matches!(steps[k], Step::Match) {
                ai += 1;
                bj += 1;
                k += 1;
            }
            segs.push(Segment {
                kind: SegmentKind::Equal,
                a: sa..ai,
                b: sb..bj,
            });
        } else {
            let (sa, sb) = (ai, bj);
            while k < steps.len() && !matches!(steps[k], Step::Match) {
                match steps[k] {
                    Step::DelA => ai += 1,
                    Step::InsB => bj += 1,
                    Step::Match => {}
                }
                k += 1;
            }
            segs.push(Segment {
                kind: SegmentKind::Diff,
                a: sa..ai,
                b: sb..bj,
            });
        }
    }
    segs
}

/// Coarse, O(n) fallback for a middle too large to align exactly: index `b`'s
/// non-overlapping blocks by hash and greedily anchor `a`'s blocks to the next
/// matching block position in `b`, extending each match run byte-for-byte.
/// Unmatched spans (including the bytes skipped in `b` before an anchor) become
/// diff segments. Block-granular, hence [`Alignment::degraded`].
fn block_segments(a: &[u8], b: &[u8], base_a: usize, base_b: usize) -> Vec<Segment> {
    let mut index: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut j = 0;
    while j + BLOCK <= b.len() {
        index.entry(fnv64(&b[j..j + BLOCK])).or_default().push(j);
        j += BLOCK;
    }

    let mut segs = Vec::new();
    let mut ia = 0usize; // byte cursor in a
    let mut jb = 0usize; // byte cursor in b (bytes consumed by matches)
    let (mut pend_a, mut pend_b) = (0usize, 0usize); // start of the pending diff

    while ia + BLOCK <= a.len() {
        let hit = index
            .get(&fnv64(&a[ia..ia + BLOCK]))
            .and_then(|positions| positions.iter().copied().find(|&pos| pos >= jb));
        match hit {
            Some(bpos) => {
                // Everything from the last anchor up to this one differs: the
                // a-bytes we skipped and the b-bytes between the cursor and bpos.
                if pend_a < ia || pend_b < bpos {
                    segs.push(Segment {
                        kind: SegmentKind::Diff,
                        a: base_a + pend_a..base_a + ia,
                        b: base_b + pend_b..base_b + bpos,
                    });
                }
                // Extend the equal run as far as the bytes keep matching.
                let (mut ea, mut eb) = (ia, bpos);
                while ea + BLOCK <= a.len()
                    && eb + BLOCK <= b.len()
                    && a[ea..ea + BLOCK] == b[eb..eb + BLOCK]
                {
                    ea += BLOCK;
                    eb += BLOCK;
                }
                segs.push(Segment {
                    kind: SegmentKind::Equal,
                    a: base_a + ia..base_a + ea,
                    b: base_b + bpos..base_b + eb,
                });
                ia = ea;
                jb = eb;
                pend_a = ia;
                pend_b = jb;
            }
            None => ia += BLOCK,
        }
    }
    // The tail on both sides is one final diff.
    if pend_a < a.len() || pend_b < b.len() {
        segs.push(Segment {
            kind: SegmentKind::Diff,
            a: base_a + pend_a..base_a + a.len(),
            b: base_b + pend_b..base_b + b.len(),
        });
    }
    segs
}

/// Merge adjacent same-kind segments left contiguous by the prefix/suffix split.
fn coalesce(segments: &mut Vec<Segment>) {
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for seg in segments.drain(..) {
        match merged.last_mut() {
            Some(prev)
                if prev.kind == seg.kind
                    && prev.a.end == seg.a.start
                    && prev.b.end == seg.b.start =>
            {
                prev.a.end = seg.a.end;
                prev.b.end = seg.b.end;
            }
            _ => merged.push(seg),
        }
    }
    *segments = merged;
}

/// FNV-1a 64-bit hash of a byte slice (block fingerprint for the fallback).
fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The segments must reconstruct both inputs exactly — no byte dropped or
    /// invented — regardless of how the alignment was found.
    fn assert_reconstructs(a: &[u8], b: &[u8], al: &Alignment) {
        let mut ra = Vec::new();
        let mut rb = Vec::new();
        for seg in &al.segments {
            ra.extend_from_slice(&a[seg.a.clone()]);
            rb.extend_from_slice(&b[seg.b.clone()]);
            if seg.kind == SegmentKind::Equal {
                assert_eq!(&a[seg.a.clone()], &b[seg.b.clone()], "equal seg not equal");
            }
        }
        assert_eq!(ra, a, "a not reconstructed");
        assert_eq!(rb, b, "b not reconstructed");
    }

    fn equal_bytes(al: &Alignment, a: &[u8]) -> usize {
        al.segments
            .iter()
            .filter(|s| s.kind == SegmentKind::Equal)
            .map(|s| s.a.len())
            .sum::<usize>()
            .min(a.len())
    }

    #[test]
    fn identical_inputs_are_one_equal_run() {
        let a = b"the same bytes on both sides";
        let al = align(a, a);
        assert!(!al.degraded);
        assert_eq!(al.segments.len(), 1);
        assert_eq!(al.segments[0].kind, SegmentKind::Equal);
        assert_reconstructs(a, a, &al);
    }

    #[test]
    fn both_empty_is_no_segments() {
        let al = align(b"", b"");
        assert!(al.segments.is_empty());
        assert!(!al.degraded);
    }

    #[test]
    fn a_pure_insertion_is_a_gap_with_the_rest_equal() {
        // b has an inserted header; the payload after it is identical.
        let a = b"PAYLOAD-that-is-long-and-shared";
        let b = b"HDRPAYLOAD-that-is-long-and-shared";
        let al = align(a, b);
        assert!(!al.degraded);
        assert_reconstructs(a, b, &al);
        // The inserted "HDR" is a diff with an empty a-side (a gap in a).
        let gap = al
            .segments
            .iter()
            .find(|s| s.kind == SegmentKind::Diff)
            .expect("a diff run");
        assert!(gap.a.is_empty(), "insertion has an empty a-side");
        assert_eq!(&b[gap.b.clone()], b"HDR");
        // Nearly everything is still equal.
        assert!(equal_bytes(&al, a) >= a.len() - 1);
    }

    #[test]
    fn a_pure_deletion_is_a_gap_on_the_b_side() {
        let a = b"HDRPAYLOAD-that-is-long-and-shared";
        let b = b"PAYLOAD-that-is-long-and-shared";
        let al = align(a, b);
        assert!(!al.degraded);
        assert_reconstructs(a, b, &al);
        let gap = al
            .segments
            .iter()
            .find(|s| s.kind == SegmentKind::Diff)
            .expect("a diff run");
        assert!(gap.b.is_empty(), "deletion has an empty b-side");
    }

    #[test]
    fn a_substitution_differs_on_both_sides_then_re_aligns() {
        let a = b"prefix-AAAA-suffix-that-is-shared-and-long";
        let b = b"prefix-BBBB-suffix-that-is-shared-and-long";
        let al = align(a, b);
        assert!(!al.degraded);
        assert_reconstructs(a, b, &al);
        let sub = al
            .segments
            .iter()
            .find(|s| s.kind == SegmentKind::Diff)
            .expect("a diff run");
        assert!(!sub.a.is_empty() && !sub.b.is_empty(), "both sides differ");
        // The long shared suffix re-aligns as equal.
        assert!(equal_bytes(&al, a) >= 20);
    }

    #[test]
    fn interleaved_differences_repeat_equal_and_diff_runs() {
        // Several equal islands separated by differing regions.
        let a = b"COMMON1xxxCOMMON2yyyCOMMON3zzzCOMMON4";
        let b = b"COMMON1aaaCOMMON2bbbbCOMMON3cCOMMON4";
        let al = align(a, b);
        assert!(!al.degraded);
        assert_reconstructs(a, b, &al);
        let equals = al
            .segments
            .iter()
            .filter(|s| s.kind == SegmentKind::Equal)
            .count();
        let diffs = al
            .segments
            .iter()
            .filter(|s| s.kind == SegmentKind::Diff)
            .count();
        assert!(equals >= 3, "the COMMON islands align as equal runs");
        assert!(diffs >= 3, "the regions between them are diffs");
    }

    #[test]
    fn a_large_pervasively_different_middle_degrades_but_still_reconstructs() {
        // No common prefix/suffix and a middle over the exact budget, so the
        // fallback runs. It shares repeating 64-byte blocks so anchoring finds
        // equal runs, and it must still reconstruct both sides exactly.
        let block: Vec<u8> = (0..64u8).collect();
        let mut a = Vec::new();
        let mut b = Vec::new();
        for i in 0..40_000u32 {
            a.extend_from_slice(&block);
            b.extend_from_slice(&block);
            // Perturb b every so often so the middle is genuinely different and
            // large enough to exceed the budget after prefix/suffix trimming.
            if i % 3 == 0 {
                a.push(0xAA);
                b.push(0xBB);
            }
        }
        // Break the common prefix and suffix so trimming can't shrink the middle.
        a.insert(0, 0x01);
        b.insert(0, 0x02);
        a.push(0x03);
        b.push(0x04);
        let al = align(&a, &b);
        assert!(al.degraded, "an over-budget middle degrades");
        assert_reconstructs(&a, &b, &al);
        assert!(
            al.segments.iter().any(|s| s.kind == SegmentKind::Equal),
            "anchoring still finds equal blocks"
        );
    }
}

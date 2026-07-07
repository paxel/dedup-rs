//! Grouping throughput for image fingerprints.
//!
//! Phase 4 target: 50k random image hashes group in well under a second.

use criterion::{Criterion, criterion_group, criterion_main};
use dedup_core::similar::group_img;
use dedup_core::store::ImgHash;
use std::hint::black_box;

/// Deterministic xorshift so the bench is reproducible without an rng crate.
fn random_fingerprints(n: usize) -> Vec<ImgHash> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..n).map(|_| std::array::from_fn(|_| next())).collect()
}

fn bench_group_50k(c: &mut Criterion) {
    let fps = random_fingerprints(50_000);
    c.bench_function("group_img_50k_random_t90", |b| {
        b.iter(|| black_box(group_img(black_box(&fps), 90.0)))
    });
}

criterion_group!(benches, bench_group_50k);
criterion_main!(benches);

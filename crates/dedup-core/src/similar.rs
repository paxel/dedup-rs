//! Similarity grouping over perceptual fingerprints, ported from the legacy
//! `findSimilar`/`groupByHamming` logic.
//!
//! Threshold semantics match Java exactly: two fingerprints are "similar" when
//! `similarity % = (1 - distance/bits) * 100 >= threshold`, where `distance` is
//! the Hamming distance (`count_ones` of the XOR).
//!
//! Image fingerprints ([`ImgHash`], 512 bits) are candidate-pruned with LSH
//! banding — the hash is split into 32 16-bit bands and only items sharing at
//! least one band are compared. By the pigeonhole principle this is exact for
//! distances ≤ 31 (similarity ≥ ~94 %) and a fast approximation below that; it
//! keeps grouping of tens of thousands of images near-linear instead of O(n²).
//! Audio (Chromaprint fingerprints, see [`similarity_audio`]) is pruned by
//! duration: only files within 2 s of each other are compared, which an
//! audiobook library of tens of thousands of chapters needs as much as images
//! need banding. Video/PDF populations are small, so those group by direct scan.

use crate::dupes::{DupeFile, DupeGroup, sort_groups};
use crate::filter::{AnnotatedFilter, FileFilter};
use crate::fingerprint::chromaprint_config;
use crate::store::{self, FileEntry, ImgHash, Store, StoreError};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const IMG_BITS: f64 = 512.0;
/// 16-bit LSH bands per image hash: 512 / 16.
const IMG_BANDS: u8 = 32;
const VIDEO_BITS: f64 = 1536.0;
/// Audio duration tolerance when matching, in milliseconds. It keeps the
/// comparison to a sliding window over duration-sorted files instead of every
/// pair. A plain re-encode lands well inside it; a first or last chapter whose
/// two editions trim their trailer differently (3–5 s apart in practice) does
/// not, and is missed on purpose rather than widening every window.
const AUDIO_DURATION_TOLERANCE_MS: u32 = 2000;

/// Image-hash similarity as a percentage in `0.0..=100.0`.
pub fn similarity_img(a: &ImgHash, b: &ImgHash) -> f64 {
    let distance: u32 = (0..8).map(|k| (a[k] ^ b[k]).count_ones()).sum();
    (1.0 - f64::from(distance) / IMG_BITS) * 100.0
}

/// 1536-bit temporal-hash similarity (three 512-bit frame hashes) as a
/// percentage in `0.0..=100.0`.
pub fn similarity_video(a: &[ImgHash; 3], b: &[ImgHash; 3]) -> f64 {
    let distance: u32 = (0..3)
        .flat_map(|f| (0..8).map(move |w| (f, w)))
        .map(|(f, w)| (a[f][w] ^ b[f][w]).count_ones())
        .sum();
    (1.0 - f64::from(distance) / VIDEO_BITS) * 100.0
}

/// Similarity of two acoustic fingerprints as a percentage in `0.0..=100.0`,
/// on the same footing as the image/video Hamming similarity: Chromaprint
/// aligns the two streams and reports the mean bit error (0–32) over the
/// matched stretch, and `similarity = (1 - error/32) * 100`.
///
/// Only a match that covers most of the shorter fingerprint counts: two files
/// sharing a 5-second jingle are not the same recording. Anything else — no
/// alignment, an empty fingerprint — is 0.
pub fn similarity_audio(a: &[u32], b: &[u32]) -> f64 {
    /// The share of the shorter fingerprint the aligned stretch must span.
    const MIN_COVERAGE: f64 = 0.8;
    const BITS: f64 = 32.0;
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let Ok(segments) = rusty_chromaprint::match_fingerprints(a, b, &chromaprint_config()) else {
        return 0.0;
    };
    // Segments of one alignment (same offset delta) are the same stretch cut
    // by a few dissimilar items; their bit errors combine item-weighted.
    let mut by_delta: std::collections::HashMap<i64, (usize, f64)> =
        std::collections::HashMap::new();
    for seg in &segments {
        let delta = seg.offset1 as i64 - seg.offset2 as i64;
        let slot = by_delta.entry(delta).or_insert((0, 0.0));
        slot.0 += seg.items_count;
        slot.1 += seg.score * seg.items_count as f64;
    }
    let Some((items, weighted)) = by_delta.values().copied().max_by(|x, y| x.0.cmp(&y.0)) else {
        return 0.0;
    };
    let shorter = a.len().min(b.len()) as f64;
    if items == 0 || (items as f64) < shorter * MIN_COVERAGE {
        return 0.0;
    }
    let error = (weighted / items as f64).clamp(0.0, BITS);
    (1.0 - error / BITS) * 100.0
}

/// The `b`-th 16-bit band of an image hash.
fn img_band(fp: &ImgHash, b: u8) -> u16 {
    (fp[usize::from(b) / 4] >> ((u32::from(b) % 4) * 16)) as u16
}

/// Greedily group image hashes whose similarity meets `threshold`, returning
/// groups of indices into `fingerprints` (singletons excluded).
///
/// Candidates are pruned via 16-bit LSH banding; see the module docs for the
/// exactness bound.
pub fn group_img(fingerprints: &[ImgHash], threshold: f64) -> Vec<Vec<usize>> {
    // Band index: (band, 16-bit value) -> item indices sharing that band.
    let mut bands: HashMap<(u8, u16), Vec<usize>> = HashMap::new();
    for (idx, fp) in fingerprints.iter().enumerate() {
        for b in 0..IMG_BANDS {
            bands.entry((b, img_band(fp, b))).or_default().push(idx);
        }
    }

    let mut handled = vec![false; fingerprints.len()];
    let mut groups = Vec::new();
    for i in 0..fingerprints.len() {
        if handled[i] {
            continue;
        }
        handled[i] = true;
        let mut group = vec![i];

        // Gather not-yet-handled candidates that share a band with i.
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        for b in 0..IMG_BANDS {
            if let Some(bucket) = bands.get(&(b, img_band(&fingerprints[i], b))) {
                for &j in bucket {
                    if j > i && !handled[j] && seen.insert(j) {
                        candidates.push(j);
                    }
                }
            }
        }
        candidates.sort_unstable();

        for j in candidates {
            if handled[j] {
                continue;
            }
            if similarity_img(&fingerprints[i], &fingerprints[j]) >= threshold {
                group.push(j);
                handled[j] = true;
            }
        }
        if group.len() > 1 {
            groups.push(group);
        }
    }
    groups
}

/// Greedily group items by a similarity predicate with a direct O(n²) scan.
/// Suited to small populations (video/audio/PDF).
fn group_by<T, F>(items: &[T], similar: F) -> Vec<Vec<usize>>
where
    F: Fn(&T, &T) -> bool,
{
    let mut handled = vec![false; items.len()];
    let mut groups = Vec::new();
    for i in 0..items.len() {
        if handled[i] {
            continue;
        }
        handled[i] = true;
        let mut group = vec![i];
        for j in (i + 1)..items.len() {
            if handled[j] {
                continue;
            }
            if similar(&items[i], &items[j]) {
                group.push(j);
                handled[j] = true;
            }
        }
        if group.len() > 1 {
            groups.push(group);
        }
    }
    groups
}

/// Greedily group audio fingerprints whose similarity meets `threshold`,
/// comparing only files whose durations lie within
/// [`AUDIO_DURATION_TOLERANCE_MS`] of each other: items are sorted by duration
/// and each is matched against the run that follows it inside the window.
/// Same greedy semantics as [`group_by`]; returns groups of indices into
/// `items` (singletons excluded).
fn group_audio(items: &[Candidate<(u32, Vec<u32>)>], threshold: f64) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].key.0);
    let mut handled = vec![false; items.len()];
    let mut groups = Vec::new();
    for (pos, &i) in order.iter().enumerate() {
        if handled[i] {
            continue;
        }
        handled[i] = true;
        let mut group = vec![i];
        for &j in &order[pos + 1..] {
            if items[j].key.0.abs_diff(items[i].key.0) > AUDIO_DURATION_TOLERANCE_MS {
                break;
            }
            if handled[j] {
                continue;
            }
            if similarity_audio(&items[i].key.1, &items[j].key.1) >= threshold {
                group.push(j);
                handled[j] = true;
            }
        }
        if group.len() > 1 {
            groups.push(group);
        }
    }
    groups
}

/// One file staged for similarity comparison: a lightweight locator plus its
/// fingerprint. The full [`FileEntry`] is fetched only for files that end up in
/// a group (see [`materialize`]), so staging holds ~8-byte keys per media file
/// instead of a cloned entry each.
struct Candidate<K> {
    repo_idx: usize,
    rel_path: String,
    key: K,
}

/// Non-missing media entries across `repo_names`, bucketed by kind and
/// deduplicated by absolute path. `names`/`roots` are parallel and indexed by
/// each candidate's `repo_idx`.
struct Staged {
    names: Vec<String>,
    roots: Vec<String>,
    images: Vec<Candidate<ImgHash>>,
    videos: Vec<Candidate<[ImgHash; 3]>>,
    pdfs: Vec<Candidate<[u8; 32]>>,
    audios: Vec<Candidate<(u32, Vec<u32>)>>,
}

fn stage(store: &Store, repo_names: &[String]) -> Result<Staged, StoreError> {
    let mut staged = Staged {
        names: repo_names.to_vec(),
        roots: Vec::with_capacity(repo_names.len()),
        images: Vec::new(),
        videos: Vec::new(),
        pdfs: Vec::new(),
        audios: Vec::new(),
    };
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for (repo_idx, name) in repo_names.iter().enumerate() {
        let meta = store.get_repo(name)?;
        staged.roots.push(meta.abs_path.clone());
        let db = store.open_repo_db(name)?;
        store::for_each_file_entry(&db, |rel_path, entry| {
            if entry.missing {
                return Ok(());
            }
            let abs = Path::new(&meta.abs_path).join(rel_path);
            if !seen.insert(abs) {
                return Ok(());
            }
            if let Some(fp) = entry.img_fingerprint {
                staged.images.push(Candidate {
                    repo_idx,
                    rel_path: rel_path.to_string(),
                    key: fp,
                });
            }
            if let Some(vh) = entry.video_hash {
                staged.videos.push(Candidate {
                    repo_idx,
                    rel_path: rel_path.to_string(),
                    key: vh,
                });
            }
            if let Some(ph) = entry.pdf_hash {
                staged.pdfs.push(Candidate {
                    repo_idx,
                    rel_path: rel_path.to_string(),
                    key: ph,
                });
            }
            // An empty fingerprint (undecodable, or indexed before v10 and
            // not yet re-scanned) has nothing to compare; staging it would
            // group every such file of a similar length together.
            if let Some(af) = entry.audio
                && !af.fingerprint.is_empty()
            {
                staged.audios.push(Candidate {
                    repo_idx,
                    rel_path: rel_path.to_string(),
                    key: (af.duration_ms, af.fingerprint),
                });
            }
            Ok(())
        })?;
    }
    Ok(staged)
}

/// Build [`DupeFile`]s for the grouped candidates, fetching each member's
/// [`FileEntry`] from its repo DB (`dbs` is parallel to `staged.names`).
fn materialize<K>(
    dbs: &[std::sync::Arc<redb::Database>],
    staged: &Staged,
    candidates: &[Candidate<K>],
    index_groups: Vec<Vec<usize>>,
) -> Result<Vec<DupeGroup>, StoreError> {
    let mut out = Vec::with_capacity(index_groups.len());
    for indices in index_groups {
        let mut group: DupeGroup = Vec::with_capacity(indices.len());
        for i in indices {
            let c = &candidates[i];
            if let Some(entry) = store::get_entry(&dbs[c.repo_idx], &c.rel_path)? {
                group.push(DupeFile {
                    repo: staged.names[c.repo_idx].clone(),
                    repo_root: staged.roots[c.repo_idx].clone(),
                    rel_path: c.rel_path.clone(),
                    entry,
                });
            }
        }
        out.push(group);
    }
    Ok(out)
}

/// Find similar-file groups across the given repos at the given threshold
/// percentage. Images/videos group by Hamming similarity, PDFs by exact text
/// hash, audio by acoustic similarity among files within 2 s of each other in
/// duration. Groups are sorted like exact duplicates (best copy first, wasted
/// bytes descending).
pub fn find_similar(
    store: &Store,
    repo_names: &[String],
    threshold: f64,
    filter: Option<&FileFilter>,
) -> Result<Vec<DupeGroup>, StoreError> {
    let staged = stage(store, repo_names)?;

    // Open each repo DB once; grouped members' entries are fetched from these.
    let mut dbs: Vec<std::sync::Arc<redb::Database>> = Vec::with_capacity(staged.names.len());
    for name in &staged.names {
        dbs.push(store.open_repo_db(name)?);
    }

    let mut groups: Vec<DupeGroup> = Vec::new();

    let img_fps: Vec<ImgHash> = staged.images.iter().map(|c| c.key).collect();
    let img_groups = group_img(&img_fps, threshold);
    groups.extend(materialize(&dbs, &staged, &staged.images, img_groups)?);

    let video_groups = group_by(&staged.videos, |a, b| {
        similarity_video(&a.key, &b.key) >= threshold
    });
    groups.extend(materialize(&dbs, &staged, &staged.videos, video_groups)?);

    let pdf_groups = group_by(&staged.pdfs, |a, b| a.key == b.key);
    groups.extend(materialize(&dbs, &staged, &staged.pdfs, pdf_groups)?);

    let audio_groups = group_audio(&staged.audios, threshold);
    groups.extend(materialize(&dbs, &staged, &staged.audios, audio_groups)?);

    // Keep only groups with at least one member matching the filter (a whole
    // group is shown when any copy matches). Annotations are per repo, so build
    // one matcher per repo and route each member to its repo's matcher.
    if let Some(filter) = filter {
        let mut matchers: HashMap<&str, AnnotatedFilter> = HashMap::with_capacity(dbs.len());
        for (name, db) in staged.names.iter().zip(dbs.iter()) {
            matchers.insert(name.as_str(), AnnotatedFilter::new(db, filter)?);
        }
        groups.retain(|group| {
            group.iter().any(|f| {
                matchers
                    .get(f.repo.as_str())
                    .is_some_and(|m| m.matches(&f.rel_path, &f.entry))
            })
        });
    }

    sort_groups(&mut groups);
    Ok(groups)
}

/// Test-only: which of a [`FileEntry`]'s fingerprints are populated. Handy for
/// asserting update wired fingerprints through without touching redb internals.
pub fn fingerprint_kinds(entry: &FileEntry) -> (bool, bool, bool, bool) {
    (
        entry.img_fingerprint.is_some(),
        entry.video_hash.is_some(),
        entry.pdf_hash.is_some(),
        entry.audio.is_some(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::fingerprint_pcm;

    /// A synthetic "recording": a few seconds of shifting tone mixture with a
    /// deterministic noise floor, as interleaved mono 16-bit PCM at 11025 Hz.
    fn take(seed: u32, secs: u32) -> Vec<i16> {
        let rate = 11025u32;
        let mut noise = seed.wrapping_mul(2_654_435_761) | 1;
        (0..rate * secs)
            .map(|n| {
                let t = f64::from(n) / f64::from(rate);
                let base = 150.0 + f64::from(seed % 7) * 37.0;
                let wobble = (t * 0.6).sin() * 30.0;
                let s = 0.4 * (2.0 * std::f64::consts::PI * (base + wobble) * t).sin()
                    + 0.3
                        * (2.0 * std::f64::consts::PI * (base * 2.5) * t).sin()
                        * (0.5 + 0.5 * (t * 1.7).sin())
                    + 0.2 * (2.0 * std::f64::consts::PI * (base * 4.1 + 20.0 * t) * t).sin();
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                let n = (f64::from(noise % 2001) - 1000.0) / 1000.0 * 0.05;
                ((s + n) * 12_000.0) as i16
            })
            .collect()
    }

    #[test]
    fn audio_similarity_is_100_for_identical_and_0_for_unrelated_or_empty() {
        let a = fingerprint_pcm(&take(1, 8), 11025, 1);
        let b = fingerprint_pcm(&take(2, 8), 11025, 1);
        assert!(
            a.len() > 20 && b.len() > 20,
            "{} / {} items",
            a.len(),
            b.len()
        );
        assert_eq!(similarity_audio(&a, &a), 100.0);
        assert_eq!(similarity_audio(&a, &[]), 0.0);
        assert_eq!(similarity_audio(&[], &a), 0.0);
        let unrelated = similarity_audio(&a, &b);
        assert!(unrelated < 60.0, "unrelated takes scored {unrelated}");
    }

    #[test]
    fn audio_similarity_survives_re_encoding_style_damage() {
        // The same take, quieter and with a little extra noise — what a second
        // codec does to a recording — still scores high.
        let original = take(3, 8);
        let mut damaged = original.clone();
        let mut noise = 12345u32;
        for s in &mut damaged {
            noise ^= noise << 13;
            noise ^= noise >> 17;
            noise ^= noise << 5;
            let n = (noise % 401) as i32 - 200;
            *s = ((i32::from(*s) * 7 / 10) + n).clamp(-32768, 32767) as i16;
        }
        let a = fingerprint_pcm(&original, 11025, 1);
        let b = fingerprint_pcm(&damaged, 11025, 1);
        let s = similarity_audio(&a, &b);
        assert!(s > 90.0, "damaged copy scored {s}");
    }

    #[test]
    fn audio_similarity_ignores_a_shared_short_stretch() {
        // Two takes that share only their first ~2 seconds (a jingle) are not
        // the same recording: the aligned stretch covers too little.
        let mut a = take(4, 2);
        a.extend(take(5, 6));
        let mut b = take(4, 2);
        b.extend(take(6, 6));
        let fa = fingerprint_pcm(&a, 11025, 1);
        let fb = fingerprint_pcm(&b, 11025, 1);
        assert_eq!(similarity_audio(&fa, &fb), 0.0);
    }

    #[test]
    fn audio_groups_only_within_the_duration_window() {
        let same = fingerprint_pcm(&take(8, 8), 11025, 1);
        let other = fingerprint_pcm(&take(9, 8), 11025, 1);
        let cand = |ms: u32, fp: &Vec<u32>| Candidate {
            repo_idx: 0,
            rel_path: String::new(),
            key: (ms, fp.clone()),
        };
        let items = vec![
            cand(8_000, &same),  // 0
            cand(9_500, &same),  // 1: same take, 1.5 s off → grouped
            cand(8_200, &other), // 2: unrelated, in the window → not grouped
            cand(20_000, &same), // 3: same take, far outside the window
        ];
        let groups = group_audio(&items, 90.0);
        assert_eq!(groups, vec![vec![0, 1]]);
    }

    #[test]
    fn similarity_is_100_for_equal_and_0_for_inverse() {
        let fp: ImgHash = [0xdead_beef; 8];
        assert_eq!(similarity_img(&fp, &fp), 100.0);
        assert_eq!(similarity_img(&[0; 8], &[u64::MAX; 8]), 0.0);
    }

    #[test]
    fn near_identical_images_group_together() {
        // Base and base with two bits flipped (distance 2, similarity ~99.6%).
        let base: ImgHash = [0x0123_4567_89ab_cdef; 8];
        let mut near = base;
        near[0] ^= 1;
        near[7] ^= 1 << 63;
        let far = base.map(|w| !w);
        let groups = group_img(&[base, near, far], 95.0);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
        assert!(groups[0].contains(&0) && groups[0].contains(&1));
    }

    #[test]
    fn dissimilar_images_do_not_group() {
        let groups = group_img(&[[0u64; 8], [u64::MAX; 8]], 90.0);
        assert!(groups.is_empty());
    }

    #[test]
    fn video_groups_by_temporal_distance() {
        let a = [[0u64; 8]; 3];
        let mut b = [[0u64; 8]; 3];
        b[0][0] = 1; // distance 1 of 1536 → ~99.9%
        assert!(similarity_video(&a, &b) >= 99.0);

        let mut far = [[0u64; 8]; 3];
        far[0] = [u64::MAX; 8]; // one frame entirely different → ~66%
        assert!(similarity_video(&a, &far) < 70.0);
    }
}

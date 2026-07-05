//! Similarity grouping over perceptual fingerprints, ported from the legacy
//! `findSimilar`/`groupByHamming` logic.
//!
//! Threshold semantics match Java exactly: two fingerprints are "similar" when
//! `similarity % = (1 - distance/bits) * 100 >= threshold`, where `distance` is
//! the Hamming distance (`count_ones` of the XOR).
//!
//! Image fingerprints (`u64`) are candidate-pruned with LSH banding — the hash
//! is split into four 16-bit bands and only items sharing at least one band are
//! compared. By the pigeonhole principle this is exact for distances ≤ 3
//! (similarity ≥ ~95 %) and a fast approximation below that; it keeps grouping
//! of tens of thousands of images near-linear instead of O(n²). Video/audio/PDF
//! populations are small, so those group by direct scan.

use crate::dupes::{DupeFile, DupeGroup, sort_groups};
use crate::store::{self, FileEntry, Store, StoreError};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const IMG_BITS: f64 = 64.0;
const VIDEO_BITS: f64 = 192.0;
/// Audio duration tolerance when matching, in milliseconds (Java used 2 s).
const AUDIO_DURATION_TOLERANCE_MS: u32 = 2000;

/// Image dHash similarity as a percentage in `0.0..=100.0`.
pub fn similarity_u64(a: u64, b: u64) -> f64 {
    (1.0 - f64::from((a ^ b).count_ones()) / IMG_BITS) * 100.0
}

/// 192-bit temporal-hash similarity as a percentage in `0.0..=100.0`.
pub fn similarity_192(a: &[u64; 3], b: &[u64; 3]) -> f64 {
    let distance: u32 = (0..3).map(|k| (a[k] ^ b[k]).count_ones()).sum();
    (1.0 - f64::from(distance) / VIDEO_BITS) * 100.0
}

/// Greedily group `u64` fingerprints whose similarity meets `threshold`,
/// returning groups of indices into `fingerprints` (singletons excluded).
///
/// Candidates are pruned via 16-bit LSH banding; see the module docs for the
/// exactness bound.
pub fn group_u64(fingerprints: &[u64], threshold: f64) -> Vec<Vec<usize>> {
    // Band index: (band, 16-bit value) -> item indices sharing that band.
    let mut bands: HashMap<(u8, u16), Vec<usize>> = HashMap::new();
    for (idx, &fp) in fingerprints.iter().enumerate() {
        for b in 0..4u8 {
            let key = ((fp >> (u32::from(b) * 16)) & 0xffff) as u16;
            bands.entry((b, key)).or_default().push(idx);
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
        for b in 0..4u8 {
            let key = ((fingerprints[i] >> (u32::from(b) * 16)) & 0xffff) as u16;
            if let Some(bucket) = bands.get(&(b, key)) {
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
            if similarity_u64(fingerprints[i], fingerprints[j]) >= threshold {
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

/// One file staged for similarity comparison, paired with its fingerprint.
struct Candidate<K> {
    file: DupeFile,
    key: K,
}

/// Collect non-missing entries across `repo_names`, bucketed by media kind and
/// deduplicated by absolute path.
struct Staged {
    images: Vec<Candidate<u64>>,
    videos: Vec<Candidate<[u64; 3]>>,
    pdfs: Vec<Candidate<[u8; 32]>>,
    audios: Vec<Candidate<(u32, Vec<[u8; 32]>)>>,
}

fn stage(store: &Store, repo_names: &[String]) -> Result<Staged, StoreError> {
    let mut staged = Staged {
        images: Vec::new(),
        videos: Vec::new(),
        pdfs: Vec::new(),
        audios: Vec::new(),
    };
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for name in repo_names {
        let meta = store.get_repo(name)?;
        let db = store.open_repo_db(name)?;
        store::for_each_file_entry(&db, |rel_path, entry| {
            if entry.missing {
                return Ok(());
            }
            let abs = Path::new(&meta.abs_path).join(rel_path);
            if !seen.insert(abs) {
                return Ok(());
            }
            let file = || DupeFile {
                repo: name.clone(),
                repo_root: meta.abs_path.clone(),
                rel_path: rel_path.to_string(),
                entry: entry.clone(),
            };
            if let Some(fp) = entry.img_fingerprint {
                staged.images.push(Candidate {
                    file: file(),
                    key: fp,
                });
            }
            if let Some(vh) = entry.video_hash {
                staged.videos.push(Candidate {
                    file: file(),
                    key: vh,
                });
            }
            if let Some(ph) = entry.pdf_hash {
                staged.pdfs.push(Candidate {
                    file: file(),
                    key: ph,
                });
            }
            if let Some(ref af) = entry.audio {
                staged.audios.push(Candidate {
                    file: file(),
                    key: (af.duration_ms, af.chunk_hashes.clone()),
                });
            }
            Ok(())
        })?;
    }
    Ok(staged)
}

fn materialize<K>(candidates: &[Candidate<K>], index_groups: Vec<Vec<usize>>) -> Vec<DupeGroup> {
    index_groups
        .into_iter()
        .map(|indices| {
            indices
                .into_iter()
                .map(|i| candidates[i].file.clone())
                .collect()
        })
        .collect()
}

/// Find similar-file groups across the given repos at the given threshold
/// percentage. Images/videos group by Hamming similarity, PDFs by exact text
/// hash, audio by chunk hash plus a 2 s duration tolerance. Groups are sorted
/// like exact duplicates (best copy first, wasted bytes descending).
pub fn find_similar(
    store: &Store,
    repo_names: &[String],
    threshold: f64,
) -> Result<Vec<DupeGroup>, StoreError> {
    let staged = stage(store, repo_names)?;

    let mut groups: Vec<DupeGroup> = Vec::new();

    let img_fps: Vec<u64> = staged.images.iter().map(|c| c.key).collect();
    groups.extend(materialize(&staged.images, group_u64(&img_fps, threshold)));

    let video_groups = group_by(&staged.videos, |a, b| {
        similarity_192(&a.key, &b.key) >= threshold
    });
    groups.extend(materialize(&staged.videos, video_groups));

    let pdf_groups = group_by(&staged.pdfs, |a, b| a.key == b.key);
    groups.extend(materialize(&staged.pdfs, pdf_groups));

    let audio_groups = group_by(&staged.audios, |a, b| {
        a.key.1 == b.key.1 && a.key.0.abs_diff(b.key.0) <= AUDIO_DURATION_TOLERANCE_MS
    });
    groups.extend(materialize(&staged.audios, audio_groups));

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

    #[test]
    fn similarity_is_100_for_equal_and_0_for_inverse() {
        assert_eq!(similarity_u64(0xdead_beef, 0xdead_beef), 100.0);
        assert_eq!(similarity_u64(0, u64::MAX), 0.0);
    }

    #[test]
    fn near_identical_images_group_together() {
        // Base and base with one bit flipped (distance 1, similarity ~98.4%).
        let base = 0x0123_4567_89ab_cdefu64;
        let near = base ^ 1;
        let far = !base;
        let groups = group_u64(&[base, near, far], 95.0);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
        assert!(groups[0].contains(&0) && groups[0].contains(&1));
    }

    #[test]
    fn dissimilar_images_do_not_group() {
        let groups = group_u64(&[0u64, u64::MAX], 90.0);
        assert!(groups.is_empty());
    }

    #[test]
    fn video_192_groups_by_temporal_distance() {
        let a = [0u64, 0, 0];
        let b = [1u64, 0, 0]; // distance 1 of 192 → ~99.5%
        assert!(similarity_192(&a, &b) >= 99.0);
    }
}

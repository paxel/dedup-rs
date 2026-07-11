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
//! Video/audio/PDF populations are small, so those group by direct scan.

use crate::dupes::{DupeFile, DupeGroup, sort_groups};
use crate::store::{self, FileEntry, ImgHash, Store, StoreError};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const IMG_BITS: f64 = 512.0;
/// 16-bit LSH bands per image hash: 512 / 16.
const IMG_BANDS: u8 = 32;
const VIDEO_BITS: f64 = 192.0;
/// Audio duration tolerance when matching, in milliseconds (Java used 2 s).
const AUDIO_DURATION_TOLERANCE_MS: u32 = 2000;

/// Image-hash similarity as a percentage in `0.0..=100.0`.
pub fn similarity_img(a: &ImgHash, b: &ImgHash) -> f64 {
    let distance: u32 = (0..8).map(|k| (a[k] ^ b[k]).count_ones()).sum();
    (1.0 - f64::from(distance) / IMG_BITS) * 100.0
}

/// 192-bit temporal-hash similarity as a percentage in `0.0..=100.0`.
pub fn similarity_192(a: &[u64; 3], b: &[u64; 3]) -> f64 {
    let distance: u32 = (0..3).map(|k| (a[k] ^ b[k]).count_ones()).sum();
    (1.0 - f64::from(distance) / VIDEO_BITS) * 100.0
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
    videos: Vec<Candidate<[u64; 3]>>,
    pdfs: Vec<Candidate<[u8; 32]>>,
    audios: Vec<Candidate<(u32, Vec<[u8; 32]>)>>,
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
            if let Some(af) = entry.audio {
                staged.audios.push(Candidate {
                    repo_idx,
                    rel_path: rel_path.to_string(),
                    key: (af.duration_ms, af.chunk_hashes),
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
/// hash, audio by chunk hash plus a 2 s duration tolerance. Groups are sorted
/// like exact duplicates (best copy first, wasted bytes descending).
pub fn find_similar(
    store: &Store,
    repo_names: &[String],
    threshold: f64,
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
        similarity_192(&a.key, &b.key) >= threshold
    });
    groups.extend(materialize(&dbs, &staged, &staged.videos, video_groups)?);

    let pdf_groups = group_by(&staged.pdfs, |a, b| a.key == b.key);
    groups.extend(materialize(&dbs, &staged, &staged.pdfs, pdf_groups)?);

    let audio_groups = group_by(&staged.audios, |a, b| {
        a.key.1 == b.key.1 && a.key.0.abs_diff(b.key.0) <= AUDIO_DURATION_TOLERANCE_MS
    });
    groups.extend(materialize(&dbs, &staged, &staged.audios, audio_groups)?);

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
    fn video_192_groups_by_temporal_distance() {
        let a = [0u64, 0, 0];
        let b = [1u64, 0, 0]; // distance 1 of 192 → ~99.5%
        assert!(similarity_192(&a, &b) >= 99.0);
    }
}

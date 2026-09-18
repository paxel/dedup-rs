//! End-to-end: `update` fingerprints audio acoustically, and `find_similar`
//! groups the same recording across codecs — an MP3 and an AAC of one take —
//! while an unrelated take of the same length stays out.
//!
//! The fixtures are 8-second synthetic signals (tone mixtures plus seeded
//! noise) rendered once with ffmpeg, so no decoder tool is needed at test time
//! and nothing copyrighted lives in the repository.

use dedup_core::similar::{SimilarPhase, SimilarProgress, find_similar, find_similar_reporting};
use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("audio")
        .join(name)
}

#[test]
fn same_take_across_codecs_groups_and_another_take_does_not() {
    let config = tempfile::tempdir().expect("config dir");
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let store = Store::open_at(config.path().to_path_buf()).expect("open store");

    for name in ["same_take.mp3", "same_take.m4a", "other_take.mp3"] {
        std::fs::copy(fixture(name), repo_dir.path().join(name)).expect("copy fixture");
    }
    store
        .create_repo("audio", &repo_dir.path().to_string_lossy())
        .expect("create repo");
    let stats =
        update_repo(&store, "audio", 2, &NoProgress, &CancellationToken::new()).expect("update");
    assert_eq!(stats.added, 3);

    // Every file decoded to a real fingerprint of its (8 s) length.
    for name in ["same_take.mp3", "same_take.m4a", "other_take.mp3"] {
        let entry = store
            .get_file_entry("audio", name)
            .expect("read entry")
            .unwrap_or_else(|| panic!("{name} indexed"));
        let audio = entry
            .audio
            .unwrap_or_else(|| panic!("{name} has audio facts"));
        assert!(
            (7_500..=8_500).contains(&audio.duration_ms),
            "{name}: duration {} ms",
            audio.duration_ms
        );
        assert!(
            audio.fingerprint.len() > 20,
            "{name}: fingerprint has {} items",
            audio.fingerprint.len()
        );
    }

    let groups = find_similar(&store, &["audio".to_string()], 90.0, None).expect("similar");
    assert_eq!(groups.len(), 1, "one group: {groups:?}");
    let mut members: Vec<&str> = groups[0].iter().map(|f| f.rel_path.as_str()).collect();
    members.sort_unstable();
    assert_eq!(members, ["same_take.m4a", "same_take.mp3"]);
}

/// The reporting search names each phase in order, the audio phase counts its
/// pairs up to the total, and a cancelled token ends the search with nothing
/// rather than a half-result.
#[test]
fn reporting_search_walks_its_phases_and_honours_cancel() {
    let config = tempfile::tempdir().expect("config dir");
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let store = Store::open_at(config.path().to_path_buf()).expect("open store");
    for name in ["same_take.mp3", "same_take.m4a", "other_take.mp3"] {
        std::fs::copy(fixture(name), repo_dir.path().join(name)).expect("copy fixture");
    }
    store
        .create_repo("audio", &repo_dir.path().to_string_lossy())
        .expect("create repo");
    update_repo(&store, "audio", 2, &NoProgress, &CancellationToken::new()).expect("update");

    let seen: Mutex<Vec<SimilarProgress>> = Mutex::new(Vec::new());
    let record = |p: SimilarProgress| seen.lock().expect("record").push(p);
    let groups = find_similar_reporting(
        &store,
        &["audio".to_string()],
        90.0,
        None,
        &record,
        &CancellationToken::new(),
    )
    .expect("similar");
    assert_eq!(groups.len(), 1);

    let seen = seen.into_inner().expect("seen");
    let phases: Vec<&SimilarPhase> = seen.iter().map(|p| &p.phase).collect();
    let mut order: Vec<&SimilarPhase> = Vec::new();
    for phase in phases {
        if order.last() != Some(&phase) {
            order.push(phase);
        }
    }
    assert_eq!(
        order,
        vec![
            &SimilarPhase::Loading {
                repo: "audio".to_string()
            },
            &SimilarPhase::Images,
            &SimilarPhase::Videos,
            &SimilarPhase::Documents,
            &SimilarPhase::Audio,
        ]
    );
    let audio_last = seen
        .iter()
        .rfind(|p| p.phase == SimilarPhase::Audio)
        .expect("audio phase reported");
    assert_eq!(audio_last.done, audio_last.total, "{audio_last:?}");
    assert!(
        audio_last.total >= 1,
        "the three files yield at least one pair"
    );

    let cancel = CancellationToken::new();
    cancel.cancel();
    let none = find_similar_reporting(&store, &["audio".to_string()], 90.0, None, &|_| {}, &cancel)
        .expect("cancelled search still returns");
    assert!(none.is_empty(), "a cancelled search yields nothing");
}

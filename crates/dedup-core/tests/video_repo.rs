//! End-to-end video fingerprinting through `update`, exercised only when
//! ffmpeg is installed (skipped cleanly otherwise so CI without it still
//! passes). Generates a short clip with ffmpeg's test source, then asserts the
//! temporal hash was computed and persisted.

use dedup_core::fingerprint::ffmpeg_available;
use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::Path;
use std::process::Command;

fn generate_testsrc(path: &Path) -> bool {
    Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=duration=2:size=64x64:rate=5")
        .args(["-pix_fmt", "yuv420p"])
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn update_computes_video_temporal_hash() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }

    let config = tempfile::tempdir().expect("config dir");
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let video_path = repo_dir.path().join("clip.mp4");
    if !generate_testsrc(&video_path) {
        eprintln!("skipping: ffmpeg could not generate the test clip");
        return;
    }

    let store = Store::open_at(config.path().to_path_buf()).expect("open store");
    store
        .create_repo("vids", &repo_dir.path().to_string_lossy())
        .expect("create repo");
    let stats =
        update_repo(&store, "vids", 1, &NoProgress, &CancellationToken::new()).expect("update");
    assert_eq!(stats.added, 1);

    let entry = store
        .get_file_entry("vids", "clip.mp4")
        .expect("get clip.mp4")
        .expect("clip.mp4 entry");
    assert_eq!(entry.mime.as_deref(), Some("video/mp4"));
    let hash = entry.video_hash.expect("temporal hash computed");
    // testsrc's moving pattern gives distinct, non-zero frame hashes.
    assert!(
        hash.iter().flatten().any(|&w| w != 0),
        "at least one sampled frame produced a non-zero hash"
    );
}

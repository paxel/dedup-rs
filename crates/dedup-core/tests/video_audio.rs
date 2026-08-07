//! Video soundtrack extraction and pitch-preserving rate rendering, exercised
//! only when ffmpeg is installed (skipped cleanly otherwise, like
//! `video_repo.rs`). Generates a clip with a sine soundtrack and a silent
//! clip, then asserts the extracted WAVs are real, decodable audio of the
//! expected length — external behavior, not call order.

use dedup_core::fingerprint::{ffmpeg_available, has_audio_track, media_duration_secs};
use dedup_core::thumbnail::{
    audio_wav_path, ensure_audio_rate, ensure_video_audio, rate_wav_path, set_cache_dir,
};
use std::path::Path;
use std::process::Command;

/// A 2-second test clip with a 440 Hz sine soundtrack.
fn generate_clip_with_audio(path: &Path) -> bool {
    Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=duration=2:size=64x64:rate=5")
        .args(["-f", "lavfi", "-i"])
        .arg("sine=frequency=440:duration=2")
        .args(["-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A 2-second test clip with no audio stream at all.
fn generate_silent_clip(path: &Path) -> bool {
    Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=duration=2:size=64x64:rate=5")
        .args(["-pix_fmt", "yuv420p"])
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// One test rather than several: the cache directory override is
/// process-global, so parallel tests must not point it at different places.
#[test]
fn extracts_soundtracks_and_renders_pitch_preserving_rates() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let with_audio = dir.path().join("sound.mp4");
    let silent = dir.path().join("silent.mp4");
    if !generate_clip_with_audio(&with_audio) || !generate_silent_clip(&silent) {
        eprintln!("skipping: ffmpeg could not generate the test clips");
        return;
    }
    set_cache_dir(dir.path().join("thumbs"));

    // The track probe tells the two clips apart — it is what decides whether
    // the viewer offers a soundtrack at all.
    assert!(
        has_audio_track(&with_audio),
        "sine clip has an audio stream"
    );
    assert!(!has_audio_track(&silent), "silent clip has none");

    // Extraction yields a real, decodable WAV of the clip's length, cached
    // under the content hash.
    let wav = ensure_video_audio(&with_audio, "cafe01").expect("extract soundtrack");
    assert_eq!(wav, audio_wav_path("cafe01"), "cached at the keyed path");
    let secs = media_duration_secs(&wav).expect("extracted WAV decodes");
    assert!(
        (secs - 2.0).abs() < 0.5,
        "soundtrack is the clip's length, got {secs}s"
    );

    // Idempotent: a second call reuses the file instead of re-extracting.
    let before = std::fs::metadata(&wav)
        .expect("meta")
        .modified()
        .expect("mtime");
    let again = ensure_video_audio(&with_audio, "cafe01").expect("reuse");
    assert_eq!(again, wav);
    let after = std::fs::metadata(&wav)
        .expect("meta")
        .modified()
        .expect("mtime");
    assert_eq!(before, after, "cached WAV was reused, not rewritten");

    // A clip with no audio stream yields no track — and no stray cache file.
    assert!(
        ensure_video_audio(&silent, "cafe02").is_err(),
        "silent clip extracts no soundtrack"
    );
    assert!(
        !audio_wav_path("cafe02").exists(),
        "a failed extraction leaves nothing behind"
    );

    // Rate rendering: 0.5× doubles the runtime (pitch-preserving time stretch,
    // not a resample) …
    let half = ensure_audio_rate(&wav, "cafe01", 50).expect("render 0.5×");
    assert_eq!(half, rate_wav_path("cafe01", 50));
    let secs = media_duration_secs(&half).expect("0.5× WAV decodes");
    assert!((secs - 4.0).abs() < 0.8, "0.5× runs ~4s, got {secs}s");

    // … and 0.25× — the chained `atempo=0.5,atempo=0.5` case — quadruples it.
    let quarter = ensure_audio_rate(&wav, "cafe01", 25).expect("render 0.25×");
    let secs = media_duration_secs(&quarter).expect("0.25× WAV decodes");
    assert!((secs - 8.0).abs() < 1.5, "0.25× runs ~8s, got {secs}s");
}

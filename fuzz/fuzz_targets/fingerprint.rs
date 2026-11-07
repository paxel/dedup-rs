//! Every in-process parser the scan and the viewer run on a file, by type: the
//! first byte picks the extension (MIME detection reads it), the rest is the
//! file. External tools stay off (`ffmpeg_available = false`).
//!
//! `compute` and `extract_document_text` contain a parser panic (the file just
//! gets no fingerprint), so a panic caught there is handled, not a finding: the
//! silent hook lets it unwind to that guard. A panic that escapes the guards
//! still aborts and is reported, as are hangs, OOMs and aborts.
#![no_main]

use libfuzzer_sys::fuzz_target;

const EXTENSIONS: [&str; 18] = [
    "jpg", "png", "gif", "webp", "tiff", "bmp", "ico", "pdf", "docx", "xlsx", "odt", "xls", "eml",
    "txt", "mp3", "flac", "wav", "ogg",
];

fuzz_target!(init: { std::panic::set_hook(Box::new(|_| {})); }, |data: &[u8]| {
    let Some((&pick, body)) = data.split_first() else {
        return;
    };
    let ext = EXTENSIONS
        .get(usize::from(pick) % EXTENSIONS.len())
        .copied()
        .unwrap_or("bin");
    let Ok(dir) = tempfile::tempdir() else {
        return;
    };
    let path = dir.path().join(format!("input.{ext}"));
    if std::fs::write(&path, body).is_err() {
        return;
    }
    let fp = dedup_core::fingerprint::compute(&path, false);
    if let Some(mime) = fp.mime.as_deref() {
        let _ = dedup_core::fingerprint::extract_document_text(&path, mime);
    }
});

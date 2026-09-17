# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.7.0] - Unreleased

### Changed

- **Audio similarity now hears the recording, not the file.** The audio
  fingerprint used to be a hash of the first 100 KiB of the *encoded* stream,
  which no two codecs ever share — so an audiobook chapter as MP3 and the same
  chapter as M4B could never be found similar. Audio files now carry a
  Chromaprint acoustic fingerprint of their first two minutes of *decoded*
  sound, and similarity search groups the same recording across codecs,
  bitrates and containers (MP3, AAC/M4A/M4B, FLAC, Vorbis, WAV, ALAC built in;
  anything else — Opus, WMA, … — through `ffmpeg` when it is installed). Only
  files within 2 s of each other in duration are compared, and a match must
  cover most of both openings — a shared intro jingle is not a duplicate. The
  threshold slider governs audio like images: 100 % is a bit-exact match, and
  the score falls with the mean bit error of the aligned fingerprints.
- **Audio files re-fingerprint on the next scan.** The old chunk hash is
  dropped from the index (the stored duration survives); every audio file is
  re-read once, its first two minutes decoded, on the first update after the
  upgrade. Until a repository has been rescanned its audio does not take part
  in similarity search at all — nothing is grouped by the stale value.

### Fixed

- **HE-AAC audiobooks report their real length.** An `.m4b`/`.m4a` whose
  container runs at a different clock than its codec (HE-AAC at 22.05 kHz)
  was indexed at half its duration; the length now comes from the
  container's own time base, so such files show the right duration and fall
  inside the 2 s window their other editions are compared in.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

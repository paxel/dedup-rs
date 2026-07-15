//! ID3 tag read/write for the audio lightbox (Phase 6.5). Reads the common
//! text frames for display, and writes edited values back with the `id3` crate
//! (ID3v2, i.e. MP3/WAV/AIFF) — a lossless metadata-only change to the file.
//! Saving is confirmed like every lightbox write (Phase 6.6).

use id3::TagLike;
use std::path::Path;

/// The common, editable ID3 text fields. Kept as strings so the editor can bind
/// text boxes directly; numeric fields are parsed on write.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub year: String,
    pub track: String,
    pub genre: String,
}

/// Read the common ID3 tags, trying ID3v2 first and falling back to ID3v1 (the
/// 128-byte trailer used by older/ripped MP3s). `None` if the file has neither
/// tag or is not an ID3-capable container (e.g. FLAC/OGG).
pub fn read(path: &Path) -> Option<Tags> {
    let tag = id3::v1v2::read_from_path(path).ok()?;
    Some(Tags {
        title: tag.title().unwrap_or_default().to_string(),
        artist: tag.artist().unwrap_or_default().to_string(),
        album: tag.album().unwrap_or_default().to_string(),
        year: tag.year().map(|y| y.to_string()).unwrap_or_default(),
        track: tag.track().map(|t| t.to_string()).unwrap_or_default(),
        genre: tag.genre().unwrap_or_default().to_string(),
    })
}

/// Write the common tags back as a modern ID3v2.4 tag, preserving any other v2
/// frames (e.g. album art) already present. Any legacy ID3v1 trailer is removed
/// so it can't shadow the edit with stale values. Empty fields clear that frame;
/// the audio data is untouched. `path` must be an ID3-capable container.
pub fn write(path: &Path, tags: &Tags) -> Result<(), String> {
    let mut tag = id3::Tag::read_from_path(path).unwrap_or_default();

    let set = |tag: &mut id3::Tag, val: &str, id: &'static str| {
        if val.trim().is_empty() {
            tag.remove(id);
        } else {
            tag.set_text(id, val.trim());
        }
    };
    set(&mut tag, &tags.title, "TIT2");
    set(&mut tag, &tags.artist, "TPE1");
    set(&mut tag, &tags.album, "TALB");
    set(&mut tag, &tags.genre, "TCON");

    match tags.year.trim().parse::<i32>() {
        Ok(y) => tag.set_year(y),
        Err(_) => {
            tag.remove_year();
        }
    }
    match tags.track.trim().parse::<u32>() {
        Ok(t) => tag.set_track(t),
        Err(_) => {
            tag.remove_track();
        }
    }

    // Write v2 *and* strip any ID3v1 trailer (v1v2::write_to_file removes it).
    id3::v1v2::write_to_path(path, &tag, id3::Version::Id3v24).map_err(|e| e.to_string())
}

/// A tiny bare MP3 (one silent MPEG-1 Layer III frame, no tag) that the `id3`
/// crate will treat as MP3 and attach a tag to. Used by tag tests here and in
/// the audio lightbox tests.
#[cfg(test)]
pub(crate) fn write_bare_mp3(path: &Path) {
    let mut bytes = vec![0xFF, 0xFB, 0x90, 0x00];
    bytes.extend(std::iter::repeat_n(0u8, 417 - 4));
    std::fs::write(path, bytes).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("song.mp3");
        write_bare_mp3(&path);

        let tags = Tags {
            title: "Chelsea Hotel".into(),
            artist: "Leonard Cohen".into(),
            album: "New Skin".into(),
            year: "1974".into(),
            track: "5".into(),
            genre: "Folk".into(),
        };
        write(&path, &tags).unwrap();

        let back = read(&path).expect("tags read back");
        assert_eq!(back, tags);
    }

    #[test]
    fn empty_field_clears_that_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.mp3");
        write_bare_mp3(&path);

        write(
            &path,
            &Tags {
                title: "First".into(),
                artist: "Someone".into(),
                ..Default::default()
            },
        )
        .unwrap();
        // Now blank the title; artist stays.
        write(
            &path,
            &Tags {
                title: String::new(),
                artist: "Someone".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let back = read(&path).unwrap();
        assert_eq!(back.title, "", "cleared title is gone");
        assert_eq!(back.artist, "Someone", "artist preserved");
    }

    /// Append a minimal ID3v1 trailer (128 bytes: `TAG` + fixed-width fields).
    fn append_v1(path: &Path, title: &str, artist: &str) {
        let field = |s: &str, n: usize| {
            let mut b = s.as_bytes().to_vec();
            b.resize(n, 0);
            b
        };
        let mut buf = std::fs::read(path).unwrap();
        buf.extend_from_slice(b"TAG");
        buf.extend(field(title, 30));
        buf.extend(field(artist, 30));
        buf.extend(field("", 30)); // album
        buf.extend(field("", 4)); // year
        buf.extend(field("", 30)); // comment
        buf.push(255); // genre (none)
        std::fs::write(path, buf).unwrap();
    }

    #[test]
    fn read_falls_back_to_id3v1() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("v1only.mp3");
        write_bare_mp3(&path); // no ID3v2
        append_v1(&path, "Old Title", "Old Artist");

        let tags = read(&path).expect("ID3v1 tags are read");
        assert_eq!(tags.title, "Old Title");
        assert_eq!(tags.artist, "Old Artist");
    }

    #[test]
    fn writing_v2_strips_the_v1_trailer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("both.mp3");
        write_bare_mp3(&path);
        append_v1(&path, "V1 Title", "V1 Artist");
        assert!(
            id3::v1::Tag::read_from_path(&path).is_ok(),
            "the v1 trailer is present to start"
        );

        write(
            &path,
            &Tags {
                title: "V2 Title".into(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(read(&path).unwrap().title, "V2 Title", "v2 is written");
        assert!(
            id3::v1::Tag::read_from_path(&path).is_err(),
            "the v1 trailer is removed so it can't shadow the edit"
        );
    }
}

//! Printable-string extraction — the `strings(1)`-style view of the readable
//! text embedded in any file's bytes (an image's EXIF, an audio file's tags, an
//! executable's paths). Pure and byte-only; the viewer and the Browse preview
//! both render its output.

/// The printable-ASCII runs of at least `min_run` characters found in `bytes`.
/// A run is a maximal stretch of bytes in `0x20..0x7f` (space through `~`); runs
/// shorter than `min_run` are dropped as noise. At most `max` runs are returned,
/// bounding the work on a large input.
pub fn printable_strings(bytes: &[u8], min_run: usize, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut run = String::new();
    for &b in bytes {
        if (0x20..0x7f).contains(&b) {
            run.push(b as char);
        } else {
            if run.len() >= min_run {
                out.push(std::mem::take(&mut run));
                if out.len() >= max {
                    return out;
                }
            }
            run.clear();
        }
    }
    if run.len() >= min_run && out.len() < max {
        out.push(run);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_runs_and_drops_short_ones() {
        // "Hello" and "World!" are long enough; "ab" (2 chars) is noise.
        let bytes = b"\x00\x01Hello\x00ab\x00World!\xff";
        assert_eq!(
            printable_strings(bytes, 4, 40),
            vec!["Hello".to_string(), "World!".to_string()]
        );
    }

    #[test]
    fn a_trailing_run_at_end_of_input_is_kept() {
        assert_eq!(printable_strings(b"\x00Readable", 4, 40), vec!["Readable"]);
    }

    #[test]
    fn the_cap_bounds_the_number_of_runs() {
        let bytes = b"aaaa\x00bbbb\x00cccc\x00dddd";
        assert_eq!(printable_strings(bytes, 4, 2), vec!["aaaa", "bbbb"]);
    }
}

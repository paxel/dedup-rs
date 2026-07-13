//! File filters for diff and file operations, ported from the legacy
//! `FilterFactory`: `mime:<substring>`, `name:<pattern>`, and
//! `size:<op><bytes>` with the operators `>=`, `<=`, `>`, `<`, `=`
//! (a bare number means equality).
//!
//! `name:` is a plain substring over the relative path unless the value
//! contains a `*`, in which case it is a glob (`*` matches any run of
//! characters, including `/`) anchored to the whole relative path — so
//! `name:*.db` matches paths ending in `.db` and `name:copy_of*` matches paths
//! starting with `copy_of`.

use crate::store::{FileEntry, StoreError, for_each_file_entry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileFilter {
    /// No filter: matches everything.
    All,
    /// Matches entries whose MIME type contains the substring.
    Mime(String),
    /// Matches entries by relative path: a plain substring, or a `*`-glob when
    /// the pattern contains a `*` (see [`glob_match`]).
    Name(String),
    /// Matches entries whose size satisfies the comparison.
    Size(SizeOp, u64),
    /// Matches entries whose provenance (`origin`) contains the substring.
    Origin(String),
    /// Matches entries whose best-known date is >= this epoch-ms.
    TakenAfter(i64),
    /// Matches entries whose best-known date is < this epoch-ms.
    TakenBefore(i64),
    /// Combine multiple filters with AND logic.
    And(Vec<FileFilter>),
}

/// Best-known date of a file: EXIF capture time when present, else file mtime.
/// Epoch milliseconds (naive-local for EXIF; see [`crate::store::ExifInfo`]).
pub fn best_date_ms(entry: &FileEntry) -> i64 {
    entry
        .exif
        .as_ref()
        .and_then(|e| e.taken_ms)
        .unwrap_or(entry.modified_ms)
}

/// Convert a civil date to epoch milliseconds (treated as UTC). Howard
/// Hinnant's days-from-civil algorithm.
pub fn ymd_to_ms(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) * 86400 * 1000
}

/// Convert epoch milliseconds (as UTC) to a civil `(year, month, day)`.
/// Inverse of [`ymd_to_ms`] (Howard Hinnant's civil-from-days).
pub fn ms_to_ymd(ms: i64) -> (i64, u32, u32) {
    let days = ms.div_euclid(86_400_000);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Parse a `YYYY[-MM[-DD]]` date prefix into the half-open epoch-ms span it
/// covers (e.g. `2020` → all of 2020, `2020-03` → that month).
fn parse_date_span(s: &str) -> Option<(i64, i64)> {
    let mut parts = s.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: Option<i64> = parts.next().map(|m| m.parse()).transpose().ok()?;
    let day: Option<i64> = parts.next().map(|d| d.parse()).transpose().ok()?;
    if parts.next().is_some() {
        return None;
    }
    match (month, day) {
        (None, _) => Some((ymd_to_ms(year, 1, 1), ymd_to_ms(year + 1, 1, 1))),
        (Some(m), None) if (1..=12).contains(&m) => {
            let (ny, nm) = if m == 12 {
                (year + 1, 1)
            } else {
                (year, m + 1)
            };
            Some((ymd_to_ms(year, m, 1), ymd_to_ms(ny, nm, 1)))
        }
        (Some(m), Some(d)) if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            let start = ymd_to_ms(year, m, d);
            Some((start, start + 86_400_000))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

#[derive(thiserror::Error, Debug)]
pub enum FilterError {
    #[error(
        "Unknown filter '{0}': expected mime:<substring>, name:<substring>, size:<expr>, or origin:<substring>"
    )]
    UnknownFilter(String),

    #[error("Invalid size filter '{0}': expected an integer byte count, e.g. size:>=1000")]
    InvalidSize(String),

    #[error("Invalid date filter '{0}': expected YYYY[-MM[-DD]]")]
    InvalidDate(String),
}

impl FileFilter {
    /// Parse an optional filter expression; `None` or blank means match-all.
    pub fn parse(filter: Option<&str>) -> Result<Self, FilterError> {
        let Some(filter) = filter else {
            return Ok(Self::All);
        };
        let filter = filter.trim();
        if filter.is_empty() {
            return Ok(Self::All);
        }

        // Split into groups at whitespace-delimited known prefixes, keeping
        // each group's value verbatim (only surrounding whitespace trimmed), so
        // a name/mime substring may itself contain spaces or repeated spaces.
        // Note: because the expression is a flat string, a value that embeds
        // another field's prefix (e.g. `name:report size:big`) is still split
        // into separate filters — keep such tokens out of substring values.
        let mut filters = Vec::new();
        for group in Self::split_groups(filter) {
            filters.push(Self::parse_single(group)?);
        }

        match filters.len() {
            0 => Ok(Self::All),
            1 => Ok(filters.remove(0)),
            _ => Ok(Self::And(filters)),
        }
    }

    /// Split a filter expression into groups, each beginning at a known
    /// `mime:` / `name:` / `size:` prefix found at the start of the string or
    /// immediately after whitespace. The text spanning one prefix to the next
    /// is kept verbatim (only its surrounding whitespace is trimmed), so
    /// substring values are not mangled by internal or repeated spaces. Any
    /// leading text before the first prefix is kept as its own group so
    /// genuinely unknown input is still rejected by `parse_single`.
    fn split_groups(filter: &str) -> Vec<&str> {
        const PREFIXES: [&str; 7] = [
            "mime:", "name:", "size:", "origin:", "date:", "before:", "after:",
        ];
        let bytes = filter.as_bytes();
        let mut starts: Vec<usize> = Vec::new();
        for i in 0..filter.len() {
            if !filter.is_char_boundary(i) {
                continue;
            }
            let at_boundary = i == 0 || bytes[i - 1].is_ascii_whitespace();
            if at_boundary && PREFIXES.iter().any(|p| filter[i..].starts_with(p)) {
                starts.push(i);
            }
        }
        if starts.first() != Some(&0) {
            starts.insert(0, 0);
        }
        let mut groups = Vec::with_capacity(starts.len());
        for (k, &start) in starts.iter().enumerate() {
            let end = starts.get(k + 1).copied().unwrap_or(filter.len());
            groups.push(filter[start..end].trim());
        }
        groups
    }

    fn parse_single(filter: &str) -> Result<Self, FilterError> {
        let filter = filter.trim();
        if filter.is_empty() {
            return Ok(Self::All);
        }
        if let Some(rest) = filter.strip_prefix("mime:") {
            return Ok(Self::Mime(rest.trim().to_string()));
        }
        if let Some(rest) = filter.strip_prefix("name:") {
            return Ok(Self::Name(rest.trim().to_string()));
        }
        if let Some(rest) = filter.strip_prefix("size:") {
            return Self::parse_size(rest.trim());
        }
        if let Some(rest) = filter.strip_prefix("origin:") {
            return Ok(Self::Origin(rest.trim().to_string()));
        }
        if let Some(rest) = filter.strip_prefix("date:") {
            let (start, end) = parse_date_span(rest.trim())
                .ok_or_else(|| FilterError::InvalidDate(rest.trim().to_string()))?;
            return Ok(Self::And(vec![
                Self::TakenAfter(start),
                Self::TakenBefore(end),
            ]));
        }
        if let Some(rest) = filter.strip_prefix("after:") {
            let (start, _) = parse_date_span(rest.trim())
                .ok_or_else(|| FilterError::InvalidDate(rest.trim().to_string()))?;
            return Ok(Self::TakenAfter(start));
        }
        if let Some(rest) = filter.strip_prefix("before:") {
            let (start, _) = parse_date_span(rest.trim())
                .ok_or_else(|| FilterError::InvalidDate(rest.trim().to_string()))?;
            return Ok(Self::TakenBefore(start));
        }
        Err(FilterError::UnknownFilter(filter.to_string()))
    }

    fn parse_size(expression: &str) -> Result<Self, FilterError> {
        let (op, number) = if let Some(rest) = expression.strip_prefix(">=") {
            (SizeOp::Ge, rest)
        } else if let Some(rest) = expression.strip_prefix("<=") {
            (SizeOp::Le, rest)
        } else if let Some(rest) = expression.strip_prefix('>') {
            (SizeOp::Gt, rest)
        } else if let Some(rest) = expression.strip_prefix('<') {
            (SizeOp::Lt, rest)
        } else if let Some(rest) = expression.strip_prefix('=') {
            (SizeOp::Eq, rest)
        } else {
            (SizeOp::Eq, expression)
        };
        let value = number
            .trim()
            .parse::<u64>()
            .map_err(|_| FilterError::InvalidSize(expression.to_string()))?;
        Ok(Self::Size(op, value))
    }

    pub fn matches(&self, rel_path: &str, entry: &FileEntry) -> bool {
        match self {
            Self::All => true,
            Self::Mime(substring) => entry
                .mime
                .as_ref()
                .is_some_and(|mime| mime.contains(substring)),
            Self::Name(pattern) => {
                if pattern.contains('*') {
                    glob_match(pattern, rel_path)
                } else {
                    rel_path.contains(pattern)
                }
            }
            Self::Origin(substring) => entry
                .origin
                .as_ref()
                .is_some_and(|origin| origin.contains(substring)),
            Self::TakenAfter(ms) => best_date_ms(entry) >= *ms,
            Self::TakenBefore(ms) => best_date_ms(entry) < *ms,
            Self::Size(op, value) => match op {
                SizeOp::Lt => entry.size < *value,
                SizeOp::Le => entry.size <= *value,
                SizeOp::Gt => entry.size > *value,
                SizeOp::Ge => entry.size >= *value,
                SizeOp::Eq => entry.size == *value,
            },
            Self::And(filters) => filters.iter().all(|f| f.matches(rel_path, entry)),
        }
    }
}

/// Match `text` against a `*`-glob `pattern`, where `*` matches any run of
/// characters (including `/`). The match is anchored to the whole string: the
/// segments between `*`s must appear in order, the first anchored to the start
/// (unless the pattern begins with `*`) and the last to the end (unless it ends
/// with `*`). Case-sensitive, matching the plain-substring path.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    // With no wildcard the pattern must match the whole string exactly.
    if !pattern.contains('*') {
        return pattern == text;
    }
    // Split on '*'. Consecutive '*'s and leading/trailing '*'s yield empty
    // segments, which impose no constraint.
    let mut segments = pattern.split('*');
    let Some(first) = segments.next() else {
        return true;
    };
    // The part before the first '*' must be a prefix.
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    // Collect the middle/last segments to know which one is last.
    let tail: Vec<&str> = segments.collect();
    for (i, seg) in tail.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i + 1 == tail.len() {
            // Last segment (pattern didn't end with '*'): must be a suffix.
            return rest.ends_with(seg);
        }
        // A middle segment: find its next occurrence and advance past it.
        match rest.find(seg) {
            Some(pos) => rest = &rest[pos + seg.len()..],
            None => return false,
        }
    }
    // Pattern ended with '*' (or had only the prefix): the remainder is free.
    true
}

/// Count the number of present (non-missing) entries in a repo database that
/// satisfy the given filter. Streams the index without materializing it.
pub fn count_matches(db: &redb::Database, filter: &FileFilter) -> Result<usize, StoreError> {
    let mut count = 0usize;
    for_each_file_entry(db, |rel_path, entry| {
        if !entry.missing && filter.matches(rel_path, &entry) {
            count += 1;
        }
        Ok(())
    })?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(size: u64, mime: Option<&str>) -> FileEntry {
        FileEntry {
            size,
            hash: [0; 32],
            modified_ms: 0,
            missing: false,
            mime: mime.map(str::to_string),
            img_fingerprint: None,
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: None,
            origin: None,
            exif: None,
        }
    }

    #[test]
    fn blank_or_absent_matches_all() -> Result<(), FilterError> {
        assert_eq!(FileFilter::parse(None)?, FileFilter::All);
        assert_eq!(FileFilter::parse(Some("  "))?, FileFilter::All);
        assert!(FileFilter::All.matches("x", &entry(1, None)));
        Ok(())
    }

    #[test]
    fn mime_filter_matches_substring() -> Result<(), FilterError> {
        let filter = FileFilter::parse(Some("mime:image"))?;
        assert!(filter.matches("a.png", &entry(1, Some("image/png"))));
        assert!(!filter.matches("a.txt", &entry(1, Some("text/plain"))));
        assert!(!filter.matches("a.bin", &entry(1, None)));
        Ok(())
    }

    #[test]
    fn name_filter_matches_path_substring() -> Result<(), FilterError> {
        let filter = FileFilter::parse(Some("name:sub/"))?;
        assert!(filter.matches("sub/a.txt", &entry(1, None)));
        assert!(!filter.matches("other/a.txt", &entry(1, None)));
        Ok(())
    }

    #[test]
    fn name_filter_supports_wildcards() -> Result<(), FilterError> {
        // Suffix glob: "ends with .db".
        let db = FileFilter::parse(Some("name:*.db"))?;
        assert!(db.matches("data.db", &entry(1, None)));
        assert!(db.matches("a/b/data.db", &entry(1, None)));
        assert!(!db.matches("data.txt", &entry(1, None)));

        // Prefix glob: "starts with copy_of".
        let copy = FileFilter::parse(Some("name:copy_of*"))?;
        assert!(copy.matches("copy_of_report.txt", &entry(1, None)));
        assert!(!copy.matches("report.txt", &entry(1, None)));
        // Anchored to the whole path, so a nested basename needs a leading '*'.
        assert!(!copy.matches("dir/copy_of_x", &entry(1, None)));
        assert!(
            FileFilter::parse(Some("name:*copy_of*"))?.matches("dir/copy_of_x", &entry(1, None))
        );

        // A middle segment between two wildcards.
        let mid = FileFilter::parse(Some("name:*IMG*.jpg"))?;
        assert!(mid.matches("2021/IMG_1234.jpg", &entry(1, None)));
        assert!(!mid.matches("2021/PIC_1234.jpg", &entry(1, None)));
        Ok(())
    }

    #[test]
    fn glob_match_edge_cases() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("abc", "abc"));
        assert!(!glob_match("abc", "abcd")); // no '*' → exact via prefix+suffix
        assert!(glob_match("a*c", "ac"));
        assert!(glob_match("a*c", "abbbc"));
        assert!(!glob_match("a*c", "ab"));
    }

    #[test]
    fn date_filters_use_best_date() -> Result<(), FilterError> {
        use crate::store::ExifInfo;
        // A file with mtime in 2019 but EXIF capture in 2021.
        let mut e = entry(1, Some("image/jpeg"));
        e.modified_ms = ymd_to_ms(2019, 6, 1);
        e.exif = Some(ExifInfo {
            taken_ms: Some(ymd_to_ms(2021, 3, 15)),
            camera: None,
        });
        // best_date is the EXIF date → matches 2021, not 2019.
        assert!(FileFilter::parse(Some("date:2021"))?.matches("p.jpg", &e));
        assert!(!FileFilter::parse(Some("date:2019"))?.matches("p.jpg", &e));
        assert!(FileFilter::parse(Some("date:2021-03"))?.matches("p.jpg", &e));
        assert!(FileFilter::parse(Some("after:2020"))?.matches("p.jpg", &e));
        assert!(FileFilter::parse(Some("before:2022"))?.matches("p.jpg", &e));
        assert!(!FileFilter::parse(Some("before:2021"))?.matches("p.jpg", &e));

        // Without EXIF, the mtime is used.
        e.exif = None;
        assert!(FileFilter::parse(Some("date:2019"))?.matches("p.jpg", &e));

        assert!(FileFilter::parse(Some("date:notadate")).is_err());
        Ok(())
    }

    #[test]
    fn ymd_ms_round_trips() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2021, 3, 15), (2024, 12, 31)] {
            let ms = ymd_to_ms(y, m, d);
            assert_eq!(ms_to_ymd(ms), (y, m as u32, d as u32), "{y}-{m}-{d}");
        }
    }

    #[test]
    fn size_filter_supports_operators() -> Result<(), FilterError> {
        assert!(FileFilter::parse(Some("size:5"))?.matches("x", &entry(5, None)));
        assert!(!FileFilter::parse(Some("size:5"))?.matches("x", &entry(6, None)));
        assert!(FileFilter::parse(Some("size:=5"))?.matches("x", &entry(5, None)));
        assert!(FileFilter::parse(Some("size:>5"))?.matches("x", &entry(6, None)));
        assert!(FileFilter::parse(Some("size:>=5"))?.matches("x", &entry(5, None)));
        assert!(FileFilter::parse(Some("size:<5"))?.matches("x", &entry(4, None)));
        assert!(FileFilter::parse(Some("size:<=5"))?.matches("x", &entry(5, None)));
        assert!(!FileFilter::parse(Some("size:<5"))?.matches("x", &entry(5, None)));
        Ok(())
    }

    #[test]
    fn invalid_filters_are_rejected() {
        assert!(FileFilter::parse(Some("bogus:x")).is_err());
        assert!(FileFilter::parse(Some("size:abc")).is_err());
    }

    #[test]
    fn count_matches_counts_present_entries() -> Result<(), Box<dyn std::error::Error>> {
        use crate::store::Store;

        let temp_dir = tempfile::tempdir()?;
        let store = Store::open_at(temp_dir.path().to_path_buf())?;
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir)?;
        store.create_repo("r", &repo_dir.to_string_lossy())?;

        let make = |size: u64, mime: Option<&str>, missing: bool| FileEntry {
            size,
            hash: [size as u8; 32],
            modified_ms: 0,
            missing,
            mime: mime.map(str::to_string),
            img_fingerprint: None,
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: None,
            origin: None,
            exif: None,
        };

        store.update_file_entry("r", "photos/a.png", &make(100, Some("image/png"), false))?;
        store.update_file_entry("r", "photos/b.png", &make(300, Some("image/png"), false))?;
        store.update_file_entry("r", "docs/c.txt", &make(50, Some("text/plain"), false))?;
        // Missing entry must be excluded even if it matches.
        store.update_file_entry("r", "photos/d.png", &make(400, Some("image/png"), true))?;

        let db = store.open_repo_db("r")?;

        assert_eq!(count_matches(&db, &FileFilter::All)?, 3);
        assert_eq!(
            count_matches(&db, &FileFilter::parse(Some("mime:image"))?)?,
            2
        );
        assert_eq!(
            count_matches(&db, &FileFilter::parse(Some("name:photos/"))?)?,
            2
        );
        assert_eq!(
            count_matches(&db, &FileFilter::parse(Some("size:>=100"))?)?,
            2
        );
        assert_eq!(
            count_matches(&db, &FileFilter::parse(Some("mime:image size:>=200"))?)?,
            1
        );
        Ok(())
    }

    #[test]
    fn parse_multiple_filters() -> Result<(), FilterError> {
        let filter = FileFilter::parse(Some("mime:image/ name:my cool photo size:>=100"))?;
        let expected = FileFilter::And(vec![
            FileFilter::Mime("image/".to_string()),
            FileFilter::Name("my cool photo".to_string()),
            FileFilter::Size(SizeOp::Ge, 100),
        ]);
        assert_eq!(filter, expected);

        assert!(filter.matches("my cool photo.png", &entry(150, Some("image/png"))));
        assert!(!filter.matches("my warm photo.png", &entry(150, Some("image/png"))));
        assert!(!filter.matches("my cool photo.png", &entry(50, Some("image/png"))));
        assert!(!filter.matches("my cool photo.txt", &entry(150, Some("text/plain"))));
        Ok(())
    }

    #[test]
    fn name_value_keeps_internal_spacing() -> Result<(), FilterError> {
        // Repeated and surrounding spaces inside a value are preserved verbatim
        // rather than collapsed by whitespace splitting.
        let filter = FileFilter::parse(Some("name:a  b"))?;
        assert_eq!(filter, FileFilter::Name("a  b".to_string()));
        assert!(filter.matches("x/a  b.txt", &entry(1, None)));
        assert!(!filter.matches("x/a b.txt", &entry(1, None)));

        let combo = FileFilter::parse(Some("name:two  spaces size:>=10"))?;
        assert_eq!(
            combo,
            FileFilter::And(vec![
                FileFilter::Name("two  spaces".to_string()),
                FileFilter::Size(SizeOp::Ge, 10),
            ])
        );
        Ok(())
    }
}

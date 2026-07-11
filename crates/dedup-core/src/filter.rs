//! File filters for diff and file operations, ported from the legacy
//! `FilterFactory`: `mime:<substring>`, `name:<substring>`, and
//! `size:<op><bytes>` with the operators `>=`, `<=`, `>`, `<`, `=`
//! (a bare number means equality).

use crate::store::{FileEntry, StoreError, for_each_file_entry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileFilter {
    /// No filter: matches everything.
    All,
    /// Matches entries whose MIME type contains the substring.
    Mime(String),
    /// Matches entries whose relative path contains the substring.
    Name(String),
    /// Matches entries whose size satisfies the comparison.
    Size(SizeOp, u64),
    /// Matches entries whose provenance (`origin`) contains the substring.
    Origin(String),
    /// Combine multiple filters with AND logic.
    And(Vec<FileFilter>),
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
        const PREFIXES: [&str; 4] = ["mime:", "name:", "size:", "origin:"];
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
            Self::Name(substring) => rel_path.contains(substring),
            Self::Origin(substring) => entry
                .origin
                .as_ref()
                .is_some_and(|origin| origin.contains(substring)),
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

//! File filters for diff and file operations, ported from the legacy
//! `FilterFactory`: `mime:<substring>`, `name:<substring>`, and
//! `size:<op><bytes>` with the operators `>=`, `<=`, `>`, `<`, `=`
//! (a bare number means equality).

use crate::store::FileEntry;

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
    #[error("Unknown filter '{0}': expected mime:<substring>, name:<substring>, or size:<expr>")]
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
        if let Some(rest) = filter.strip_prefix("mime:") {
            return Ok(Self::Mime(rest.trim().to_string()));
        }
        if let Some(rest) = filter.strip_prefix("name:") {
            return Ok(Self::Name(rest.trim().to_string()));
        }
        if let Some(rest) = filter.strip_prefix("size:") {
            return Self::parse_size(rest.trim());
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
            Self::Size(op, value) => match op {
                SizeOp::Lt => entry.size < *value,
                SizeOp::Le => entry.size <= *value,
                SizeOp::Gt => entry.size > *value,
                SizeOp::Ge => entry.size >= *value,
                SizeOp::Eq => entry.size == *value,
            },
        }
    }
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
}

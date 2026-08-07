//! Rasterizing a document to page images for the viewer's Render tab — the
//! document shown as it *looks*. External-tool based, like video via `ffmpeg`:
//! a PDF is rendered with `pdftoppm` (poppler). Absent the tool, rendering
//! yields nothing and the caller simply omits the tab. Best-effort throughout:
//! a failure is an empty result, never an error.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Whether `pdftoppm` (poppler) is runnable — probed like
/// [`crate::fingerprint::ffmpeg_available`].
pub fn pdftoppm_available() -> bool {
    Command::new("pdftoppm")
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Render a PDF's pages to PNG files under `out_dir` at ~150 DPI via `pdftoppm`,
/// returning the page image paths in page order. Empty when `pdftoppm` is
/// unavailable or the PDF can't be rendered, so a missing tool degrades
/// gracefully (the Render tab just doesn't appear).
pub fn render_pdf_pages(pdf: &Path, out_dir: &Path) -> Vec<PathBuf> {
    let prefix = out_dir.join("page");
    let ok = Command::new("pdftoppm")
        .args(["-png", "-r", "150"])
        .arg(pdf)
        .arg(&prefix)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Vec::new();
    }
    let mut pages: Vec<PathBuf> = std::fs::read_dir(out_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    // pdftoppm zero-pads page numbers to a uniform width per document, so a
    // lexical sort is page order.
    pages.sort();
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Author a valid one-page PDF (a filled rectangle, no fonts) with lopdf, so
    /// the fixture is a real PDF `pdftoppm` accepts — fake bytes would be
    /// rejected.
    fn tiny_pdf(path: &Path) {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content = Content {
            operations: vec![
                Operation::new("rg", vec![0.85.into(), 0.15.into(), 0.15.into()]),
                Operation::new("re", vec![20.into(), 20.into(), 200.into(), 100.into()]),
                Operation::new("f", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 240.into(), 140.into()],
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        doc.save(path).unwrap();
    }

    #[test]
    fn renders_a_pdf_to_at_least_one_page_image() {
        if !pdftoppm_available() {
            eprintln!("skipping: pdftoppm not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("doc.pdf");
        tiny_pdf(&pdf);
        let out = tempfile::tempdir().unwrap();
        let pages = render_pdf_pages(&pdf, out.path());
        assert_eq!(pages.len(), 1, "a one-page PDF renders to one image");
        let bytes = std::fs::read(&pages[0]).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "a real PNG was written");
    }

    #[test]
    fn junk_input_yields_no_pages() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("notpdf.pdf");
        std::fs::write(&bad, b"not a pdf").unwrap();
        let out = tempfile::tempdir().unwrap();
        // pdftoppm (when present) rejects it; absent, we short-circuit. Either
        // way: no pages, no panic.
        assert!(render_pdf_pages(&bad, out.path()).is_empty());
    }
}

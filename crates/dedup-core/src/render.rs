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

/// Render exactly **one page** (1-based) of a PDF to a single PNG under
/// `out_dir`, returning its path. `-f N -l N -singlefile` makes poppler stop
/// after that page instead of rasterizing the whole document — the difference
/// between ~0.3 s and minutes on a 200-page book. Returns `None` when
/// `pdftoppm` is unavailable, the page doesn't exist, or it can't be rendered.
pub fn render_pdf_page(pdf: &Path, out_dir: &Path, page: usize) -> Option<PathBuf> {
    let prefix = out_dir.join("page");
    let n = page.max(1).to_string();
    let ok = Command::new("pdftoppm")
        .args(["-png", "-r", "150", "-f", &n, "-l", &n, "-singlefile"])
        .arg(pdf)
        .arg(&prefix)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    // `-singlefile` writes exactly `<prefix>.png`, with no page-number suffix.
    let out = out_dir.join("page.png");
    out.is_file().then_some(out)
}

/// Render only a PDF's **first page** — [`render_pdf_page`] at page 1.
pub fn render_pdf_first_page(pdf: &Path, out_dir: &Path) -> Option<PathBuf> {
    render_pdf_page(pdf, out_dir, 1)
}

/// How many pages a PDF has, via `pdfinfo` (ships with poppler alongside
/// `pdftoppm`). `None` when the tool is unavailable or the file is not a
/// readable PDF — the caller then steps pages blind and relies on
/// [`render_pdf_page`] returning `None` past the end.
pub fn pdf_page_count(pdf: &Path) -> Option<usize> {
    let out = Command::new("pdfinfo")
        .arg(pdf)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|v| v.trim().parse().ok())
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
    fn first_page_renders_a_single_png() {
        if !pdftoppm_available() {
            eprintln!("skipping: pdftoppm not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("doc.pdf");
        tiny_pdf(&pdf);
        let out = tempfile::tempdir().unwrap();
        let page = render_pdf_first_page(&pdf, out.path()).expect("first page renders");
        let bytes = std::fs::read(&page).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "a real PNG was written");
        // `-singlefile` writes exactly one image, no page-number suffix.
        let pngs = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
            .count();
        assert_eq!(pngs, 1, "only the first page is written");
    }

    #[test]
    fn page_past_the_end_is_none_and_count_reads_pages() {
        if !pdftoppm_available() {
            eprintln!("skipping: pdftoppm not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("doc.pdf");
        tiny_pdf(&pdf);
        let out = tempfile::tempdir().unwrap();
        assert!(
            render_pdf_page(&pdf, out.path(), 2).is_none(),
            "a one-page PDF has no page 2"
        );
        // pdfinfo may be absent even where pdftoppm exists; only assert the
        // value when the probe answers at all.
        if let Some(n) = pdf_page_count(&pdf) {
            assert_eq!(n, 1, "the fixture is one page");
        }
    }

    #[test]
    fn first_page_of_junk_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("notpdf.pdf");
        std::fs::write(&bad, b"not a pdf").unwrap();
        let out = tempfile::tempdir().unwrap();
        assert!(render_pdf_first_page(&bad, out.path()).is_none());
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

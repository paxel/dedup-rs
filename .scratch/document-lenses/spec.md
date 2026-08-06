# Document comparison lenses — Text · Render · Strings · Hex

Status: ready-for-agent

Spec synthesized 2026-08-04 from two grilling sessions ("support for other file
formats and text view", "about text rendering") plus the first implemented slice
(the Text tab's extracted-text view and aligned content diff). Where a decision
is already built, it is marked **[done]**; the rest is **[planned]**.

## Problem Statement

I inherited a pile of documents and I am using the viewer to decide, copy by
copy, what to keep. But I cannot compare what two documents *contain*. A PDF or a
Word file opens as a meaningless hex dump — I can't read it, let alone see
whether two copies say the same thing. The only comparison the viewer offers for
a document is a byte-level hex diff, which is drowned in container and encoding
noise and tells me nothing about the words. Worse, a document that was re-saved
or re-exported reads identically but differs in bytes, so it looks "completely
different" when it isn't. I need to see, quickly and faithfully, what each
document says and exactly where two copies differ — and I do not want the machine
deciding sameness for me; that call is mine.

## Solution

The viewer offers up to four independent **representation tabs** for a file, each
a faithful lens and none of them a verdict:

- **Text** — for a file whose purpose is text (PDF, Word/OpenDocument, spreadsheet,
  presentation, email): its **extracted readable words**. Comparing two, their
  content is **line-aligned side by side** with the differences marked — **green**
  where a line exists on only one side, **amber** where characters changed within a
  line. A document that yields no text (scanned, encrypted, empty) says so. **[done]**
- **Render** — the document **rasterized to page images**, shown as it actually
  looks. Comparing two, they sit side by side, each side with its **own page
  advancer**, and **flicker** swaps the current pages so subtle differences jump
  out. Judged by eye: no automated pixel diff, no page-pairing guess, no sameness
  verdict. **[partial]** (PDF first-page render, side by side, ships now; office
  formats, multi-page navigation, and flicker are follow-ups.)
- **Strings** — the **printable character runs** embedded in any file's bytes (the
  `strings`-style view the Browse tab already has), now available in the viewer
  too, and comparable. **[done]**
- **Hex** — the **raw bytes** of any file, **always** present as its own tab;
  comparing two, the existing aligned hex diff. **[planned as its own tab; today it
  is the Text tab's fallback for non-documents]**

Governing principle (chosen by the user, twice): the viewer **reveals and
compares faithfully and highlights what is not equal; it never weighs in on
keep-versus-delete, and never declares two things "the same."** The removed
"pixels identical" banner is the shape of thing that must not return.

## User Stories

1. As someone triaging inherited documents, I want to open a PDF and read its
   actual words in the viewer, so that I don't have to launch an external app just
   to see what it is.
2. As a user comparing two copies of a report, I want their text lined up side by
   side, so that I can read them together instead of squinting at two blobs.
3. As a user comparing two documents, I want the lines that are the same to sit
   across from each other, so that my eye isn't dragged around by reflowed text.
4. As a user comparing two documents, I want a line that only one copy has to leave
   the other side blank and be marked green, so that I can see something was added
   or removed.
5. As a user comparing two documents, I want a line that changed to show both
   versions with only the differing characters marked amber, so that I see the
   precise edit, not a whole highlighted line.
6. As a user, I never want the viewer to tell me two documents are "the same," so
   that the keep-or-delete judgement stays mine.
7. As a user opening a scanned or encrypted PDF, I want the Text tab to say plainly
   that there's nothing readable, so that I'm not staring at a blank pane wondering
   if it's broken.
8. As a user, I want the extracted text to be the words as written — spacing and
   all — not a normalized hashing form, so that what I read matches the document.
9. As a user, I want the Text tab offered only for files whose purpose is text
   (PDF, Word, spreadsheets, presentations, OpenDocument, email), so that a JPEG or
   an archive doesn't sprout a meaningless text tab.
10. As a user, I want to know which document formats the tool can read as text, so
    that I'm not guessing whether a given file will show its words.
11. As a user comparing two large documents, I want the alignment to fall back to a
    coarser pairing and tell me it did, rather than hang, so that a big file is
    still usable.
12. As a user, I want to render a document to pages and look at it as it appears, so
    that layout and formatting I can only judge visually are available to me.
13. As a user comparing two rendered documents, I want each side to have its own
    page advancer, so that when one copy has an extra front page I can line them up
    myself.
14. As a user comparing two rendered documents, I never want the tool to guess which
    page maps to which; I just want it to tell me the page counts and let me
    navigate, so that it isn't silently mis-pairing pages.
15. As a user comparing two rendered pages, I want to flicker between the current A
    page and current B page, so that a small visual change is obvious.
16. As a user comparing two rendered documents, I don't want an automated pixel diff,
    because different rendering (fonts, antialiasing) would drown it in noise and any
    "equal" claim would be a lie.
17. As a user on a machine without the rendering tools installed, I want the Render
    tab to be simply absent (like video without ffmpeg), so that the app degrades
    gracefully instead of erroring.
18. As a user, I want a Strings tab that pulls the printable runs out of any file's
    bytes, so that I can see the embedded text in a JPEG's metadata, an mp3's tags,
    or an unknown binary.
19. As a user comparing two binaries, I want their extracted strings diffed, so that
    shared embedded text hints at shared provenance.
20. As a user, I want a Hex tab available for every single file with no exceptions,
    so that the raw byte truth is always one click away regardless of type.
21. As a user, I want the Hex, Strings, and Text tabs to be independent of each
    other, so that each answers its own question without the others getting in the
    way.
22. As a user comparing two documents that read identically but differ in bytes, I
    want the Text tab to show no marks (content matches) while the Hex tab shows the
    byte difference, so that I can see both truths without either being called a
    verdict.
23. As a user opening a slow-to-parse PDF, I don't want the interface to freeze while
    it extracts, so that the app stays responsive.
24. As a developer picking this up, I want the extraction and diff logic to be pure
    and unit-testable without a GUI, so that correctness is pinned cheaply.
25. As a developer, I want any new layout verified by a rendered screenshot, not just
    a label query, so that column overlap or misalignment is actually caught.

## Implementation Decisions

- **Extraction is a core concern, display-shaped.** `dedup-core` exposes
  `extract_document_text(path, mime) -> Option<String>` returning the **raw,
  un-normalized** text, reusing the same per-format extractors that back the dedup
  text-hash (PDF via the text half of the PDF hasher; office/ODF via the zip/XML
  reader; legacy xls via the workbook reader; email as subject + body). It returns
  `None` for a non-document mime or an empty/unreadable document. A cheap
  `is_extractable_document(mime) -> bool` predicate drives tab presence per frame
  without reading the file. **[done]**
- **The set of "text-purpose" documents** is exactly what the core can extract:
  PDF, Word (`.docx`), spreadsheets (`.xlsx`/`.xls`/`.ods`), presentations
  (`.pptx`/`.odp`), OpenDocument text (`.odt`), and email (`.eml`). Plain `text/*`
  is excluded from the *document* path (it is already its own text and shows as
  text). Legacy binary `.doc` has no extractor and is out. This set is documented
  for the user. **[done]**
- **Reuse the byte-alignment engine for both diffs.** `dedup_core::align` (prefix/
  suffix trim + bounded LCS degrading to block-anchoring) already backs the hex
  diff; the content diff reuses it for the **per-character** highlight *within* a
  changed line. **[done]**
- **The content diff is line-aligned.** A GUI `textdiff` module builds the diff:
  a **line-level LCS** pairs equal lines; unmatched runs between matches are paired
  positionally (line i of A's block against line i of B's block, character-diffed
  via `align`), and any surplus lines on one side become one-sided **gap** rows.
  Colours are the review-board vocabulary — **green** = gap (one side only),
  **amber** = change (both sides differ). It caps rows and degrades the line pairing
  on very large documents, saying so. Rendered as two aligned columns in one scroll,
  a faint band behind changed rows. No verdict. **[done]**
- **The Text tab routes by mime.** Comparing two, if both sides are extractable
  documents it shows the content diff; otherwise the aligned hex diff. One file
  shows its extracted words, or a short empty-state note for a document that yields
  nothing. The note (and any new near-tab string) must **not contain a tab name**
  ("Text"/"Hex"/"Image"/"Metadata"/"Strings") because the kittest label query
  panics on multiple matches. **[done]**
- **Extraction runs on demand and is cached** by content hash in the viewer, so it
  runs once per pairing, never per frame. The target is a **worker thread** (the UI
  thread never blocks); the first slice runs it synchronously, matching the existing
  text-preview, and threading is a follow-up. **[partial]**
- **Office paragraph structure.** The office extractor currently joins runs with
  spaces, so a Word/Office file extracts as one long line and its content diff
  degenerates to a single wrapped row. Teaching the office text reader to emit a
  newline at `<w:p>` (and ODF paragraph) boundaries restores line structure. This is
  **hash-neutral** — the dedup text-hash strips all whitespace — but it is a *core*
  extractor change. **[done]** (paragraph and spreadsheet-cell boundaries now emit a
  newline; verified hash-neutral by the cross-container grouping test.)
- **Strings comes to the viewer.** The Browse tab already extracts printable runs
  (a `Hex | Strings` byte-mode). Lift that extraction to a shared pure function and
  add a **Strings** representation tab to the viewer, comparable via the same
  aligned-diff machinery on the printable runs. **[done]** (core `strings::printable_strings`,
  reused by the Browse preview and the new viewer Strings tab.)
- **Render is rasterize-to-images, compared by eye.** A core function rasterizes a
  document to page images by shelling out — `pdftoppm` for PDF, headless LibreOffice
  (`--convert-to pdf`) then rasterize for office formats — mirroring the existing
  external-tool pattern (ffmpeg). Absent tools ⇒ no Render tab, no error. The tab
  reuses the image compare surface (texture, zoom/pan, flicker) but adds an
  **independent per-side page advancer**; flicker swaps the *current* A page against
  the *current* B page. The machine never pairs pages or diffs pixels; it states the
  page counts and lets the user align and judge. **[partial]** — shipped:
  `render::render_pdf_pages` (PDF via `pdftoppm`, tool-gated) and a `Render` tab for
  `application/pdf` showing the **first page**, one file or two side by side,
  synchronously (fast, no async needed). Deferred: office via headless LibreOffice
  (needs the async channel, since it is multi-second), multi-page navigation with the
  per-side page advancer, and flicker.
- **The tab split is last.** Splitting today's dual-purpose Text tab into
  independent **Hex** (every file), **Strings** (every file), and **Text**
  (documents) tabs is done *after* Text/Strings/Render each have content, so the tab
  model is reshaped once with everything in place rather than renamed and re-touched.
  **[planned]**
- **No sameness verdict anywhere.** The image "pixels identical" banner and its
  raster-equality seam were removed; no equivalent may appear in Text, Strings, or
  Render. The factual hex "the shown bytes are identical" line is a byte statement,
  not a rendered-pixel verdict, and stays. **[done — removal]**

## Testing Decisions

A good test asserts **external behaviour**, not implementation: the *rows* a diff
produces (which are equal, which changed, what's on each side, which runs are
green vs amber), the *presence and absence* of user-visible content, and — for
anything with layout — a **rendered screenshot**, because a label query passes on
a clipped or overlapping widget. Three seams, all already established in the
codebase; prefer them, add functions at them rather than new seam types:

1. **Core pure functions** (`dedup-core::fingerprint`): `extract_document_text` /
   `is_extractable_document`, the planned strings extractor, and the planned
   document rasterizer. Unit-tested on **constructed fixtures** (a zip built as a
   `.docx`, a written `.eml`) with no GUI. The rasterizer follows the **ffmpeg-gated
   video test** prior art: an integration test that skips when the external tool is
   absent. Prior art: the existing office/eml/text fingerprint tests.
2. **Pure diff modules** (`textdiff`, `hexdiff`, `dedup_core::align`):
   `TextDiff::build` / `HexDiff::build` take strings/bytes and return an inspectable
   row structure — asserted directly (equal/changed rows, gap vs change marks,
   char-level highlight, degrade/truncate flags). Prior art: the existing `align`
   and `hexdiff` unit tests.
3. **Viewer behaviour** (`compare_view` kittest harness): headless geometric and
   label asserts that the right content shows and controls don't overlap, plus
   **`#[ignore]`d wgpu render tests** that write a PNG for a human to look at (e.g.
   the content-diff screenshot). Prior art: the existing `doc_screenshot_*` and
   `render_*` tests and the compare_view kittest suite. Per the roadmap lesson: a
   label-query-only test does not catch a layout bug — render it.

## Out of Scope

- Storing extracted text or rendered pages in the index (both are on-demand,
  viewer-cached; no `ENTRY_VERSION` change, no scan cost).
- Full-text search and word clouds over extracted text (a separate, far-future
  effort in the roadmap).
- Native pure-Rust PDF/office rendering (pdfium/mupdf and friends); rendering is
  external-tool rasterization only.
- New format decoders beyond what the core already extracts; legacy binary `.doc`.
- In-viewer editing of document content (the metadata/ID3 editor is a separate,
  existing surface).
- Any automated verdict of document or page sameness.

## Further Notes

- The Render lens exists precisely *because* extracted text is not guaranteed
  faithful (two extractors may disagree) and a rendered pixel diff is not reliable
  (antialiasing/font substitution). Neither lens is authoritative; offering both,
  with the human judging, is the design.
- Everything degrades gracefully: absent rendering tools ⇒ no Render tab; a document
  with no extractable text ⇒ an empty-state note; a document too large ⇒ coarse
  alignment that says so. The content hash always identifies the file regardless.
- The already-shipped slice: `extract_document_text` + `is_extractable_document`
  (core, unit-tested), the `textdiff` module (unit-tested), the Text tab wired for
  single-document words and the two-document content diff, a rendered screenshot,
  and the documentation of supported formats. Since shipped: office paragraph structure
  (Word/Office docs diff line by line), the core `strings::printable_strings` extractor
  (Browse now reuses it), and the viewer **Strings** tab (single-file runs and a two-file
  aligned diff), and the **Render** tab (PDF first page, one file or two side by side, via
  `pdftoppm`). Remaining: office rendering + multi-page navigation + flicker (needs headless
  LibreOffice and the async channel), the Hex/Strings/Text **tab split**, and worker-threaded
  extraction.

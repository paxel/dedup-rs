# Spec — Compare viewer: faithful diff, focused flicker, honest locks

Status: implemented (all nine tickets resolved 2026-08-03)

## Origin

A live review of the compare viewer in the new light theme surfaced a batch of feedback while
comparing two perceptually-grouped TIFFs (`Ford_alt 006.tif` / `011.tif`): **identical image
pixels, different embedded metadata**, different sizes/hashes (866.21 vs 865.59 KB). The hex tab
showed every byte after the inserted header as "different"; flicker appeared to do nothing;
several light-mode legibility and control-clarity problems came with it.

## Governing principle (Q1, Q2)

**The compare viewer's job is to reveal and compare the data faithfully, and to highlight what
is _not_ equal. It does not weigh in on keep-vs-delete — that decision is the user's, and the
machine stays out of it.**

- Highlighting differences is the *purpose*, not interference. Surfacing a distinction (which
  copy is older, which bytes differ) is always in scope; recommending an *action* is not.
- The green "distinction" highlight on the facts strip (older date / larger size) stays. Green
  is a **neutral highlight of a difference, not a weight**. "Older = green" is the current
  preference; the *direction* may become configurable later, not now. (The older-vs-newer flip
  is already implemented.)

## Detection model (Q3)

Layered, built bottom-up:

1. **Byte-alignment engine** (general, any file type) — the foundation; powers the hex view.
2. **Decode-and-compare** (media) — the "pixels are identical" verdict for images.
3. **Format-aware parsing** (per format, TIFF/EXIF first) — names and extracts metadata.

Each layer lands independently. Byte alignment first, since it fixes the most visible problem
and everything else layers on it.

---

## A. Hex view — a full-file, aligned, paginated diff (Q4–Q8)

Replaces today's header-only preview ("hex of only the first bytes").

- **General byte-level alignment (Q4, reframed):** not an "N-bytes-inserted" detector — a real
  alignment. Find the **equal sections** and lay them side-by-side; lay the **differing
  sections** side-by-side too — a **gap** padded into one side (insertion/deletion), or
  **substituted** data on both sides — **repeating as often as the file demands**. No prose
  summary of the diff; the visual alignment *is* the explanation.
- **Whole file, paginated (Q4):** the header-only limit is removed. Pagination is mandatory.
- **Jump to next/prev difference (Q5):** so long equal runs can be skipped; plain pagination
  alone is unusable on mostly-equal large files. (A diff minimap is a later nice-to-have.)
- **Scale + exactness (Q6):** exact/optimal alignment up to a size/complexity budget, then
  **degrade to anchor-based (rolling-hash) alignment with an honest on-screen notice** ("file
  too large for full alignment — showing block-level"). Small files perfect; large files
  responsive; never a silent cap, never a claim of equal where unequal.
- **Visual language (Q7):** region background **bands** for structure + **per-byte** emphasis
  inside a region, in the review-board vocabulary — **gap/insertion = green**, **substitution =
  amber**, equal bytes plain. Theme-aware (legible light and dark).
- **No hex flicker (Q8):** the aligned, highlighted, paginated side-by-side already shows every
  diff. Flicker adds nothing here and is dropped for hex.

## B. Image view — state pixel-identity (Q9)

- When the two rasters are **byte-for-byte identical**, show a neutral line in the image view:
  *"Pixels identical — no visual difference; differences are in metadata."* A comparison result,
  not a verdict — and it dissolves the "flicker only flips the corner" confusion (the corner
  label was the sole thing changing because the pixels don't differ).
- A **quiet pointer** to the tab that does differ (metadata/hex) is allowed; it must **not**
  auto-switch the user's tab. Cheap to compute — both images are already decoded for display.

## C. Flicker — single-file focus (Q8, Q9, Q10)

Flicker is **image/media only** (never the hex tab; the SWAP / SIDE BY SIDE controls must not
leak onto Text).

- In flicker, the top chrome shows **only the currently-visible file**: its repo, size, path;
  its rotate/mirror/save; its delete. **Never two sides.** **SWAP flips the image and all of
  that chrome together** to the other file. There is no hidden-side control to click by
  accident — which is what made "ROTATE B does nothing" so confusing.
- The green distinction highlight **stays** in the single-file strip (Q10): green means "this
  shown file is the older/larger of the two," and it **flips on SWAP**.

## D. Metadata — compare and salvage (Q11)

- The Metadata tab already lists EXIF/TIFF fields per side. Add **diff highlighting**: show
  which fields differ between the two sides.
- Make the metadata **extractable**: a control to **save a side's metadata to a human-readable
  sidecar file** (decoded text/JSON of the fields — the goal is preserving the *information*,
  not reconstructing the exact bytes) in a folder you pick. One extract per side, whole blob.
- Per-field extraction and **merging metadata across sides** (writing metadata back into an
  image) are **out of scope for now** — merge modifies the file in a new way and is deferred,
  like the archive write-back was.

## E. Lock semantics — protect existing, allow add (Q13, Q14)

A locked / read-only ("Protected") repo protects the **existing** files; adding a **new** file
is fine.

- **DELETE** (removes an existing file) → blocked when locked.
- **SAVE → overwrite in place** (alters an existing file) → blocked when locked.
- **SAVE → as a new copy** (writes a new suffixed sibling, original untouched; indexed on the
  next scan) → **allowed even when locked**.
- Therefore: **stop hiding SAVE on a locked side.** Show it; in the save dialog, disable
  "overwrite in place" when locked (with "locked — original protected") and keep "save a copy"
  live. DELETE stays blocked. **Don't silently vanish controls** — the blocked ones (overwrite,
  delete) show disabled with the reason and how to lift it ("unlock in Duplicates"). An
  in-viewer unlock affordance is a possible later follow-up; the lock is a deliberate,
  per-session safety gate and shouldn't become one-click frictionless.

## F. Light-mode legibility & consistency (Q12, plus two polish items)

- **Ink-on-accent (Q12):** filled pills paint `theme::black()` on the accent fill, which reads
  poorly on the light palette's deliberately-dark accents (the "check the delete buttons"
  report). Introduce **one palette-aware "ink on accent" colour** — black on dark (byte-
  identical), a near-white light ink on light — and route every filled pill through it. Fixes
  DELETE and all siblings at once; stops the whack-a-mole.
- **Selected representation tab:** the active tab (Image / Text / Metadata / …) needs a clear
  **border/highlight** so it's obvious which is selected. (Recommended: an accent border around
  the selected tab; confirm treatment at build time.)
- **Repo-name decoration consistency:** the repo name is decorated (identicon chip) in the Text
  view but plain in the Image view. Make the header consistent across representation tabs.

---

## Out of scope (for this effort)

- Any keep/delete recommendation or automation — the machine does not decide (Q1).
- **Merging metadata** across sides / writing metadata back into an image (Q11).
- Per-field metadata extraction as individual "members" (Q11) — whole-blob sidecar first.
- A diff **minimap** for the hex view (Q5) — jump-to-next-diff first.
- Making the green-highlight **direction** configurable (Q2) — fixed to "older" for now.
- An **in-viewer unlock** affordance (Q13) — disabled-with-reason first.

## Already done

- Facts-strip date highlight now prefers the **older** copy (green on the earlier
  `modified_ms`); doc comment updated to "bigger-or-older". No test depended on the direction.

## Next

Break this into tickets (issue-tracker convention under `.scratch/compare-intel/issues/`),
sequenced: the byte-alignment engine (A) and ink-on-accent (F) are the unblockers; B/C/D/E layer
on top. Two small polish items (selected-tab border, repo-name consistency) can land anytime.

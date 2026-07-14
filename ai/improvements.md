# dedup-rs — Improvement Roadmap

## Vision

dedup-rs is a **data-inheritance triage tool**. The scenario it serves: someone dies (or a
machine dies) and leaves behind a NAS, broken PCs, and an unsorted heap of disks full of
redundant backups. A caretaker must find the useful and important material — documents,
photos, crypto wallets, keys — without eyeballing terabytes of duplicates. As the
"no hardcopies" generation ages, this is a recurring, real problem.

Media strategy is **hybrid**: in-app image zoom, in-app audio playback, video as
scrub-able frame strips; one click hands any file to the system's external app for full
fidelity.

All four phases originally planned in this document (review tooling, sanitize workflow,
content coverage, forensic layer) have shipped. What remains is a near-term preview &
lightbox polish wave (Phase 6), the deferred Browse (Phase 7) and Recognition (Phase 8)
layers, and cross-cutting debt.

---

## Remaining / deferred work

- **Light theme toggle** (M/L, deferred by decision 2026-07-08): requires converting
  `theme.rs` constants to a runtime palette across all views. Still dark-only.
- **Performance at scale**: banded grouping, staged pipelines, and the multi-reference
  diff's merged content index are fine at ~10⁵ files; revisit content-index memory
  (`HashMap<(u64,[u8;32]), _>` across all references) and timeline streaming at 10⁷.
- **Testing discipline** (standing practice, not a task): every GUI feature ships with
  kittest geometric tests + an `--ignored` render snapshot; every core feature with
  temp-repo integration tests; store format changes must include a legacy-decode test
  (pattern: `store.rs::v1_entries_decode_and_flag_images_stale`).


## Phase 5 — Transfer, Grooming & smarter ETA  *(current branch: `feature/summer/transfer_and_grooming`)*

This wave splits the old **Files** tab into two purpose-built sections and fixes the
progress ETA. Scope confirmed 2026-07-13: Transfer + Grooming + ETA are near-term; the
Browse/forensic layer is Phase 6 and recognition/extensibility is Phase 7.

### 5.0 Tab restructure  — ✅ done

### 5.1 Transfer section  — ✅ done

### 5.2 Grooming section  — ✅ done

### 5.3 Smarter ETA  — ✅ done

---

## Phase 6 — Preview & Lightbox polish  *(near-term)*

The Duplicates lightbox is where triage actually happens, and it has three gaps: audio is
near-unusable (files render as the generic broken-image placeholder), the keyboard/​hint
affordances are incomplete, and there is no way to fix or re-tag a file without leaving the
app. This wave makes the lightbox a first-class comparison surface for **both** images and
audio, with safe in-place edits. (Folded from the "Open issues" list, kept verbatim below.)

### 6.1 Keyboard shortcuts & discoverable hints

**Lightbox — ✅ done (2026-07-14).**
- **Escape is a universal "back"**, not an immediate close: it pops one view level per press
  — flicker → side-by-side → single image → closed. This is the intended principle for all
  modals/future views (applied to the lightbox now; rolled out elsewhere with the cross-view
  work below).
- **`Space` drives flicker**: it enters flicker from side-by-side (previously button-only),
  then swaps A/B once there.
- **Hint "vanished on compare" was a real layout bug** (user was right): the hint string
  existed, but compare stacks three bottom lines (path A, path B, hint) into a strip only
  tall enough for two, so the hint rendered *below* the window bottom (measured at y≈709 in
  a 700 px window). Fixed by growing the bottom strip (and shrinking the viewport to match)
  in compare mode; a geometric test now asserts the hint stays on-screen.
- The compare hint bar is now **mode-aware** (distinct side-by-side vs flicker strings) and
  names every available shortcut.

**Remaining (deferred 2026-07-14 — this pass is lightbox-only by decision):** extend a
persistent, discoverable hint bar *and* actual keyboard shortcuts to the Transfer, Grooming,
and Duplicates-grid views, which have none today. Audio play/pause + next-audio shortcuts
land with the audio lightbox (6.3).

### 6.2 Audio preview tile

- **Bug:** audio files fall back to the generic broken-image thumbnail placeholder.
- Card view: replace it with a meaningful audio tile — duration + basic metadata plus a
  **deterministic fingerprint glyph** derived from `AudioFp.chunk_hashes` (identicon-style:
  hash bits drive colour / mirror / line generators). Determinism means identical or similar
  audio produces visibly similar glyphs. Fallback/simplest form: render the fingerprint as a
  grayscale block.
- Clicking the glyph opens the file in the audio lightbox (6.3).

### 6.3 Audio lightbox & audible comparison

- Give audio a lightbox that renders the fingerprint as an **audio graph** (waveform-style)
  so differences between similar files are *visible* side-by-side — reuse the A/B compare and
  flicker affordances where they map cleanly.
- While in the lightbox, let the user switch between the group's audio copies **without
  stopping playback**, preserving the playback offset, so differences are *audible*.
- *(Flagged "maybe" by the user — confirm before building:)* carry the same "keep offset when
  switching files" behaviour into the normal card view (`player` is a single global player,
  so this is a small extension of existing state).

### 6.4 Lightbox image editing (lossless)

- Add an **Edit** control in the image lightbox for lossless flip / rotate (90° steps +
  mirror).
- Offer an option to **overwrite** the original file with the transformed image, lossless
  where the format allows (e.g. jpegtran-style transforms for JPEG); document per-format
  limits. Gated by the 6.6 confirmation.

### 6.5 Audio id3 tags

- Display **id3** tags in the audio lightbox.
- Optional inline editor to change tags and write them back to the file. Gated by 6.6.

### 6.6 Safe in-place saves  *(cross-cutting for 6.4 & 6.5)*

- Every action that writes to a file from the lightbox (overwrite image, write id3) requires
  an explicit confirmation ack — "you are about to change this file on disk."
- Saving must **not** close the lightbox: the user stays in context to keep reviewing the
  group.

---

## Phase 7 — Browse & forensic layer  *(deferred)*

A fifth **Browse** tab hosting the forensic tools:

- Filter, display, and **annotate** files (trash / important / …); a new **annotation
  filter** follows naturally once annotations exist.
- **Binary / hex view** of at least a file's header.
- **Strings** on demand for unknown files.
- Unlocks the annotation-driven Transfer exports noted in 5.1.

## Phase 8 — Recognition & extensibility  *(far future)*

- Face recognition and object recognition for photos/images.
- VLA tagging of files to topics; word clouds for documents.
- MP3 tag handling; metadata extraction for all known formats.
- Plugin support for new formats; an API to externalise features.

---

### Source remarks (verbatim, kept as reference)

user demands changes:

* The ETA calculation is waaay off. the last time it predicted about 2h and it took 6. even 1h before finish the eta was still like 8 minutes. there must be some better prediction algos. is there a lib that allows that? if not we should create something like that: a function where you put total items, concurrent lanes, and then duration per lane and ask for eta every 5s to have a less flickering display?
* I dont understand the also REF selection. I find it not intuitive and there must be a better solution for whatever it tries to solve. create a plan with different options for the user to decide
* The Files section should be renamed to transfer section. because its transfering  files from different repos together. the delete should be moved to the new fourth section: grooming
* the move to and copy to commands are added, where unique files are copied or moved into a folder that the user can specify with a folder selector
  * the move and copy to have also mode selector where duplicates and siliars can be selected and an inverter so that duplicates instead are cpied / moved
* the grooming should have a top selector for the command and as it is the most complex one every command should have its own layout.
* the delete has a source repo selector and a multi repo selector for the remaining repos. the source repo deletes all duplicates that are in any of the selected repos.
* the organize command has a filter a path generator where the taret path can be generated with placeholders and alternatives to define the new reative path of files in a repo
  * maybe multiple filters and paths
  * the complete selection can be named, stored and reactivated by the user on other repos or in te future. for repeating or modifying the organisation
  * no file should ever get lost or overwritten here.
  * a preview similar to the copy move is required
  * a progress when executed too
* small tools like: 
  * delete empty dirs
  * delete ALL of a filter: mime, size name. the name filter should allow some kind of wildcards to ensure that you can say: ends with .db or starts with copy_of
* the fith page will be browse where all the forensic tools will be added. allo the user to filter files, display them, mark them with annotations (trash, important, etc)
* with annotations a new filter for annotations makes sense
* binary / hex view of at least the header of files
* strings on demand on unknown files
* the transfer copy to with annotation filter can be an export of important or otherwise tagged stuff. similar the move to can be an removal and archival of unimportant stuff
* face recognition of photos and image is a far future task
* object recognition also
* vla of files to specific topics
* word clouds to documents
* mp3 tags handling
* meta data extraction of all known formats
* plugin support for new formats
* api for externalize features


### Open issues (verbatim, folded into Phase 6)

* The lightbox has a shortcut description on the bottom. it goes away when you choose compare
* I like the shortcut description and I want one in every view and also as much as possible shortcutable
  * in lightbox the flicker view and there especially the swap need a key, maybe space?
* the display of audio is quite awful. the icon looks like a broken image. maybe show some meta info and maybe display the fingerprint as a glyph? you know where some short seed generates some small image, by taking some bits as color some as mirror some as line generator. dunno. some small human recognicable visualisaton. or optionl just the fingerprint as a grayscale block.
  * clicking the fingerprint image should bring you to the lightbox and the audio data is converted to nice audio graphs that can be compared against each other, so you can SEE the diffs?
* playing audio in the lightbox should allow switching between different audios and continues to play so you can also hear the differences of similar files
  * maybe this should also be possible in the normal view? when you play another play button, it keeps the offset?
* the lightbox should have an edit button to flip and rotate images losless with the option to actually overwrite the image with the flipped rotated image
* the audio lightbox should have a id3 display, and maybe an editor to change the id3 tag and save it
* all save actions in the lightbox need a "sure, you change that, you know" kind of ack for the user. saving does not leave the lightbx

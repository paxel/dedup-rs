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

All four phases planned in this document (review tooling, sanitize workflow, content
coverage, forensic layer) have shipped. What remains is cross-cutting debt.

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

## Phase 6 — Browse & forensic layer  *(deferred)*

A fifth **Browse** tab hosting the forensic tools:

- Filter, display, and **annotate** files (trash / important / …); a new **annotation
  filter** follows naturally once annotations exist.
- **Binary / hex view** of at least a file's header.
- **Strings** on demand for unknown files.
- Unlocks the annotation-driven Transfer exports noted in 5.1.

## Phase 7 — Recognition & extensibility  *(far future)*

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

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

### 5.0 Tab restructure

Grow `Tab` from `{ Repositories, Duplicates, Files }` to
`{ Repositories, Duplicates, Transfer, Grooming, Browse }`:

- Rename the **Files** tab (and `files_view.rs`) to **Transfer** — it moves files
  *between* repos, so the name should say so.
- The **DELETE** command leaves Transfer and becomes a Grooming command (see 5.2).
- Add the **Grooming** tab now; reserve **Browse** for Phase 6.

### 5.1 Transfer section

Commands: **COPY**, **MOVE**, plus two new folder-targeted variants:

- **COPY TO / MOVE TO a folder**: the destination is an arbitrary directory chosen with a
  folder picker (not a repo). Unique files are copied/moved there.
  - **Mode selector**: choose the working set — *duplicates* or *similars* — with an
    **invert** toggle, so the same command can act on the redundant copies instead of the
    unique ones.
- **Reference relabel** (decision 2026-07-13): keep today's target + extra-refs model but
  rename **ALSO REF** to a plainer label (e.g. **"ALREADY HAVE IN"**) and make the
  target's automatic inclusion visually obvious — a source file is "new" only when none of
  the reference repos already holds its content.
  - *Rejected alternatives (kept if relabel proves insufficient):* a single unified
    reference multiselect with a separate destination picker; inverting the model so the
    user picks the "keep set" and everything else is implicitly source.
- Preview + background-run progress already exist in `files_view.rs` — reuse them.
- Forward link: once annotations land (Phase 6), **COPY TO** with an annotation filter
  becomes an *export of important/tagged* material, and **MOVE TO** an *archival of
  unimportant* material.

Core work: `diff_copy` currently lands files in a target **repo** (`diff.rs::CopyDest`).
Add a plain-folder destination and a "unique / duplicate / similar" set selector (building
on `dupes.rs` / `similar.rs`) so extraction to a directory reuses the same preview and
progress path.

### 5.2 Grooming section

A **top command selector**; because these commands differ a lot, **each gets its own
layout** (unlike Transfer's shared form).

- **DELETE** — a **source** repo selector plus a **multi-repo** selector for the rest.
  Deletes every source file whose content exists in *any* selected repo. Generalises
  today's single-target-plus-extra-refs `diff_delete` to an arbitrary reference set.
- **ORGANIZE** — rewrites the *relative paths* of files within one repo:
  - A **path generator**: target paths built from **placeholders** (date Y/M/D, mime,
    extension, size bucket, original name, …) with **alternatives** (fallbacks used when a
    placeholder is empty).
  - One or **multiple filter → path** rules.
  - **Named, saved, reusable selections**: a whole organize config can be named, stored,
    and re-applied to other repos or re-run later (for repeating or tweaking an
    organisation).
  - **Safety invariant: no file is ever lost or overwritten** — collisions must resolve to
    a new name or refuse, never clobber.
  - **Preview** (like copy/move) and **progress** on execution.
  - Core: a new path-template engine, extending beyond today's
    `organize.rs::export_by_date`. Persist presets alongside the GUI settings.
- **Small tools**:
  - **Delete empty directories**.
  - **Delete everything matching a filter** (mime / size / name). The **NAME** filter
    needs **wildcards** — today `FileFilter::Name` is a plain substring
    (`filter.rs:127`); add prefix/suffix/glob matching so "ends with `.db`" or "starts
    with `copy_of`" work.

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

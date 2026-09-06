# Changelog

All notable changes to the `dedup-rs` project will be documented in this file.

## [0.6.0] - Unreleased

### Added

- **ACCEPT: contents allowed to repeat.** A podcast's per-folder `cover.jpg`
  (or any content you have decided may exist many times inside one
  repository) can be accepted, per repository, from the Duplicates tab: a
  copy's right-click menu offers **ACCEPT IN REPO**, and each group header an
  **ACCEPT GROUP** button that settles every repository present in one click.
  Accepted copies are protected like read-only ones — never preselected,
  never marked by AUTO-RESOLVE or MARK ALL, badged **accepted** on the card,
  and unlockable one file at a time when you do want one gone. A group whose
  every copy is accepted stays out of the list until **SHOW ACCEPTED** is
  turned on (the way back to un-accept it; the count of hidden groups is
  shown). The mark follows the content, not the path, so next month's episode
  folder with the same cover is accepted already; acceptance never crosses
  repositories, so the same cover in another repository is still an ordinary
  duplicate.
- **Accepted content is safe from every duplicate delete.** Grooming DEDUPE
  (and `dedup diff rm`) leave accepted source files alone and the REVIEW
  preview does not list them; `dedup repo dupes --delete` keeps every
  accepted copy.
- **`accepted:yes` / `accepted:no` filter condition** in the shared FILTER
  wizard and on the CLI, and an **accepted** badge in the Browse file panel.
- **`dedup accept <repo> [<path>...] [--rm]`** grants or withdraws the mark by
  path, and with no paths lists a repository's accepted contents with the
  files currently holding them; `dedup repo dupes` labels accepted copies.

---

Historical changes have been moved to [OLDER_CHANGES.md](OLDER_CHANGES.md).

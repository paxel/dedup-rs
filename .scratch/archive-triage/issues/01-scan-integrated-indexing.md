# 01 — Scan-integrated, incremental archive indexing

Status: resolved
Spec: ../spec.md

## Problem

Archive member indexing exists (`archive.rs::index_repo_archives`) but runs as a
**separate, opt-in bulk pass** (`dedup archive index`) that re-reads every archive
in the repo every time. That is the wrong economics: it re-hashes unchanged
archives, and it only ever happens when the user remembers to run a second command.

**Decided in session:** archive reading folds into the normal scan, gated by the
same change-detection loose files use. A changed archive (its file hash differs from
the index) is opened once and its members read into the index; an unchanged archive
is skipped — never re-opened.

## Approach

The scan pipeline (`update.rs`) already splits walked files into to-hash vs
unchanged by (size, mtime). When a to-hash file is an archive
(`archive::is_archive`), after hashing it as a file, also read its members and store
them via `set_archive_members`. An unchanged file that is an archive is skipped like
any other — its member list persists from the previous scan.

- Member reading happens on the same worker path as file hashing, so a repo of large
  archives parallelises and reports progress like any scan.
- The member list is keyed to the archive's rel-path (as today); re-reading a changed
  archive replaces its member list wholesale.
- `index_repo_archives` and the `dedup archive index` CLI command are **removed**; the
  coverage report (`repo_archive_coverage`, `dedup archive coverage`) stays and now
  reads whatever the incremental scan populated.

## Seam and tests

Core seam — `crates/dedup-core/tests/`, prior art `archive_test.rs` / `update_repo.rs`:

- scanning a repo with a new zip populates its member list as part of the scan (no
  separate index call)
- re-scanning with the zip unchanged re-reads nothing (assert no member re-hash — e.g.
  via a mtime-preserving no-op, mirroring the existing unchanged-skip tests)
- replacing the zip on disk (new hash) re-reads its members and replaces the old list
- coverage still reports correctly against the incrementally-built index

## Done

Standing gate green. `CHANGELOG.md`: the archive report no longer needs a separate
index step. `README.md` if it documented `archive index`. This ticket has no GUI
surface yet — coverage stays CLI — so the GUI is untouched.

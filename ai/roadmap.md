# Roadmap — week of 2026-07-20 → 2026-07-26

Fleshed-out plan for the open issues in [improvements.md](improvements.md). Ordered
strictly by **priority** (not by day): work top-down, what is finished is finished.
Decisions baked in below were confirmed 2026-07-19.

---

## Prio 1 — Preserve file dates on copy/move  *(S)*

**Problem.** Cross-repo copies (`diff.rs` — `diff_copy`, the move fallback in
`move_file`, `diff_sync`; `organize.rs` export copy) use `std::fs::copy`, which does
**not** copy timestamps. The new file gets a fresh mtime, so the next `repo update`
sees size-ok/mtime-changed and re-hashes every file we just copied.

**Fix.**
- After every file copy into a repo or export dir, set the destination mtime from the
  source's metadata via `std::fs::File::set_modified` (std, stable — no new crate).
- Applies to: `diff.rs` copy path (including the cross-device rename fallback used by
  move) and `organize.rs` `Dest::Copy`.
- Make sure the index entry recorded for the target keeps matching the on-disk mtime
  (the transfer already records "real on-disk mtime" — after this change that equals
  the source's), so the next `update` skips the file entirely.

**Tests.** Extend `crates/dedup-core/tests/diff_ops.rs`: after `diff cp` / `diff mv`,
assert destination mtime == source mtime, and assert a follow-up `update` on the
target re-hashes nothing (entry not stale, hash unchanged, `last_scan` bumps only).

---

## Prio 2 — Repo diff view: new DIFF command in the Transfer tab  *(M/L)*

A manual, Beyond-Compare-style diff of any two repos (in or out of a sync group).
This view later doubles as the sync-group divergence resolver, so it lands **before**
sync groups.

**Modes.**
- **BY HASH** — pair files by content. Same content at the same path: *equal* (grey,
  hidden by default). Same content, different paths: both sides yellow with a
  **RENAME** action per side (renaming the left file to the right file's name, and
  vice versa). Content missing on one side: **COPY** button on the side that lacks it,
  **DELETE** on the side that has it.
- **BY PATH** — pair files by relative path. Same path, same hash: *equal* (grey,
  hidden by default). Same path, different content: open a **lightbox** showing both
  sides with type-specific details (image/video/audio/pdf — reuse `lightbox.rs`);
  actions available in both the table row and the lightbox: **DELETE**, **RENAME**,
  **OVERWRITE with other side** (each action offered for both sides). Path missing on
  one side: same COPY/DELETE buttons as hash mode.

**Duplicate content (hash mode) — group row per hash, progressive narrowing.**
When a hash maps to multiple paths on a side, the row shows *all* paths per side and
its buttons adapt after every action until the row reaches the plain 1:1 state:
- Multi-path side: **DELETE ALL**, and **KEEP 1** — a popup lists the paths, picking
  one deletes the others (with a warning + confirmation).
- **RENAME** with several candidate names on the other side: popup to choose which
  name to take.
- After each direct manipulation the row re-evaluates: narrow same-side duplicates
  first, then pick a name from the other side, then copy/delete — the buttons always
  reflect the current state.

**Core (`dedup-core/src/diff.rs`).**
- New pure function producing diff rows from two repo indexes in one pass (both
  directions), keyed by hash or by path, with per-side path lists, size, and mtime.
- Single-file primitives the actions need: rename (disk + index), overwrite-from-
  other-side, single copy, single delete. Where a batch op restricted to one row
  already exists (the `DiffRun` `only`/exclude machinery powering the review table's
  per-row apply), reuse it instead of new code.
- Every copy/overwrite obeys Prio 1's mtime rule.

**UI (`dedup-gui`).**
- New DIFF command beside COPY/MOVE/DELETE in `transfer_view.rs`; both repos picked
  with the shared `repo_chip` selectors.
- Reuse/extend the shared `review.rs` table (virtualised, click-to-sort): add
  **size** and **modified date** columns (sortable) and per-row action buttons;
  keep the equal-rows-hidden toggle as the default.
- Popups (KEEP 1, rename-name picker) as modal confirm dialogs, LCARS-styled.

**Tests.** Temp-repo integration tests for the diff rows + each primitive (rename,
overwrite, copy, delete — assert file system *and* index state). Kittest geometric
tests for the table/buttons/popups + an `--ignored` render snapshot.

---

## Prio 3 — Sync groups & remote backup  *(L)*

Replace the manual DUPLICATE → RELOCATE → UPDATE backup dance with native groups:
one **main** repo plus one or more remote **sinks**.

**Data model (registry).**
- New `sync_groups` registry table: group name → postcard-encoded struct
  `{ main: String, sinks: Vec<String>, mode: SyncMode }`,
  `SyncMode = AddOnly | Mirror` — **the mode is configured per group**.
- Membership must be editable: add an existing repo to a group, move a repo out.
  A repo belongs to at most one group; guard rename/remove of member repos.
- Registry schema change ⇒ ship with a legacy-decode test per the standing
  store-format rule (pattern: `store.rs::v1_entries_decode_and_flag_images_stale`).

**Repo lists.** Sinks are **collapsed** under their main repo (expand chevron) in the
Repositories tab and in every `repo_chip::chip_row` selector; collapsed sinks are
treated as part of their main repo.

**Sync Groups tab (new).**
- Manage groups: create/delete, add/remove members, switch main, set the group mode.
- **RUN SYNC** (push main → sinks): update-scan main + sinks, `plan_sync` per sink,
  preview in the shared review table (PREVIEW/RUN pattern — always confirm before
  executing), then `diff_sync` per sink. `AddOnly` copies new content only;
  `Mirror` also deletes sink files whose content the main no longer has (existing
  `SyncDelete` machinery).
- **External changes in a sink**: reverse diff (sink vs main) listing sink-side
  additions/changes with a **migrate back** (copy to main) action — this is exactly
  the Prio 2 diff view pointed at (main, sink), plus divergence resolution via its
  row actions.

**Tests.** Core integration tests for group storage, membership rules, and sync
semantics per mode (AddOnly never deletes; Mirror converges sink to main). Kittest
tests for the tab and the collapsed-sink chip rows.

---

## Standing discipline (applies to every item above)

- Every GUI feature ships with kittest geometric tests + an `--ignored` render
  snapshot; every core feature with temp-repo integration tests.
- Store/registry format changes include a legacy-decode test.
- Docs upkeep in the same change: README, CHANGELOG (Keep a Changelog), docs/gui,
  docs/cli where commands change.

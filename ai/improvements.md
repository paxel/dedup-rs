# dedup-rs — Improvement Roadmap

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

## A usability
- the preview button should be renamed review as it is now a way for the user to review the action
- for the review pane we need more infos. the filename columns need to contain basically the same infos as the duplicate tabs group items. thumbnail, audio duration, size, etc. extremely nice would be a diff button between both that opens a lightbox diff view
  - for purge no diff button but still the info panel. 
  - these type based info panels should be in a way that we can easily extend them in the future
- sync repos. it seems the grouping of repos belongs to the first tab
  - repositories get a "main repo button. if clicked they are converted to repo group. getting an elbow and and an add repo button. 
  - the add repo button is for adding new repos. they inherit everything of the main repo but the path
  - existing repos get a "sink" button. which when clicked offers existing groups to be added to
  - the main group gets a sub elbow sinks, that can be colapsed
  - the main group gets a update all button that updates all repos of the group
  - each sink has a toggle pill mirror to select if the repo should be mirrored or just copied to
  - in all transfer groom etc views only the main groups are offered
  - group sync becomes repo sync where groups can be synced, but also ALL repos diffed vs another
- 

## B Recognition & extensibility  *(far future)*

- Face recognition and object recognition for photos/images.
- VLA tagging of files to topics; word clouds for documents.
- MP3 tag handling; metadata extraction for all known formats.
- Plugin support for new formats; an API to externalise features.

---


## Open issues & requests — 2026-07-18

### Sync groups & remote backup
- Backing up to a remote is currently manual: DUPLICATE a repo, RELOCATE the copy to the
  remote path, then UPDATE it. Make it a native feature. Mark repos as a **sync group**:
  one **main** repo plus one or more remote **sinks**. In the normal repo lists the sinks
  are **collapsed** (shown only when expanded) and otherwise treated as sinks of their main
  repo. A dedicated **Sync Groups** tab maintains the groups: run a sync (push main →
  sinks), detect **external changes in a sink** that may need migrating back to the main,
  resolve divergence, etc. It must also be possible to add existing repos to a group, an move repos out of a group
- new command in the transfer is for manually diffing two repos. (can also be two repos in a group but also outside of a sync group) creates a diff view of the files. 
  - you can diff by hash, if both exist and have same path: grey (default is hide equals completely), otherwise both yellow and two action buttons "rename" one in column 1 the other in column 4 and if the 1 is clicked the left repo file is renamed to the right repo name and vice versa if a file is missing on one side the side without the file gets a copy button and the other a delete button. thats basically all there is right?
  - you can diff by path, if both exist and have equal hash: grey (default is hide equals completely), otherwise we need to have some lightbox where we can display all the details of both files depending on their types. in the lightbox and in the table view we need the options to delete, rename or overwrite with other side is needed for both sides.
    - if one side is missing the copy and delete buttons as in the hash based diff is required
- we could also think about having size and modified date in the table.


## Open issues & requests — 2026-07-20

### C Safety: MIRROR can wipe a sink  — **done 2026-07-20**

A MIRROR push deletes sink content whose hash is absent from the main's **live**
entry set (`diff.rs::diff_sync`, `source_present`). Nothing guards the case where
that set is empty, so an empty main deletes the sink outright. Two reachable ways in:

- The main was added but never scanned — its `FILES` table is empty.
- The main's path is an **unmounted mountpoint**. `update_repo` only refuses when
  the root is not a directory (`update.rs:251`, `:426`), and an unmounted `/mnt/x`
  is normally still an empty directory: the walk finds nothing, every entry is
  marked missing (`update.rs:373`), and the main now looks legitimately scanned
  and empty. A fully absent path *is* already refused (`UpdateError::RootMissing`).

Done:
- `sync_group::guard_mirror_source` refuses a MIRROR plan *and* push when the main's
  `file_count` is 0 (`DiffError::EmptyMirrorSource`). ADD ONLY is exempt — it never
  deletes. Checked once per group, off the maintained META counter, not a scan.
- The confirm prompt now states the real copy and delete counts (it plans first, so a
  refused plan never reaches the dialog) and warns when a push would replace a sink's
  entire current contents. Note it says *replaces*, not *empties*: because the mirror
  guard proves the main is non-empty, its content is always copied in, so a sink can
  never actually end up empty — the earlier "empties completely" wording would have
  been false on every firing.
- `UpdateStats::empty_walk` flags a scan that found no file at all over a non-empty
  index, and `update_repo` emits a progress error naming the directory.
- Tests: `sync_groups.rs::mirror_refuses_a_main_that_was_never_scanned`,
  `::add_only_still_runs_with_an_empty_main`, `::an_empty_walk_is_flagged_and_disarms_mirror`,
  `::a_partial_deletion_is_not_flagged`, `::a_cancelled_push_reports_the_sinks_it_never_reached`; GUI
  `sync_view::run_sync_refuses_to_mirror_from_an_empty_main` and
  `::run_sync_asks_before_pushing_and_says_how_much`.

Deliberately **not** done — the empty-walk scan was left non-destructive-by-warning
rather than by refusal. Keeping the index on an empty walk breaks a legitimate
workflow (emptying a small repo so the deletion propagates — see
`diff_ops.rs::sync_deletes_when_marked_missing_in_a_and_updates_index`), and a
threshold for "too many to be real" would be arbitrary. The files are protected by
the MIRROR guard regardless of how the main got to zero. Follow-up if the index loss
itself proves annoying: a confirmation (GUI) / `--force` (CLI) path before a scan is
allowed to mark *every* entry missing.

### D Error handling, logging & diagnostics  — **done 2026-07-20**

Beta users cannot report what the app never tells them. Today a sync that copied
nothing and failed on every file reports success.

Done:
- `dedup_core::logging` writes one session log per run under
  `$XDG_STATE_HOME/dedup/logs` (default `~/.local/state/dedup/logs`), keeps the newest
  `SESSIONS_KEPT` (10) and prunes the rest. Installed behind the `log` facade, so
  `log::error!` anywhere in the workspace lands in it. Millisecond-stamped filenames so
  two runs in the same second don't truncate each other; flushed per record so a crashed
  session's log is complete. Both entry points (`main.rs`, `gui::run`) initialise it, and
  a log that cannot be opened warns without stopping the app.
- `store::get_config_dir` now honours `$XDG_CONFIG_HOME`, matching `logging::state_dir`.
- Settings gains **OPEN LOG FOLDER** plus the current session's path.
- `util::or_log_default` replaces the silent `unwrap_or_default()` reads (mime stats,
  annotations) so they are logged; the sync-group read in `app.rs` is now surfaced as a
  `load_error` because it changes what the repo list *means*.
- Logged at the points that matter for a bug report: group push start and per-sink
  outcome, mirror refusal, empty-walk scans, GUI push result.
- `logging::capture_panics` routes panics (message, source location, thread) into the log
  and chains to the previous hook, so a worker thread that falls over no longer just
  freezes the window. Installed by `init`.
- Scan results now surface `empty_walk` in the repo row ("FOUND NO FILES AT ALL; check the
  drive is mounted") rather than reading as an ordinary scan, and are logged at warn level
  when the scan errored or found nothing.
- Operation trail: scan start and result, transfer and grooming completions, group push
  start and per-sink outcome.
- Tests: `logging.rs` (pruning, cap counts the new session, records written with level,
  debug filtered out, panic payload shapes, no `/var/log`), `util.rs::or_log_default_*`,
  and `tests/logging_session.rs` — its own process, asserting a real panic reaches the file
  with its source location.

**Checked, not a problem:** the Transfer and Grooming tabs already reported cancellation
and per-item error counts (`transfer_view.rs` `OpResult::Synced`, `grooming_view.rs`), as
did the scan summary. The earlier note that they claimed success was wrong — only the new
`empty_walk` flag was unreported.

- **Surface run outcomes.** *Done for the Sync Groups tab in section C* — `SyncView::start`
  now reports cancellation, per-file `stats.errors`, failed sinks and skipped sinks, and
  says "Sync incomplete" rather than "Sync done" whenever any of those fire. **Still open:**
  the same audit across the other long-running operations (Transfer, Grooming, scans), and
  promoting the one-line status into a proper result panel/modal listing the per-item errors
  instead of a joined string.
- **Report skipped sinks.** *Done in section C* — `run_group_sync` returns
  `SinkOutcome::Skipped` for sinks a cancel never reached, and the GUI names them as stale.
Result panel — done:
- `run_result::{RunReport, ResultModal}` is the shared end-of-run report: counts, the
  individual failures listed one per line (not joined), an amber note for stale sinks,
  and a headline that says "incomplete" whenever anything went wrong. Capped at
  `MAX_PROBLEMS` (200) with an exact overflow count, so a catastrophic run cannot pin the
  heap. Wired into Sync Groups, Transfer (Copy/Move/Sync/Mirror) and Grooming
  (Delete/Empty-dirs/Organize).
- The real gap this closed: the live `run_log` in Transfer/Grooming caps at **10** lines
  (`RUN_LOG_LIMIT`), so on a 10⁵–10⁶-file run a file's error scrolls out within ten more
  files — it was never retained in the UI *or* written anywhere. Both tabs now
  `log::warn!` every `DiffEvent::Error` (full list in the session log regardless of the UI
  cap) and accumulate a capped copy into the report. My earlier "the other tabs are
  already adequate" note was wrong on exactly the large runs where the list matters — the
  advisor caught it against `RUN_LOG_LIMIT = 10`.
- `Pruned` (index maintenance, not per-file) keeps its own status line rather than a
  file-list panel. Single-row APPLY in Transfer keeps the preview, no panel.
- Tests: `run_result.rs` (headline complete/incomplete, cap-with-exact-overflow, modal
  open/close, and a kittest asserting each failure renders on its own line).

### E Code review backlog — 2026-07-20 (`feature/master/qa`)

Correctness:
- `rename_file` (`diff.rs:1472`) writes the new path and removes the old one in two
  separate write transactions. Interrupted in between, the index holds the same
  content under both names: phantom duplicate, inflated `file_count`/`total_size`.
  Both writes belong in one `RepoTables` transaction.

Performance:
- `run_preview_diff` (`transfer_view.rs:1635`) and `run_preview` (`sync_view.rs:654`)
  run full index scans on the egui UI thread. At the 10⁵–10⁶ entries this tool targets
  that is a multi-second freeze on the main workload; move both onto the existing
  `worker.rs` thread + crossbeam channel.
- `plan_group_sync`/`run_group_sync` re-read and re-deserialize the main's entire
  index once per sink. Collect the main's entries and content-key set once before
  the loop.
- `Store::get_sync_group`/`sync_group_of` are full-table scans over a keyed redb
  table; `create_sync_group` scans twice. `get_sync_group` can be a keyed `get`.

Design depth:
- `review::table` infers "single-sided board" from an empty `target_header`
  (`review.rs:221`). A header that is empty for a frame silently drops the target
  columns and rewrites `sort_col`. Model it like the neighbouring `RowControls`:
  an explicit `Option<&str>` or `BoardSides` parameter.
- The sync preview bakes the sink name into `ReviewRow.target_path` as
  `format!("{sink}: {rel}")` (`sync_view.rs:669`), so sorting by target path sorts by
  sink name and a path containing ": " is ambiguous. Give `ReviewRow` a repo/scope field.
- `resolve_in_repo` (`diff.rs:1410`) re-implements the path-escape check already in
  `resolve_subdir` (`diff.rs:62`). Two copies of a security check drift; factor into one.

Cleanup:
- `diff_board.rs` passes the popup's row index through a `thread_local` `PENDING_ROW`
  cell with an `unwrap_or(0)` fallback; `BoardAction::OpenPopup` could just carry
  `row: usize`. (Not currently exploitable — `rows.get()` is bounds-checked and the
  modal blocks background input — but it is hidden global state for one value.)
- Dead arm: `BoardAction::OpenPopup | Inspect` in `start_board_action`
  (`transfer_view.rs:1759`) is unreachable — `board()` returns `None` for popups and
  `Inspect` is intercepted at `transfer_view.rs:1389`.
- `sync_view.rs` defines a private `NoProgress` duplicating
  `dedup_core::diff::NoDiffProgress`; `diff_board.rs` duplicates review.rs's paging
  strip and computes `totals(rows)` twice per frame.







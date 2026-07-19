# Branch review: feature/master/browsing (vs `summer`)

Fix list for the issues found in this branch. Work top to bottom — items are
ordered by user impact. Keep every fix as simple as possible (KISS): prefer
touching the one file named, reuse existing widgets/modules, don't refactor
around a fix. All line numbers are from the current branch head (02948e0).

---

## 1. Review preview: add real paging (user-reported)

**Where:** `crates/dedup-gui/src/review.rs` (shared table), callers in
`transfer_view.rs:33` and `grooming_view.rs:39` (`PREVIEW_CAP = 100_000`).

**Problem:** The preview materialises up to 100 000 `ReviewRow`s in memory and
renders them as one giant virtualised scroll list. A 65 k-entry preview is one
endless scroll with no page controls. The "showing first N of M" note only
appears past the cap, so below 100 k there is no orientation at all. The user
explicitly asked for paging on the results and did not get it.

**Fix (KISS):** Add paging inside `review::table` so both Transfer and Grooming
get it for free:

- Add `page: usize` to `ReviewState`, plus a `PAGE_SIZE` const (500 is plenty —
  the Duplicates tab already pages at 50 groups, follow that precedent).
- After the `visible` index list is built (`review.rs:157`), slice it to the
  current page and render `« PREV | page X of Y | NEXT »` controls (reuse
  `lcars::action_button`) above the table. Reset `page` to 0 whenever the sort
  column/direction or the unchanged-toggle changes, and clamp it when the row
  set shrinks.
- Always show a count line ("N rows matching"), not only when the cap bites.
- Keep `PREVIEW_CAP` as the memory safety net, but with paging it can drop to
  e.g. 10 000; the "refine the filter" note stays for the capped case.

**Acceptance:** preview with > PAGE_SIZE rows shows page controls and a row
count; sorting resorts across the whole sample, not just the visible page.

---

## 2. Organize: template token chips must insert at the cursor (user-reported)

**Where:** `crates/dedup-gui/src/grooming_view.rs:602` — `self.rules[i].template.push_str(insert);`

**Problem:** Clicking an `insert:` chip always appends to the end of the
template. The user expects insertion at the caret, e.g. to add `{camera}` in
the middle of an existing template.

**Fix:** Give the template `TextEdit` a stable id (`.id(...)` derived from the
rule index), then on chip click load its state and insert at the caret:

```rust
let id = egui::Id::new(("organize_template", i));
// TextEdit::singleline(...).id(id)
if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
    let ccursor = state.cursor.char_range()
        .map(|r| r.primary.index)
        .unwrap_or(self.rules[i].template.chars().count());
    // convert char index -> byte index, insert `insert` there,
    // move the cursor to just after the inserted text,
    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(
        egui::text::CCursor::new(ccursor + insert.chars().count()))));
    state.store(ui.ctx(), id);
} else {
    self.rules[i].template.push_str(insert);
}
```

(Adjust to the actual egui 0.35 API — see the `egui-desktop` skill; verify with
an egui_kittest test that types into the field, moves the caret, clicks a chip,
and asserts the token landed mid-string.)

**Acceptance:** with the caret in the middle of the template, clicking a chip
inserts the token at the caret and leaves the caret after it. When the field
has never been focused, appending (current behaviour) is the fallback.

---

## 3. Organize: fallback (`|` alternative) syntax — test it and document it (user-reported)

**Where:** engine `crates/dedup-core/src/organize.rs:297-311`
(`resolve_placeholder`), only explanation is a verbose-mode tooltip at
`grooming_view.rs:583-585`.

**Problem:** The user asked "the alternative value is not explained at all, is
it even working?" — a fair question: `crates/dedup-core/tests/organize_test.rs`
has 5 tests and **none** covers `|` fallbacks or `"literal"` defaults, and the
GROOMING help text (`help_content.rs:69`) never mentions the syntax.

**Fix:**

- Add unit tests in `organize.rs` (a `#[cfg(test)] mod tests` — the file
  currently has zero) for `render_template`: token resolves → token wins;
  empty token falls through to next alternative; `"literal"` always terminates
  (even when empty); all-empty placeholder renders `""`; unclosed `{` is
  copied literally. Fix any behaviour these tests expose as broken.
- Document the syntax where users look: see item 5 (help text).

**Acceptance:** `cargo test -p dedup-core` covers every branch of
`resolve_placeholder`; GROOMING help explains `{a|b|"default"}` in one sentence
with an example.

---

## 4. Organize: rule card layout (user-reported — "abysmal")

**Where:** `crates/dedup-gui/src/grooming_view.rs:558-623` (the per-rule
`section_lcars` body). Screenshot of the defect:
`Screenshot_20260719_145943.png` in the project root.

**Problem (root cause identified):** It is not just an empty *line* — the whole
rule section is stretched to fill the rest of the panel, and the DELETE RULE
button floats alone, vertically centred in a huge black void (see screenshot:
content occupies the top ~250 px, then ~450 px of nothing with the button
mid-right). Cause: `ui.with_layout(Layout::right_to_left(Align::Center), …)`
at line 609 — `with_layout` claims the **entire remaining available rect**
(full height of the panel), and `Align::Center` centres the button vertically
inside it. The `right_to_left` overlap trap (see memory) applies on top when
narrow.

**Fix (KISS):**

- Delete the `with_layout(right_to_left…)` block entirely. Put a compact
  icon-only delete (trash glyph from `icon.rs`, red, with the existing tooltip
  text) on the **same row as the TEMPLATE field**, right after the text edit —
  no dedicated row, no dead space.
- **Nest the per-rule sections inside the RULES elbow**: move the `for i in
  0..rule_count` loop (line 558) *into* the body closure of the "RULES —
  ADD RULES & MANAGE PRESETS" `section_lcars` (line 531). Nested
  `section_lcars` already renders correctly — the FILTER section nests inside
  each RULE in the screenshot — so this is just moving the loop. Result: one
  RULES section containing the + RULE / presets row followed by RULE 1,
  RULE 2, … as inner sections.
- Make each rule section collapsible once item 7 lands — with several rules
  the tab becomes very tall.

**Acceptance:** the rule sections render inside the RULES elbow; no rule
section is taller than its content; the delete control shares a row with the
template field; nothing overlaps at narrow window widths (add a kittest
geometric assert like the existing ones noted in memory).

---

## 5. Help texts: rewrite as user guidance, cover what's missing (user-reported)

**Where:** `crates/dedup-gui/src/help_content.rs`.

**Problem:** The copy reads like the changelog of what was built ("Built on the
same virtualised table…" tone), not help for someone using the UI. Concrete
defects:

- **Drag & drop is never mentioned.** REPOSITORIES help must say: drop folders
  onto the window to add them as repositories.
- **Organize path templates are not explained at all.** GROOMING help mentions
  "templates (date, MIME, camera, original name, …)" but never lists the
  tokens (`{o-path}`, `{o-name}`, `{o-stem}`, `{o-ext}`, `{year}`, `{month}`,
  `{day}`, `{mimetop}`, `{camera}`, `{origin}`, `{size}`), the `{a|b|"lit"}`
  fallback syntax, or that paths are always repo-relative and `..` is rejected.
  Add a short token table + one worked example
  (`{year}/{month}/{o-stem}-{camera|"nocam"}.{o-ext}`).
- **Implementation narration instead of guidance**, e.g. GROOMING: "Every
  file's match is decided up front, before anything moves — so a rename made by
  one rule never causes another rule to gain or lose a match mid-run." Keep the
  user-relevant consequence ("rule order is what matters; each file is placed
  by its first matching rule") and cut the engine internals. Same for BROWSE
  "built entirely from the index" and TRANSFER "live progress and all".
- Style rule (same as the tooltip memory): help text is product copy — tell
  the user what to do and what will happen; never explain how the code works
  or why it was designed that way.

**Acceptance:** each tab's help answers "how do I use this tab" in imperative
voice; drag & drop and the full template syntax are documented; no sentence
describes internals.

---

## 6. Drag & drop: silent failure when the backend gives no paths (user-reported bug)

**Where:** `crates/dedup-gui/src/app.rs:289-306` (`handle_dropped_folders`).

**Problem:** The user reports drops don't work. Two things compound:

1. `filter_map(|f| f.path.clone())` silently discards dropped files whose
   `path` is `None` — which is exactly what some Linux backends deliver
   (Wayland DnD support in winit is recent and partial; the codebase itself
   works around Wayland gaps at `app.rs:1780`). When every path is `None`,
   `dropped.is_empty()` returns early and the user gets **no feedback at all**.
2. There is no discoverability (see item 5) — so a non-working drop looks like
   a missing feature.

**Fix:**

- Distinguish "nothing dropped" from "dropped, but no usable path": read
  `i.raw.dropped_files.len()` first; if it is non-zero but no entry has a
  path, set `load_error` to something actionable, e.g. "Your desktop didn't
  provide file paths for the drop (common on Wayland) — use ADD REPOSITORY
  instead."
- Manually verify on this machine whether drops arrive at all (eframe on
  Wayland, winit 0.30.13): log `dropped_files`/`hovered_files` once, try both a
  Wayland session and XWayland (`WINIT_UNIX_BACKEND=x11` / eframe's x11
  feature). Record the outcome in the commit message; if Wayland delivers
  nothing, the error message above is the honest mitigation.

**Acceptance:** dropping something that can't be handled always produces a
visible notice; a working drop is covered by the existing add-repo flow.

---

## 7. LCARS: make elbow sections collapsible (investigated — feasible)

**Where:** `crates/dedup-gui/src/lcars.rs` (`section_lcars`, `elbow_shapes`).

**Verdict:** Yes, cleanly doable, contained entirely in `lcars.rs` (~50 lines).
The chrome is painted into a reserved shape slot behind a `Frame`, and the
header already has an interact rect (`lcars.rs:153`) — it only needs to become
clickable.

**Approach (KISS, opt-in):** add `section_lcars_collapsible(ui, title, accent,
default_open, add)` (keep `section_lcars` as-is; call sites migrate one by
one):

1. Persist open state with
   `egui::collapsing_header::CollapsingState::load_with_default_open(ctx, id)`
   where `id = ui.id().with(("lcars_sec", title))`. This gives per-section
   sticky state for free; skip openness animation initially (binary
   open/closed is fine).
2. Upgrade the existing title interact from `Sense::hover()` to
   `Sense::click()`; on click, toggle and store the state.
3. When **collapsed**: don't run the body closure; render only the header cap
   bar (full stadium rounding on both ends — reuse the `head` shape from
   `elbow_shapes` with `sw`/`nw` rounded too, no rail, no body panel). The
   collapsed header **must carry a visible expand hint**: a right-pointing
   caret glyph (`icon.rs`) at the left of the title, so it clearly reads as
   "click to open" and not as an empty decorative bar.
4. When **open**: current rendering, plus the caret pointing down.

Caveats to respect:

- Ids derive from the title — for the per-rule sections ("RULE 1 — …") the
  state follows the *position*, not the rule, after add/remove. Acceptable;
  don't over-engineer.
- Skipping the body closure must be safe: bodies only build UI and push `Act`s,
  so it is — but keep any state-syncing code *outside* section bodies when
  migrating call sites.
- Best first uses: the per-rule sections in ORGANIZE (item 4) and the FILTER
  sections in Transfer/Grooming.

**Acceptance:** clicking a collapsible section's header bar hides/shows its
body; state survives tab switches; a kittest asserts the body widgets are
absent when collapsed.

---

## 8. Browse: previews must open the lightbox (user-reported)

**Where:** `crates/dedup-gui/src/browse_view.rs:923-960` (`draw_preview`, the
`Cat::Image | Cat::Video` arm).

**Problem:** The Browse preview dock renders image/video thumbnails as a plain
`egui::Image` — not clickable, no way to reach the lightbox. The lightbox
(zoom, pan, filmstrip) exists but is wired only into the Duplicates tab
(`dupes_view.rs`). Per the one-app consistency rule (see memory
`ui-consistency-one-app`): don't leave Browse with a downgraded variant of a
widget another tab already has.

**Fix:** Make the preview image respond to clicks (`Sense::click` /
`.interact`) and open the lightbox on click. The state types
(`LightboxState`, `FullResCache`) live in `lightbox.rs` and are reusable; the
*rendering* currently lives inside `dupes_view.rs` (~line 1567 on). Extract
the single-image lightbox rendering into a shared function (natural home:
`lightbox.rs`, taking the state + an abs path/texture source) and call it from
both tabs. Do **not** copy-paste the dupes lightbox code into browse_view.
Dupes-specific features (A/B compare, mark-for-delete keys) stay in
dupes_view; Browse only needs view/zoom/pan/close.

**Acceptance:** clicking an image or video preview in Browse opens the
full-window lightbox with zoom/pan and Esc-to-close; the Duplicates lightbox
behaves exactly as before; the shared renderer has one implementation.

---

## 9. LCARS section titles: use the room — explain the section (user-reported)

**Where:** every `section_lcars` call site. Current static titles: `ACTION`,
`COMMAND`, `DEST`, `FOLDER`, `FILTER`, `INTO`, `MANAGE`, `MODE`, `OPTIONS`,
`REPO`, `REPOS`.

**Problem:** The header cap bar spans the full panel width, but most titles
are a single terse word ("REPOS", "DEST", "INTO", "MODE"). The user asked for
more verbose headers — there is plenty of horizontal room to state the
section's purpose. The organize sections already do this right:
"RULES — ADD RULES & MANAGE PRESETS", "RULE 1 — MATCH & RENAME".

**Fix:** Rename every terse title to the established `SHORT — PURPOSE`
pattern, e.g. (adjust wording to each section's actual content):

- `REPOS` → `REPOS — CHOOSE WHICH REPOSITORIES TO SEARCH`
- `DEST` → `DEST — WHERE MATCHING FILES GO`
- `INTO` → `INTO — TARGET REPOSITORY`
- `MODE` → `MODE — WHAT TO FIND`
- `FILTER` → `FILTER — NARROW WHICH FILES COUNT`
- `ACTION` → `ACTION — PREVIEW & RUN`

Same product-copy rule as items 5: describe purpose for the user, no
implementation talk, ALL-CAPS to match the LCARS chrome. `elbow_shapes`
already widens the chrome if a title outgrows the panel (`lcars.rs:176`), and
long titles still get truncated visually on very narrow windows — keep the
short keyword **first** so a clipped title stays identifiable.

**Acceptance:** no single-word section title remains; titles follow
`SHORT — PURPOSE`; narrow-window kittest snapshots still pass.

---

## Minor findings (fix opportunistically, lowest priority)

- **`release.sh` token on the curl command line** (`packaging/release.sh`,
  `curl -u "upload:$DEDUP_PI_TOKEN"`): the token is visible in the process
  list for the duration of the upload. On a single-user CI box this is low
  risk; if touched anyway, feed it via `curl --config -` on stdin. Everything
  else in packaging/CI (scoped file secret, tag-gated upload, `set -euo
  pipefail`, `--locked` builds) looks sound.
- **Duplicated constants**: `PREVIEW_CAP` and `RUN_LOG_LIMIT` are defined in
  both `transfer_view.rs` and `grooming_view.rs`. When item 1 moves paging
  into `review.rs`, move `PREVIEW_CAP` there too.
- **Help copy hardcodes tunables**: "Results page 50 groups at a time"
  (`help_content.rs:46`) will silently rot if the constant changes. Either
  drop the number or format it from the constant.
- **`create_repo` uses `to_string_lossy`** on dropped paths
  (`app.rs:318`): a path with invalid UTF-8 is silently mangled. Extremely
  unlikely; a note, not a task.

## Explicitly checked, no action needed

- `review.rs` itself: clean separation (presentation-only), tested, truncating
  path cells, true totals independent of the cap — good module.
- Core `normalize_rel` rejects `..` escapes and collision-renames instead of
  overwriting — the safety story for ORGANIZE is right.
- CI manifests (`.builds/*.yml`): secrets handled correctly (file secret, not
  exposed to forks), tests run on core+cli, GUI tests correctly excluded on
  the headless runner.

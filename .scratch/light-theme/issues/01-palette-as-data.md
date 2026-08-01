# 01 — Turn the palette into data, with the dark appearance provably unchanged

Status: resolved
Spec: ../spec.md

## Problem

The palette is twelve `const Color32` values compiled into the binary and named from roughly
737 places. There is no seam at which a second appearance could be chosen, so the light theme
cannot exist. This ticket creates that seam and changes nothing a user can see.

## Approach

Introduce a palette type holding the twelve colours, install one in a **`thread_local`**, and
replace each constant with an accessor function reading it.

- **`thread_local`, not a `static`.** Tests run in parallel; a shared global would let a test
  asserting light colours race one asserting dark, presenting as intermittent colour or layout
  failures. Thread-local makes that impossible by construction.
- **No signature changes.** Colours are read from free functions in the board, repo-chip,
  media-cell, LCARS and util modules, which have no application state to thread a palette
  through. Accessors are what keeps this a rename rather than a refactor.
- The dark palette keeps **exactly** its current values. This ticket ships one palette.
- The style-application entry point takes the palette to install rather than hardcoding a dark
  visual set. It still installs dark, because dark is all there is yet.
- `PILL` is a corner radius, not a colour: leave it a constant.

The rename is mechanical and the compiler finds every site. Do it in one pass; a partially
renamed module is harder to review than a large uniform diff.

## Seam and tests

Theme-module unit tests, the highest available seam — the palette is data now, so these are
pure functions:

- every accessor returns the value the dark palette was defined with (pins the appearance so
  later tickets cannot shift it by accident)
- installing a palette and reading an accessor returns the installed value
- two threads installing different palettes do not observe each other's — the property that
  makes the parallel test suite safe

No new seam. The existing GUI tests are the regression net for the rename: they must stay green
**unmodified**, since nothing a user sees has changed.

## Done

Standing gate green (`cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`). No
user-visible change, so no `CHANGELOG.md` entry; note the mechanism in `ai/improvements.md`.

## Comments

**Implemented 2026-08-01, TDD.** Gate green: fmt clean, clippy 0 warnings,
`cargo test --workspace` 24 suites / 0 failures.

Assessed as **legacy**: `theme.rs` had no test module at all, so ~737 call sites depended on
twelve entirely unpinned constants. Characterization first, then refactor.

Five cycles:

1. **RED→GREEN** `text()` reads the dark palette's value — invents `Palette`, the
   `thread_local` and one accessor. One colour, because this cycle's job was the mechanism.
2. **RED→GREEN** every accessor reads the dark value — the characterization proper, twelve
   colours in one test. Refactored the twelve near-identical accessors into a macro in the
   same step.
3. **RED→GREEN** installing a palette changes what the accessors read — the new behaviour the
   whole ticket exists for.
4. `thread_local` isolation across threads. **Honest note: this one passed on first run** — the
   implementation already had the property, so it is a regression guard on a decision, not a
   RED→GREEN cycle. It earns its place by failing if anyone later "simplifies" the
   `thread_local` into a `static`.
5. **RED→GREEN** `apply` installs the palette it is given, and egui's own chrome uses it — so
   the style egui draws with and the colours the app draws with cannot disagree.

Then the mechanical rename, with the four tests plus the existing 263 as the net. It compiled
clean on the first pass. One self-inflicted wound worth recording: the sed that added the
palette argument to `apply` used `[^)]*`, which stopped at the inner paren of `ui.ctx()` and
produced `apply(ui.ctx(, DARK))` at ~95 sites. The compiler caught it immediately and it was
repaired in one pass — but a regex over call sites containing nested parens is a trap.

**Strongest evidence the appearance is unchanged:** re-rendering `docs/screenshots/board.png`
produced a **byte-identical** file. No assertion could show that as well.

REFACTOR: the twelve raw colour constants became private. Nothing outside the module needed
them after the rename, and keeping them public would let new code bypass the active palette —
a future `theme::TEXT` now fails to compile.

**Not committed** — this repo's standing rule keeps git writes with the user.

# Light theme

Status: ready-for-agent

Spec produced by a grilling session on 2026-08-01. Every decision below was put to the user
and chosen by them; the rationale recorded here is the reason given at the time, not a
reconstruction. Facts were verified against source and against the vendored egui 0.35.

## Problem Statement

dedup-rs is dark-only. Some people prefer light interfaces, some work in bright rooms, and
some simply have their desktop set to light and expect applications to respect that. Offering
one appearance and no choice is, in the user's words, "like only being able to run on Kubuntu
22.04 with X.org" — it is not a taste question, it is table stakes.

The obstacle is that appearance is not configurable at all: the palette is twelve `const`
values compiled into the binary, referenced from roughly 737 places, and the egui style is
installed once at startup as `Visuals::dark()`. There is no seam at which a different
appearance could be chosen, so the preference cannot exist.

## Solution

Two hand-made palettes — the existing dark one, and a light one designed alongside it — with a
three-way **System / Light / Dark** preference in Settings that takes effect immediately and is
remembered between sessions.

Dark stays the default for everyone who has not chosen otherwise, so nobody's interface changes
under them on upgrade. Someone who wants light picks it once and the application obliges,
without a restart.

## User Stories

1. As someone who prefers light interfaces, I want to switch dedup-rs to a light appearance, so that I am not forced into a dark one to use the tool.
2. As someone who prefers dark interfaces, I want dedup-rs to stay exactly as it is, so that adding a light option costs me nothing.
3. As a user working in a bright room, I want a light appearance, so that the screen is readable in daylight.
4. As a user whose desktop is set to light, I want the option to have dedup-rs follow it, so that it does not stand out from every other window.
5. As a user whose desktop switches between light and dark on a schedule, I want the application to follow, so that I do not have to change it by hand twice a day.
6. As a user who wants one specific appearance regardless of the desktop, I want to pin Light or Dark explicitly, so that the system preference does not override my choice.
7. As a user changing the setting, I want it to apply immediately, so that I can see the result and decide.
8. As a user changing the setting, I want it remembered next launch, so that I choose once.
9. As an existing user upgrading, I want my interface to look exactly as it did, so that an update never silently repaints my tools.
10. As someone triaging files, I want the four review-board colours to remain distinguishable in whichever appearance I choose, so that "will be deleted" never reads as "differs".
11. As someone triaging files, I want red to keep meaning deletion in both appearances, so that the colour vocabulary I have learned still holds.
12. As someone triaging files, I want green to keep meaning "only on this side", so that an addition is never mistaken for a deletion.
13. As someone triaging files, I want amber to keep meaning "conflict or renamed", so that an ambiguous row still looks ambiguous.
14. As someone triaging files, I want grey to keep meaning "unchanged", so that the rows I can ignore stay quiet.
15. As a user of the light appearance, I want text to be comfortably readable against the background, so that long triage sessions do not tire my eyes.
16. As a user of the light appearance, I want repository identicons to be legible, so that I can still tell repositories apart at a glance.
17. As a user of the light appearance, I want the LCARS pills and elbow rails to still look deliberate, so that the application does not feel like an unfinished port of itself.
18. As a user of the light appearance, I want thumbnails and previews to sit properly against the background, so that images do not appear to float.
19. As a user of the light appearance, I want disabled and protected controls to still read as disabled, so that I do not click something that cannot act.
20. As a user of either appearance, I want the selected tab, pill and toggle states to be obvious, so that I always know what is active.
21. As a user, I want the appearance control to live with the other preferences, so that I know where to look for it.
22. As a user, I want the control to name its three options plainly, so that "System" is not a mystery.
23. As a developer, I want the palette to be data rather than constants, so that a second appearance is expressible at all.
24. As a developer, I want to add a colour without touching hundreds of call sites, so that the palette stays maintainable.
25. As a developer, I want a test to fail when two semantic colours become indistinguishable, so that a palette tweak cannot quietly break the board's vocabulary.
26. As a developer, I want to see both palettes side by side in one image, so that I can judge them together rather than from memory.
27. As a developer, I want the existing dark appearance pinned by tests, so that introducing the light one cannot alter it by accident.
28. As a developer, I want tests to remain runnable in parallel, so that the suite does not become slow or flaky.
29. As the maintainer, I want user-chosen arbitrary colours to stay out of scope, so that nobody can configure the application into unusability.

## Implementation Decisions

**Decided in session, in order:**

1. **A genuine light palette, hand-derived — not an algorithmic inversion.** The four semantic
   board colours become dark variants of grey, green, red and amber. The remaining decorative
   colours are chosen to suit, and the LCARS pills and elbow rails are tried on a light
   background and adjusted by eye. The user is the customer for light mode and their judgement
   is the acceptance test for the values themselves.
2. **The palette becomes data behind accessor functions, backed by a `thread_local`.** Each
   colour constant becomes a function reading the currently installed palette. Every call site
   changes name; **no function signature changes**. This is the decisive constraint: colours are
   read from free functions in the shared board, repo-chip, media-cell, LCARS and util modules,
   which have no application state to thread a palette through, and several already take many
   arguments.
3. **`thread_local`, specifically — not a process-wide `static`.** Rust runs tests in parallel.
   With a shared global, a test asserting light colours and one asserting dark would race, and
   the failures would present as intermittent layout or colour bugs. A thread-local palette
   makes the problem structurally impossible.
4. **Switching is live; no restart.** Nothing bakes a theme colour into a cached texture — the
   repository identicon is painter-drawn every frame — so applying a palette is: install it,
   re-apply the egui style, request a repaint. A restart requirement was offered and declined
   because it would buy no engineering simplification while costing the user.
5. **The preference model is egui's own.** egui 0.35 already provides a three-way theme
   preference, exposes the operating system's setting, and supports registering a separate style
   per theme. No preference enum is invented and no platform detection is written; the
   application supplies two palettes and lets egui resolve which is active.
6. **Dark is the default.** Light is strictly opt-in. Colour here is *semantic* — red means
   files will be deleted — so an existing user on a light desktop must not launch after an
   update into a repainted colour vocabulary, least of all the delete-versus-differs distinction
   that is the most fragile pair. A "system default for new installs only" variant was
   considered and rejected: behaviour that differs between fresh installs and upgrades makes bug
   reports irreproducible.
7. **The control lives in the existing Settings modal**, beside tooltip verbosity, as a plain
   three-way System / Light / Dark choice, persisted with the other settings.

**Modules affected:** the theme module (palette type, accessors, style installation); the
settings module (the persisted preference); the settings modal (the control); the repo-chip
module (identicon legibility); and mechanically, every module that names a colour.

**Interfaces changed:** colour constants become accessor functions; the style-application
entry point stops hardcoding a dark visual set and instead registers a style per theme and
installs the matching palette.

**Known breakage to handle rather than discover:**

- The identicon paints a **hardcoded dark tile** — its comment says a dark tile makes the
  pastel cells legible — which on a light chip is a black square.
- Its hues are high-lightness pastels chosen for dark backgrounds and will be low-contrast on
  light.
- Amber and tan are already near-neighbours in the dark palette and converge further when
  darkened; this is the pair most likely to fail the distinctness check.

## Testing Decisions

A good test here asserts what a user of the module observes, not how it is implemented. This
change is unusually favourable to that, because **turning the palette into data makes most of
the important questions pure functions**: "is the delete colour distinguishable from the
differs colour on a light background" needs no window, no harness and no rendering.

**No new seams.** All four already exist:

| Seam | Used for | Prior art |
| --- | --- | --- |
| Theme-module unit tests | palette contrast and semantic distinctness, in both palettes | new tests in the existing module |
| Board inline tests | the four status colours stay mutually distinct under either palette | `the_four_statuses_have_distinct_colours` |
| Settings inline tests | the preference round-trips and its serialized tag is stable | `settings_round_trip_and_default_on_missing`, `tooltip_verbosity_json_tag_is_stable` |
| `#[ignore]`d render test writing a PNG | the both-palettes image the user judges the colours from | `doc_screenshot_board` |

**Specific decisions:**

- The existing four-status distinctness test becomes a **loop over both palettes** rather than a
  second test, so a new palette cannot be added without being checked.
- Distinctness is asserted as a **minimum perceptual distance**, not mere inequality: two
  colours that differ by one channel value are "distinct" and still indistinguishable on screen.
- Text-against-background contrast is asserted for each palette, so a light palette cannot ship
  with unreadable body text.
- **Documentation screenshots stay dark.** One new render test emits both palettes in a single
  image as the eyeball target. Regenerating all ~14 documentation screenshots in both
  appearances was considered and rejected: it doubles the images in the repository and makes
  every future interface change cost two regenerations.
- The dark palette's existing values are pinned, so introducing the light one cannot shift them.

## Out of Scope

- **User-configurable colours.** Raised and withdrawn in the same breath by the user, and agreed
  out: arbitrary persisted colours need contrast validation, or someone sets text to the panel
  colour and has no way back. Two curated palettes only.
- **A high-contrast or accessibility-specific appearance.** A separate axis with a different
  driver; not this feature.
- **Per-tab or per-view appearance.** One choice, applied everywhere.
- **Theming anything that is not a colour.** Corner radii, spacing, fonts and the condensed
  LCARS typeface are unchanged.
- **Regenerating the documentation screenshots in light.**
- **The spectrogram and waveform colour ramps**, which are their own scale keyed to signal
  intensity rather than to the interface palette.
- **Any change to what the colours mean.** The review board's vocabulary — grey unchanged,
  green only-on-this-side, red will-be-deleted, amber differs — is preserved exactly; only its
  rendering changes.

## Further Notes

- The user is the customer for the light palette and will choose its twelve values by looking at
  the both-palettes image and iterating. The spec deliberately does not fix them.
- The mechanical rename of roughly 737 call sites is large but low-risk: it changes no
  signatures, and the compiler catches every site. It should land on its own, with the dark
  appearance provably unchanged, before any light values exist.
- This work was deferred once before, on 2026-07-08, on the grounds that it "requires converting
  the theme constants to a runtime palette". That remains true and is exactly what the first
  ticket does; what has changed is that a reason to pay the cost has been articulated.
- Each ticket's definition of done includes the standing gate: `cargo fmt --check` clean,
  `cargo clippy -- -D warnings` clean, `cargo test` green, and documentation updated in the same
  change rather than as a follow-up.

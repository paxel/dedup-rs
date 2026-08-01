# One lightbox: the two-up analysis surface

Status: resolved

Spec produced by a grilling session on 2026-08-01, driven by the user's own testing of the
committed backlog batch. Every decision below was put to them and chosen by them; the
rationale recorded is the reason given at the time. Facts were verified against source.

## Problem Statement

Looking closely at two files is the core act of this tool, and it behaves differently
depending on where you started from.

There are **three** viewers. The Duplicates tab has a tabbed one that can compare, play audio
and edit tags. Transfer's DIFF has a second, which gained tabs only yesterday. Browse has a
third that shows a single image with no tabs and no comparison at all. A review board row has
no viewer: clicking a row does nothing.

The consequences are visible in the user's own screenshots:

- Comparing steps out of the tabbed screen onto a different one with a different set of
  controls, so the metadata tab disappears mid-comparison, the CLOSE button changes colour, and
  the Overview button moves and loses its icon.
- The switcher walks one side through every member of the group, including the file the other
  side is showing — so a two-file group offers "1/2, 2/2" and position 2 compares a file with
  itself. That is not a display glitch; it is the navigation model.
- Repository badges sit in a top bar, visually detached from the image they describe.
- Controls drift downward the further right they sit, and the rightmost is clipped off the edge.
- The overlay is not opaque, so the tab underneath reads through the photographs.
- A file that cannot be rendered as a picture cannot be opened for comparison at all, even
  though its bytes and its metadata are exactly what a forensic comparison would need.

Underneath all of it: the same job is implemented three times, so every improvement must be
made three times or the surfaces drift further apart.

## Solution

**One lightbox, reachable from everywhere, for any file.**

Clicking any file — in a duplicate group, on a review board row, in Browse — opens the same
screen. It shows one file, or two side by side. The row of tabs across the top says what you
are looking *at* — the picture, the sound, the metadata, the text, the raw bytes — and never
changes shape, so comparing is something that happens *inside* a tab rather than a different
screen you travel to.

Each side carries its own title with the facts that identify it and its own delete control.
Each tab brings the tools that suit it: rotate and mirror for pictures, play and speed for
sound, search for text. Rotating a picture and then comparing keeps the rotation, because
finding the copy somebody flipped is the whole point.

When there are more than two candidates, a switcher on each side moves that side through them
and cannot land on the file the other side already shows.

## User Stories

1. As someone triaging files, I want clicking any file to open the same viewer, so that I learn one screen instead of three.
2. As someone triaging files, I want to open a file that is not a picture, so that documents and unknown formats can be examined too.
3. As someone triaging files, I want the viewer to open from a review board row, so that I can see what a plan will do before running it.
4. As someone browsing a repository, I want the same viewer with the same abilities, so that Browse is not a lesser tab.
5. As someone comparing two files, I want the tab row to stay put when I start comparing, so that nothing jumps under my cursor.
6. As someone comparing two files, I want the metadata still reachable while comparing, so that I do not have to leave the comparison to check a capture date.
7. As someone comparing two files, I want the controls to stay in fixed positions, so that I can build muscle memory.
8. As someone comparing two files, I want every control fully visible, so that nothing useful is clipped off the edge of the window.
9. As someone comparing two files, I want the background fully covered, so that the tab underneath does not read through the images.
10. As someone comparing two copies, I want each side labelled with its path, size, dimensions, date and repository, so that I always know which file I am about to act on.
11. As someone comparing two copies, I want the delete control beside the file it deletes, so that I cannot mark the wrong one.
12. As someone comparing two copies, I want the repository shown with its file rather than in a distant bar, so that the association is obvious.
13. As someone with a two-file group, I want no switcher at all, so that I cannot accidentally compare a file with itself.
14. As someone with a large group, I want a switcher on each side, so that I can change either side independently.
15. As someone with a large group, I want a side's switcher to skip the file the other side shows, so that the two sides are never the same file.
16. As someone with one file open, I want to hide the second side, so that a single file can fill the screen for close analysis.
17. As someone examining a photograph, I want to zoom and pan, so that I can inspect detail.
18. As someone examining a photograph, I want to rotate and mirror it, so that I can match a copy somebody flipped.
19. As someone comparing photographs, I want my rotation carried into the comparison, so that the two are aligned when I compare them.
20. As someone comparing photographs, I want to rotate and mirror inside the comparison, so that I can align them while looking at both.
21. As someone comparing photographs, I want to flicker between them in place, so that a subtle edit becomes obvious.
22. As someone comparing recordings, I want to play either side, so that I can hear which is which.
23. As someone comparing recordings, I want to change playback speed, so that I can examine a passage closely.
24. As someone comparing recordings, I want playback to keep its paused state when I switch files, so that stepping through does not start music I stopped.
25. As someone comparing recordings, I want the switch between them to be gapless, so that a difference in the audio is audible rather than masked by a pause.
26. As someone examining a tagged file, I want its metadata shown, so that I can compare capture dates or track numbers.
27. As someone examining an image, I want its EXIF shown read-only, so that I am not offered an edit the tool cannot perform.
28. As someone examining audio, I want to edit its tags, so that I can correct them where the format allows.
29. As someone examining any file, I want a text view, so that I can read what is readable regardless of type.
30. As someone examining any file, I want a raw byte view, so that I can inspect a header or a trailer.
31. As someone comparing two files as bytes, I want the two panes scroll-locked, so that I am looking at the same offset in both.
32. As someone comparing two files as bytes, I want to flicker between them, so that differing bytes stand out from a wall of hex.
33. As someone comparing two files as bytes, I want differing bytes highlighted where that is cheap to provide, so that I can see immediately where they diverge.
34. As someone examining a sampled view, I want to be told it is a sample, so that I do not conclude two files are identical from their first kilobyte.
35. As someone acting on a file, I want the actions to belong to the place I came from, so that the Duplicates tab offers marks and a review row offers its own commands.
36. As someone in a read-only repository, I want deletion controls disabled and visibly so, so that I do not attempt something that cannot happen.
37. As a developer, I want one implementation of the viewer, so that an improvement lands everywhere at once.
38. As a developer, I want the old viewers deleted in the same change, so that no variant survives to drift.
39. As a developer, I want the viewer to know nothing about duplicate groups, so that any caller can supply a pair.
40. As a developer, I want the behaviours we recently fixed to keep passing their existing tests, so that the rewrite provably loses nothing.

## Implementation Decisions

**Decided in session, in order:**

1. **One implementation.** The Transfer-side comparison surface survives and absorbs the
   others; the Duplicates viewer and its separate audio viewer, and the Browse single-image
   viewer, are deleted. It is chosen as the survivor because it already accepts an arbitrary
   pair with caller-supplied actions and knows nothing about duplicate groups — it is the one
   that generalises.
2. **Big bang, not incremental.** Both-alive migration was offered and rejected by the user:
   *"we do this for the xths time, keeping variants is not helping. git has the history if
   something breaks."* This session exists because a previous epic left a variant behind, so
   the argument is evidence-based. The deletion of the old viewers is part of the same change.
3. **The law: clicking any file anywhere opens it** — duplicate group, review board row,
   Browse. Explicitly including files that cannot be rendered as pictures. Previewability
   stops gating *opening* and only decides which tabs appear.
4. **At most two files.** The group view already exists upstream (duplicate cards, Browse
   listing, both sides of a review row); rebuilding it inside the viewer would be a third place
   showing the same thing. Hiding the second side gives the first the whole screen.
5. **The tab row is always present, and comparing is a mode of the current tab** — not a second
   screen. This is the structural fix for three separate complaints at once: the disappearing
   metadata, the shifting and recolouring controls, and the migrating repository badges.
6. **Tabs:** picture, sound, video, metadata, text, raw bytes. Text and raw bytes are offered
   for **every** type — a change from today, where they are offered only for files that are not
   image, audio or video. The viewer opens on the pair's own representation.
7. **No overview tab.** Its content is redistributed: identifying facts go into each side's
   title, EXIF goes to metadata. The user's earlier instinct to keep it was withdrawn once the
   group view was established as living upstream.
8. **Metadata survives**, on a forward-looking argument from the user rather than present need:
   many more formats are coming and metadata will matter for them. Read-only for images (there
   is no EXIF writer); editable for audio, as today.
9. **Per-side titles carry the identifying facts** — path (truncatable), size, dimensions, date,
   repository — replacing the legend that currently sits at the bottom, and reusing that space.
   The per-side delete control moves there too.
10. **Tools belong to the tab and vary by type:** rotate, mirror and fit for pictures; play,
    pause and speed for sound; search for text and documents. Rotation and mirroring carry into
    the comparison and remain available inside it, because identifying a flipped copy is a
    primary use.
11. **Flicker is a shared primitive, not an image feature.** It is an A/B swap in place, so it
    serves pictures, hex and text alike — one mechanism rather than three.
12. **Raw-byte comparison is two panes side by side, scroll-locked, with flicker.** Highlighting
    the differing bytes is a welcome bonus if it comes cheaply, not a requirement.
13. **A switcher per side**, moving that side through a caller-supplied pool of candidates —
    group members for duplicates, the listing for Browse, nothing meaningful for a review row
    (which offers one or two files, so no switcher renders). A switcher **skips** the file the
    other side is showing, so the counter is a position in the pool with a hole in it; when the
    pool holds two or fewer, no switcher is rendered at all and self-comparison is impossible.
14. **The comparison command comes off review board rows.** With every row opening the viewer,
    a row-level command is a second door to the same place. This undoes part of a ticket
    completed the previous day.

**Interfaces:** the viewer takes a first file, an optional second, an optional pool of
candidates, and the caller's own actions. It returns what the user chose. It has no knowledge
of duplicate groups, review rows or repositories beyond what a file carries.

**Fixed in passing**, all observed in the user's screenshots: the overlay is not opaque enough
and lets the underlying tab read through; controls drift vertically toward the right of the bar;
the rightmost control is clipped at narrower widths.

## Testing Decisions

A good test here asserts what someone using the screen observes — which tabs are offered, which
file each side shows, whether a control is present and enabled — not how the viewer is
structured internally. That matters unusually much for this change, because the point is to
delete two implementations: any test written against the internals of the surviving one will
obstruct the next refactor rather than protect this one.

**One seam replaces three.** Assertions are made against **the viewer driven with a supplied
pair and pool**, not against any caller. The same test then covers all four entry points, and
the existing per-caller viewer tests collapse into a single suite.

| Seam | Used for | Prior art |
| --- | --- | --- |
| The viewer's own inline tests, driven with a pair and a pool | tabs offered per type; the landing tab; switcher skipping and absence at two; per-side titles; per-side marks; the tools each tab brings | the existing DIFF-side representation tests, and the audio viewer's open/compare/play/escape test |
| Each caller's existing inline tests | that Duplicates, review rows, Browse and DIFF each open the viewer and receive their own actions back | the four view test modules |
| `#[ignore]`d render tests writing a PNG | layout: the overlay fully covers, controls share a baseline, nothing clips at narrow widths | the existing lightbox and board screenshots |

**The regression constraint is the spec's main safety mechanism.** These behaviours were bought
recently and at cost, and their existing tests must keep passing **unmodified** against the new
viewer:

- gapless A/B audio switching
- playback staying paused across a file switch, with the newly shown copy loaded
- each copy of a four-file audio group showing its own tags
- the switcher walking three distinct others in a four-file group
- independent per-copy delete marks, and protected repositories rendering struck through

Needing to rewrite any of them is the signal that the rewrite lost something, and should be
treated as a failure rather than an inconvenience.

**Deliberately not asserted:** exact colours, pixel positions, or the internal shape of the
viewer's state. Layout claims are made geometrically (rectangles inside their region, controls
sharing a baseline) and confirmed by looking at a rendered image, per this repository's standing
lesson that a label query passes even when a widget is clipped.

## Out of Scope

- **Pagination of the text and raw-byte views.** Today's sampled head stays; the view must
  *say* it is a sample rather than implying it read the whole file. Paging through a large file
  is wanted later.
- **Renderers for PDF and office documents**, and searchable side-by-side text. Named as
  future direction; the text and byte views serve those formats until then.
- **Highlighting differing bytes** — a bonus if cheap, not a requirement of this spec.
- **An EXIF writer.** Image metadata stays read-only.
- **Cross-type comparison as a deliberate feature.** The machinery is type-agnostic, but no
  surface sets out to compare a photograph against a spectrogram.
- **Marking files across several places to open together.** Raised as a possibility; not
  specified here.
- **The light theme.** Tickets 02–05 of the light-theme effort are deferred behind this work —
  there is no point colouring a surface about to be replaced.
- **Changes to how duplicates are found, grouped or scored.** This is the viewing surface only.

## Further Notes

- The user is the customer for this surface and tested the current one by hand; the screenshots
  in `target/` from 2026-08-01 are the evidence base for the layout complaints and should be
  consulted before the layout work.
- The switcher defect that started this session — a two-file group offering positions 1/2 and
  2/2, where 2/2 compares a file with itself — is fixed by the navigation model rather than
  patched. It cannot recur once a pool of two renders no switcher.
- The surviving viewer is the smallest of the three by a wide margin, so this is mostly
  *moving capability into* it rather than rewriting from nothing. The audio player is the
  largest and most delicate piece and carries the most regression tests.
- Each ticket's definition of done includes the standing gate: `cargo fmt --check` clean,
  `cargo clippy -- -D warnings` clean, `cargo test` green, and documentation updated in the
  same change.

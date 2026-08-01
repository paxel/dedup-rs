# 04 — Keep playback paused when stepping to the next audio file

Status: resolved
Spec: ../spec.md

## Problem

Reported from real use: "when I click pause the music stops; when I click next the playing
continues. The play state is not checked when resuming play on the new file."

Stepping to another file in the audio lightbox starts playback regardless of whether the user
had deliberately paused. Verified: nothing in the Duplicates view consults a prior
playing/paused state when loading the next file.

## Approach

Treat playing-or-paused as state that survives a file switch. When the lightbox moves to
another audio file, the new file adopts the previous file's transport state: previously
playing continues playing, previously paused stays paused at the start of the new file.

Take care not to regress the gapless A/B flip while comparing, which deliberately keeps a
pre-loaded paired stream and flips between the two rather than re-seeking from disk. That
path has its own regression test; it must stay green.

## Seam and tests

GUI seam — inline `ui_tests` in the Duplicates view, prior art the existing audio lightbox
tests that open, compare, play and escape:

- pause, then step to the next file: the new file is not playing
- play, then step to the next file: the new file is playing
- the existing gapless-flip assertions still hold while comparing

## Done

Standing gate green. `CHANGELOG.md` and `ai/improvements.md` updated.

## Comments

**Implemented 2026-07-31.** Gate green on the new machine (Rust 1.97.1): `cargo fmt --check`
clean, `cargo clippy --workspace --all-targets` 0 warnings, `cargo test --workspace` 24 suites
/ 0 failures.

**The defect was worse than the report described.** The nav branch already consulted
`snap.playing`, so it did not wrongly *resume* — it did nothing at all when paused, which left
the **previous** file loaded in the player. The lightbox therefore showed copy N+1 while the
player still held copy N, and pressing play resumed the wrong file. That is the actual bug
behind "the pause is back when switching the mp3s".

Fix: `Player::load_paused()` plus a `paused` flag on the internal `Cmd::Play`. The audio
thread calls `play()` then immediately `pause()` — a rodio sink primes at the right position
that way, so resuming is instant. The nav branch now loads the new copy on *both* paths:
playing continues on the new file, paused loads it and stays paused.

Test: `stepping_while_paused_loads_the_new_copy_without_resuming` asserts both halves — the
pause survives, and the newly loaded hex is the copy now shown. It drives the paused state
through the player API rather than a key press, because the audio thread corrects `playing`
from the real sink and a headless run has none, which made key-driven pausing
non-deterministic. **Verified as a real regression test**: reverting the fix makes it fail,
restoring it makes it pass — checked on this machine, not just inherited from the previous one.

The existing gapless A/B flip regression test stays green, so the paired-stream path is
unaffected.

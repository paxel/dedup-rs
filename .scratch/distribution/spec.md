# Distribution — GitHub CI, Windows/macOS images, package channels

Status: resolved

Implemented 2026-08-12 (uncommitted, awaiting the user's review/commit/push). The
CI/release workflows' acceptance test is by design the first green Actions run
after the push; everything testable locally is green (workspace suites, packaging
scripts, template rendering).

Spec synthesized 2026-08-12 from a grilling session ("making this app a real app
with windows and mac images and ci on github and deployment to some package
maintainers"). All decisions below were confirmed one by one in that session.

## Problem Statement

The app is usable now, but nobody can install it. It builds only where Rust is
set up by hand; the only CI lives on sourcehut ("too barren for me to care"),
producing a `.deb` for a private Pi apt pool and nothing else. There are no
Windows or macOS builds at all, no releases page, no package-manager story. A
usable app that cannot be installed is still just a repository.

## Solution

GitHub becomes the project's home: Actions CI gates every push on three
operating systems, and pushing a version tag produces a complete release —
a Debian package and tarball for Linux, a zip with a proper double-clickable
GUI executable for Windows, an unsigned `.app` bundle in a `.dmg` for macOS —
published as a GitHub Release, pushed to crates.io, and propagated to the
project's own Homebrew tap and Scoop bucket. The sourcehut CI and the Pi apt
pool are retired. A release becomes: `git tag v0.1.0 && git push --tags`.

## User Stories

1. As a Linux user, I want to install the app from a `.deb`, so that my package
   manager tracks its files and can remove it cleanly.
2. As a Linux user without dpkg, I want a plain tarball with the binary, so
   that I can install it anywhere.
3. As a Windows user, I want to download a zip, unpack it, and double-click an
   exe that opens the app without a black console window behind it, so that it
   feels like a real application.
4. As a Windows power user, I want a console `dedup.exe` beside the GUI exe, so
   that the CLI works with normal stdout/stderr in my terminal.
5. As a macOS user, I want a `.dmg` containing a real `.app` bundle with an
   icon, so that I can drag it to Applications like any other app.
6. As a macOS user on an unsigned build, I want the README to tell me about
   right-click → Open past Gatekeeper, so that the first launch doesn't look
   broken.
7. As a Rust user, I want `cargo install dedup-rs-cli` to give me the `dedup`
   binary, so that I can install straight from crates.io.
8. As a Homebrew user, I want `brew install paxel/tap/dedup`, so that macOS
   installation is one command.
9. As a Scoop user, I want the app in a bucket I can add, so that Windows
   installation and updates are one command each.
10. As the maintainer, I want every push and pull request gated by formatting,
    clippy with zero warnings, and the full test suite, so that the main branch
    is always releasable.
11. As the maintainer, I want the CI to run the GUI's headless render tests
    (software Vulkan) and the tool-gated tests (ffmpeg, poppler) on Linux, so
    that a layout or extraction regression cannot slip through.
12. As the maintainer, I want Windows and macOS lanes to at least build the
    workspace and run the non-GUI tests on every push, so that a
    platform-breaking change surfaces in minutes, not at release time.
13. As the maintainer, I want a release to be triggered by pushing a version
    tag and nothing else, so that version, tag, and artifacts can never drift.
14. As the maintainer, I want the GitHub Release notes taken from the
    changelog's section for that version, so that release notes are written
    once, in one place.
15. As the maintainer, I want the release workflow to publish the three crates
    in dependency order, so that crates.io always has a consistent set.
16. As the maintainer, I want the Homebrew formula and Scoop manifest updated
    automatically with the new version and checksums, so that channels never
    lag a release.
17. As the maintainer, I want the sourcehut build manifests and the Pi upload
    gone, so that there is exactly one CI telling one truth.
18. As a contributor, I want the README to state how to install on each OS and
    which optional tools unlock which features (ffmpeg, poppler, LibreOffice),
    so that expectations are set before the first launch.
19. As a user of any channel, I want the installed binary to be identical in
    behavior to a locally built one — external tools optional, features
    degrading gracefully — so that no channel is a second-class citizen.
20. As the maintainer, I want CI caching for the Rust toolchain and build
    artifacts, so that the feedback loop stays fast enough to be respected.

## Implementation Decisions

- **GitHub is primary.** CI, releases, and issues live on GitHub. The
  sourcehut build manifests are deleted along with the Pi apt pool upload;
  the sourcehut remote itself is the user's business and is not touched.
- **Crate rename for crates.io.** `dedup-cli` is squatted by a foreign crate;
  `dedup-rs-core`, `dedup-rs-cli`, and `dedup-rs-gui` are free (verified
  2026-08-12) and become the package names. Directory names, the `dedup`
  binary name, and all internal `use` paths keep working via explicit `lib`
  names or updated imports — the user-visible install name is
  `cargo install dedup-rs-cli`, the binary remains `dedup`.
- **Two executables on Windows.** The console problem is unsolvable in one
  binary: a console-subsystem exe flashes a console behind the GUI on
  double-click, a windows-subsystem exe loses CLI output. The zip therefore
  carries `dedup.exe` (console subsystem, full CLI, can launch the GUI) and a
  new `dedup-gui.exe` (windows subsystem, GUI only, the double-click target).
  The GUI exe is a small new binary target on the GUI crate; the windows
  subsystem attribute applies only on Windows.
- **macOS ships an unsigned `.app` in a `.dmg`.** The bundle is assembled in
  the release workflow: Info.plist, the `dedup` binary, and an `.icns`
  generated on the macOS runner from the existing 256 px PNG icon. No Apple
  Developer signing or notarization at this stage; the README documents the
  right-click → Open first launch. Signing is revisited when installs beyond
  the maintainer's own machines matter.
- **Linux keeps the existing Debian packaging.** The `.deb` is built by the
  packaging script already in the repository, unchanged in role; a stripped
  release binary tarball is added beside it.
- **Tiered CI.** One workflow gates pushes and pull requests: an Ubuntu job
  runs `cargo fmt --check`, `cargo clippy -- -D warnings` on the workspace,
  and the full test suite with the software-Vulkan (lavapipe) stack plus
  ffmpeg and poppler installed so the gated tests actually run; Windows and
  macOS jobs build the workspace and run the core and CLI test suites only —
  GUI render tests stay a Linux concern. All jobs use Rust build caching.
- **Tag-driven release.** A second workflow triggers on `v*` tags: it re-runs
  the gate, builds all platform artifacts, creates the GitHub Release with
  the changelog section for that version as its notes, publishes the crates
  in dependency order (core, gui, cli) with the registry token secret, and
  pushes the regenerated Homebrew formula and Scoop manifest (version +
  sha256 of the release archives) to the user's `homebrew-tap` and
  `scoop-bucket` repositories using a channel PAT secret.
- **Channel repositories are self-owned.** Homebrew tap and Scoop bucket live
  in the user's own GitHub repositories; no curated-repo submissions (winget,
  Flathub, Homebrew core, Debian) in this effort.
- **One-time user-side setup** (cannot be done from this repository): create
  the two channel repositories, add the `CARGO_REGISTRY_TOKEN` and channel
  PAT secrets, push the branch, and push the first tag. The changelog's
  version heading receives its date only when the tag is actually cut.
- **Runtime dependencies stay optional everywhere.** No package hard-depends
  on ffmpeg, poppler, or LibreOffice; the app degrades gracefully and the
  Status centre explains what is missing. The Debian package may recommend
  them; brew/scoop mention them as caveats.

## Testing Decisions

A good test asserts external behavior at an existing seam. CI infrastructure
itself is only truly tested by running it, which happens on the user's first
push — everything that can be tested locally, is:

1. **The workspace suites are the seam for the rename.** After the package
   renames, `cargo test --workspace`, fmt, and clippy must pass unchanged —
   the rename is correct exactly when nothing else notices it.
2. **The new GUI binary target** must build on Linux too (the subsystem
   attribute is Windows-conditional), asserted by the ordinary workspace
   build; its behavior is the existing GUI, so no new tests.
3. **Packaging scripts** (Debian, bundle assembly, formula/manifest
   generation) are plain shell run by the workflows; where a script computes
   something testable (version extraction, checksum insertion), it is checked
   by running it locally against a scratch build. Prior art: the existing
   Debian packaging script, exercised today by the sourcehut manifest.
4. **Workflow files** are reviewed, not unit-tested; the acceptance test is
   the first green run on GitHub. Anything failing there is fixed in
   follow-up commits by the user's normal push loop.

## Out of Scope

- Code signing and notarization (Apple Developer ID, Windows EV certificate).
- Installers (MSI/WiX, Inno Setup, AppImage, Flatpak/Flathub, Snap).
- Curated package repositories: winget-pkgs, Homebrew core, Debian archive,
  AUR (may become a later wave).
- The Pi apt pool and any sourcehut CI replacement.
- CLI-only scanner/report GUI surfaces, or any feature work — this effort
  ships exactly what exists today, installably.
- Auto-update mechanisms inside the app.

## Further Notes

- `dedup-cli` on crates.io belongs to someone else and is active; the
  `dedup-rs-*` prefix was chosen to match the repository name rather than
  fighting for the shorter names.
- The Windows and macOS lanes deliberately skip the GUI render tests: GitHub
  runners' software GPU stacks (WARP, headless Metal) are close to but not
  identical with the lavapipe baseline, and chasing per-platform pixel
  differences buys nothing while the Linux lane already renders every layout.
- The unsigned macOS first-launch friction is accepted consciously: the
  current audience is the maintainer and early adopters; signing costs money
  and process and is listed as the natural next wave together with winget.

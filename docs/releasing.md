# Releasing

A release is one command:

```bash
git tag v0.2.1 && git push --tags
```

The tag must equal the crate version in the workspace (the release workflow refuses
otherwise), and the `CHANGELOG.md` section for that version becomes the release notes —
give its heading the release date in the same commit that gets tagged.

## What the tag triggers

The `Release` GitHub Actions workflow:

1. **Gate** — the full Linux check once more: `cargo fmt --check`, clippy with zero
   warnings, the complete test suite (software Vulkan, ffmpeg, poppler installed).
2. **Artifacts**
   - Linux (x86_64 and arm64): `dedup_<ver>_<arch>.deb` (via
     `packaging/mkdeb.sh --release`), `dedup-<ver>-linux-<arch>.tar.gz`, and
     `dedup-<ver>-linux-<arch>.AppImage` (via `packaging/mkappimage.sh --release`).
   - Windows: `dedup-<ver>-windows-x86_64.zip` holding `dedup.exe` (console CLI) and
     `dedup-gui.exe` (windows-subsystem GUI, the double-click target).
   - macOS (arm64 and x86_64): `dedup-<ver>-macos-<arch>.tar.gz` and an **unsigned**
     `dedup.app` inside `dedup-<ver>-macos-<arch>.dmg` (via `packaging/mkapp.sh`).
3. **GitHub Release** — created from the tag with all artifacts attached and the
   changelog section as its notes.
4. **crates.io** — `dedup-rs-core`, `dedup-rs-gui`, `dedup-rs-cli`, published in that
   (dependency) order. Install with `cargo install dedup-rs-cli`; the binary is `dedup`.
5. **Channels** — the Homebrew formula and Scoop manifest are rendered from the
   templates in `packaging/homebrew/` and `packaging/scoop/` (version + artifact
   sha256s filled in) and pushed to `paxel/homebrew-tap` and `paxel/scoop-bucket`.

Every push and pull request already runs the `CI` workflow: the full gate on Linux,
build + core/CLI tests on Windows and macOS.

## One-time setup

Repository **secrets** (Settings → Secrets and variables → Actions):

- `CARGO_REGISTRY_TOKEN` — a crates.io API token with publish scope.
- `CHANNEL_PAT` — a personal access token with push access to
  `paxel/homebrew-tap` and `paxel/scoop-bucket`.

The two channel repositories hold `Formula/dedup.rb` and `bucket/dedup.json`
respectively; the workflow creates or overwrites those files on each release.

## Signing (deliberately absent)

Neither the Windows executables nor the macOS bundle are code-signed. macOS users
right-click → **Open** on first launch; the README documents it. Signing and
notarization (and curated channels like winget or Homebrew core) are a later wave.

# Releasing

A release is one command:

```bash
git tag v0.4.0 && git push --tags
```

The tag must equal the crate version in the workspace and point at a commit on `summer`
(the release workflow refuses otherwise), and the `CHANGELOG.md` section for that version
becomes the release notes; the release fails without one.

## What the tag triggers

The `Release` GitHub Actions workflow:

1. **Gate** — the tag is the crate version and on `summer`, whose ruleset only takes
   pull requests with green, up-to-date CI. Then the paxel/gates release gate: no open
   code-scanning, secret-scanning or SonarQube Cloud finding, vulnerability or licence
   outside the allowlist that `code-scanning-ignores.toml` or `osv-scanner.toml` does not
   accept, and a `CHANGELOG.md` section for the version.
2. **Artifacts**
   - Linux (x86_64 and arm64): `dedup_<ver>_<arch>.deb` (via
     `packaging/mkdeb.sh --release`), `dedup-<ver>-linux-<arch>.tar.gz`, and
     `dedup-<ver>-linux-<arch>.AppImage` (via `packaging/mkappimage.sh --release`).
   - Windows: `dedup-<ver>-windows-x86_64.zip` holding `dedup.exe` (console CLI) and
     `dedup-gui.exe` (windows-subsystem GUI, the double-click target).
   - macOS (arm64 and x86_64): `dedup-<ver>-macos-<arch>.tar.gz` and an **unsigned**
     `dedup.app` inside `dedup-<ver>-macos-<arch>.dmg` (via `packaging/mkapp.sh`).
3. **Provenance** — the paxel/gates provenance workflow: a source SBOM
   (`dedup-v<ver>-source.cdx.json`), a build provenance attestation for every file and its
   Sigstore bundle (`dedup-v<ver>-provenance.sigstore.json`).
4. **GitHub Release** — created from the tag with all artifacts, the SBOM and the bundle
   attached and the changelog section as its notes.
5. **crates.io** — `dedup-rs-core`, `dedup-rs-gui`, `dedup-rs-cli`, published in that
   (dependency) order. Install with `cargo install dedup-rs-cli`; the binary is `dedup`.
6. **Channels** — the Homebrew formula and Scoop manifest are rendered from the
   templates in `packaging/homebrew/` and `packaging/scoop/` (version + artifact
   sha256s filled in) and pushed to `paxel/homebrew-tap` and `paxel/scoop-bucket`.

Every pull request and every push to `summer` already runs the `CI` workflow: the
paxel/gates Rust gate on Linux (format, lint set, clippy, all tests, the coverage ratchet
of `coverage-floors.toml`), the OSV gate and SonarQube Cloud, plus build, clippy and the
core/CLI tests on Windows and macOS.

## One-time setup

Repository **secrets** (Settings → Secrets and variables → Actions):

- `GATES_ALERTS_TOKEN` — a fine-grained token with Secret scanning alerts: read, for the
  release gate and the weekly Security run.
- `SQ_TOKEN` — the SonarQube Cloud token, for CI's analysis.
- `CARGO_REGISTRY_TOKEN` — a crates.io API token with publish scope.
- `CHANNEL_PAT` — a personal access token with push access to
  `paxel/homebrew-tap` and `paxel/scoop-bucket`.

The two channel repositories hold `Formula/dedup.rb` and `bucket/dedup.json`
respectively; the workflow creates or overwrites those files on each release.

## Signing (deliberately absent)

Neither the Windows executables nor the macOS bundle are code-signed. macOS users
right-click → **Open** on first launch; the README documents it. Signing and
notarization (and curated channels like winget or Homebrew core) are a later wave.

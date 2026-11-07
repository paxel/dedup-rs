# Security hardening: scanners, a release gate, signed provenance and parser limits

Decided on 2026-11-07, with the sibling projects sanshain-service and oldbrainfix
as the model for the CI side.

dedup-rs reads untrusted files in its own process: images, audio, EXIF, PDF,
zip/tar/gzip archives, office documents and e-mail. Only ffmpeg and poppler run
as separate programs. A malicious file on disk is therefore the main threat, more
than the CI pipeline.

**Scope.** All three layers: the scanners the siblings run, the release supply
chain, and the app itself (a threat model and the fixes it finds).

**Scanners.** SonarQube Cloud (clippy and coverage imported), CodeQL, and
`cargo-deny` for advisories, licenses, banned crates and crate sources. Every
GitHub Action is pinned by commit SHA, and every workflow runs with
least-privilege permissions. The OpenSSF Scorecard rates the result.

**Gate strictness.** Split: on everyday pushes the scanners only report. The
release workflow blocks on a known vulnerability, a failed SonarQube quality
gate or a `cargo-deny` violation. A finding that cannot be fixed yet gets an
ignore entry naming the advisory or rule and the reason.

**Provenance.** Every release artifact gets a GitHub build attestation, checked
with `gh attestation verify`. No cosign, no checksum file.

**Licenses.** Permissive licenses (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode)
plus MPL-2.0, whose copyleft stays file-level. The GPL family, LGPL included,
fails the release.

**Dependency updates.** Dependabot moves the pinned actions weekly. Cargo
dependencies get a Dependabot pull request only for a published advisory with a
fixed version; routine Cargo updates stay deliberate.

**Toolchain.** `rust-toolchain.toml` pins an exact Rust version, so a commit
always builds the same way and a new clippy lint arrives in a commit of its own.

**Parsers.** Hard limits plus fuzzing. Limits cap decoded image size, archive
entry count, total unpacked size, nesting depth and decode time. A file over a
cap is still hashed and indexed — content identity never loses a file — only
the deep part (archive members, fingerprint, thumbnail) is skipped, and the scan
report names the cap. The caps are fixed in code and generous: they stop bombs,
not big files. Fuzz targets for each parsing entry point run on a schedule; a
crash becomes a regression test and a fix. Moving parsing into a separate
process was rejected as too large a change for a single-user desktop tool.

## Consequences

- New workflows: security (`cargo-deny`), SonarQube Cloud, CodeQL, Scorecard
  and fuzzing; the release gate gains `cargo-deny`, the quality gate and the
  attestations.
- The threat model lives in `docs/security/threat-model.md` and drives the
  app-side fixes.
- Upgrading Rust is a commit that changes `rust-toolchain.toml` and the
  workflows' toolchain pin together.

## Addendum, 2026-11-07: the first Scorecard and CodeQL results

- The SonarQube Cloud scan runs in CI's coverage job on pushes to summer. A
  `workflow_run` workflow that checked out the tested commit was flagged by
  Scorecard as a dangerous pattern, even though it only ran for this
  repository's own pushes.
- CodeQL skips `crates/*/tests/**`: their archive passwords are fixtures. False
  positives in shipped code are dismissed with a reason, so an open CodeQL alert
  is a real finding and also fails a release.
- EXR decoding is dropped: its crate pulls in the unmaintained `paste`. EXR
  files are still hashed and found as exact duplicates. image's `rayon` feature
  is off too, because its weak dependency locked the unused AVIF encoder.
- eframe 0.37 without winit's default features: the Adwaita window frame and its
  unmaintained ttf-parser are gone. The three accesskit advisories stay ignored;
  egui_kittest needs accesskit.
- summer is protected by a ruleset: changes arrive through a pull request with
  green CI, without force-push or deletion. No approval is required, as there is
  a single maintainer.
- No OpenSSF Best Practices badge: it is a self-assessment, not a protection.

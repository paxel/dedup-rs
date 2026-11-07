# Security policy

## Supported versions

Only the latest release gets security fixes.

## Reporting a vulnerability

Report it privately through GitHub:
<https://github.com/paxel/dedup-rs/security/advisories/new> (the repository's
**Security** tab, **Report a vulnerability**). Please do not open a public issue for it.
Expect a first answer within a week; a fix ships in the next release, and the advisory
is published with it.

A report is most useful with the dedup version, the operating system, and the file or
steps that trigger the problem. dedup reads untrusted files (images, audio, video,
PDF, office documents, e-mail and archives), so a crafted file that crashes it, hangs
it, exhausts memory or makes it touch anything outside the repository it scans is in
scope.

## How releases are protected

Dependencies are checked against the RustSec advisory database, licenses and crate
sources by `cargo-deny`; the code is analysed by CodeQL and SonarQube Cloud. A release
fails on a `cargo-deny` finding, a SonarQube Cloud quality gate that is not OK, or an
open CodeQL alert. Every release file carries a build provenance attestation:

```bash
gh attestation verify <file> -R paxel/dedup-rs
```

The decisions behind this are in
[docs/adr/0005-security-hardening.md](docs/adr/0005-security-hardening.md).

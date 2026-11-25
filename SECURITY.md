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

dedup-rs follows the [paxel/gates](https://github.com/paxel/gates) standard. Dependencies
are checked for known vulnerabilities and licences by osv-scanner; the code is analysed by
CodeQL and SonarQube Cloud; the OpenSSF Scorecard rates the repository. A release fails on
any open code-scanning or secret-scanning alert, SonarQube Cloud finding, vulnerability or
licence outside the allowlist that `code-scanning-ignores.toml` or `osv-scanner.toml` does
not accept with a reason and a date; the weekly Security run checks the same. Every release
file carries a build provenance attestation, and the release a source SBOM and the
attestation's Sigstore bundle:

```bash
gh attestation verify <file> -R paxel/dedup-rs \
  --signer-workflow paxel/gates/.github/workflows/provenance.yml --source-ref refs/tags/<tag>
```

The decisions behind this are in
[docs/adr/0006-move-to-gates.md](docs/adr/0006-move-to-gates.md); the threat model and the
parser limits in
[docs/adr/0005-security-hardening.md](docs/adr/0005-security-hardening.md).

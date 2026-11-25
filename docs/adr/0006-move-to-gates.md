# Move to the paxel/gates standard

Decided on 2026-11-25 ([dedup-28](https://github.com/paxel/dedup-rs/issues/28)).

Every other active paxel repository runs the shared workflows of
[paxel/gates](https://github.com/paxel/gates) behind a protected `summer`, and its weekly
audit reported dedup-rs drifting from that standard (#25). dedup-rs moves to it in full:
the scanner, gate and provenance sections of [ADR 0005](0005-security-hardening.md) are
superseded by this record. ADR 0005's threat model, parser limits and licence policy stay;
the licences it allows are within the standard's allowlist.

**Workflows.** CI calls the shared `vulns.yml`, `rust.yml` and `sonar.yml`; Security calls
`release-gate.yml` weekly; CodeQL, Fuzz and Scorecard are the standard's callers; the
release calls `release-gate.yml` before it builds and `provenance.yml` after. Each shared
workflow is pinned to a gates release by SHA, and Dependabot moves it.

**CI triggers.** Pull requests and pushes to `summer` only. `summer` takes nothing but pull
requests, so a branch without one has nothing to merge, and SonarQube Cloud analyses only
the default branch and pull requests.

**Windows and macOS stay dedup-rs's own lane.** The shared Rust gate tests on Linux only;
its other runners only run clippy. dedup-rs ships on Windows and macOS (Scoop, Homebrew,
the zip and the dmg), so its `native` job keeps building them, running clippy and the core
and CLI suites, which catch path and file-system breakage clippy cannot. GUI render tests
stay a Linux concern.

**Coverage per crate.** The ratchet measures three parts, `core`, `cli` and `gui`, each
leaving the other crates' files out, with a floor and a badge each: a well-covered core
cannot hide a drop in the GUI.

**cargo-deny is gone.** osv-scanner checks vulnerabilities and licences, as on every other
stack. Its accepted advisories moved to `osv-scanner.toml` with a reason and a date. The
bans and sources checks go without a replacement: `--locked` builds and a crates.io-only
lockfile leave little for them to find.

**Release.** A tag push only; the manual trigger is gone. Before the build, the tag must
equal the crate version and the tagged commit must be on `summer`: the ruleset takes only
pull requests whose checks are green and up to date, so the commit already passed the full
CI and the release does not run it again. Then the release gate. After the build,
`provenance.yml` adds a source SBOM and the attestation as a Sigstore bundle to every
release.

**Fuzzing.** The shared `fuzz.yml` runs `fingerprint` and `archive` for 900 seconds each,
starting from the seeds in `fuzz/seeds/`. It takes no libFuzzer flags, so `-max_len` and
`-timeout` are dropped: libFuzzer takes the longest input from the biggest seed, and a hang
is found at libFuzzer's default timeout.

**Ruleset.** `summer` requires the shared gates' checks and the native lanes, strict. The
switch to the new check names happens once a pull request has reported them, so no pull
request waits for a check that never comes.

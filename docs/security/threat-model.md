# Threat model

What an attacker can make dedup do, and what stops it. Decided scope and rules are in
[ADR 0005](../adr/0005-security-hardening.md).

## Assets and attacker

- **Assets:** the user's files (never lost or overwritten), the index (correct content
  identity), the machine (no code execution, no data sent anywhere), and the app's
  availability (a scan finishes).
- **Attacker:** anyone who can put a file into a folder the user indexes — a download, an
  e-mail attachment, an archive from someone else, a shared drive. The attacker does not
  control the user's clicks, the index database or the installed tools.
- **Out of scope:** an attacker who already runs code as the user; the external tools'
  own vulnerabilities (ffmpeg, poppler, LibreOffice), beyond what dedup feeds them; files
  the user deliberately opens in another application.

## Where untrusted bytes are parsed

| Input                       | Parser (in process unless noted)         | Reached by                      |
|-----------------------------|------------------------------------------|---------------------------------|
| Every file                  | BLAKE3 hashing, `infer` MIME sniffing    | scan                            |
| Images                      | `image`, `kamadak-exif`                  | scan (fingerprint), viewer      |
| Audio                       | `symphonia`, ffmpeg fallback (process)   | scan (fingerprint), viewer      |
| Video                       | ffprobe and ffmpeg (process)             | scan (fingerprint), viewer      |
| PDF                         | `lopdf`; `pdftoppm`, `pdfinfo` (process) | scan (text), viewer             |
| Office (OOXML, ODF)         | `zip`, `quick-xml`                       | scan (text), viewer             |
| Spreadsheets (xls)          | `calamine`                               | scan (text), viewer             |
| Legacy office, RTF          | LibreOffice `soffice` (process)          | viewer                          |
| E-mail (`.eml`)             | `mail-parser`                            | scan (identity), viewer         |
| Archives (zip, tar, tar.gz) | `zip`, `tar`, `flate2`                   | scan (members), browse, extract |
| Small non-media files       | secret probes (byte patterns)            | scan                            |

## Threats and mitigations

1. **Archive bomb during a scan.** Every readable member of a changed archive is
   decompressed and hashed. A few kilobytes can declare terabytes of zeros or millions of
   members, and the scan would never finish. *Mitigation:* member indexing stops at a
   member cap and an unpacked-bytes cap scaled to the archive's own size; the archive is
   still hashed and indexed, the scan report names the cap.
2. **Decompression bomb in a document.** An office document's XML part, an `.eml` or a PDF
   is read into memory whole. *Mitigation:* each is read up to a fixed cap; past it the
   document gets no text fingerprint and no text view.
3. **A tool that never returns.** ffprobe, ffmpeg and pdftoppm run without a deadline; a
   crafted file can make one loop or wait forever, stalling the scan. *Mitigation:* every
   external call runs under a deadline and is killed when it passes (LibreOffice already
   had one).
4. **A tool told to do something else.** ffmpeg reads playlist formats (HLS, concat) that
   can name network URLs or other local files, so a renamed `.m3u8` could make it fetch
   from the network. A file name starting with `-` would read as an option to a tool.
   *Mitigation:* ffmpeg and ffprobe accept only the `file` and `pipe` protocols, and every
   path handed to a tool is absolute.
5. **Oversized image.** A small file can declare a huge canvas. *Mitigation:* the `image`
   crate's default decoder limit (512 MiB per decode) already refuses it; the file keeps
   its hash and gets no fingerprint or thumbnail.
6. **Parser crash or hang on malformed input.** A panic deep in a dependency, or an
   endless loop, on input nobody tested. *Mitigation:* fuzz targets per parsing entry
   point (`fuzz/`) run weekly; a finding becomes a fixture under
   `crates/dedup-core/tests/fixtures/fuzz/`, a regression test and a fix. Fingerprinting,
   document text, the waveform worker and the player's probe contain a parser panic: the
   file gets no fingerprint or preview, the scan and the app go on. Dependencies with
   published advisories fail the release (`cargo-deny`).
7. **Path traversal on extract.** A member named `../../.bashrc` or `/etc/x`.
   *Mitigation (existing):* extraction keeps only normal path components, never
   overwrites (collisions get a suffix), writes symlink members as plain files and only
   runs when the user asks for it.
8. **Secrets on disk.** Archive passwords the user saves. *Mitigation (existing):* stored
   encrypted ([ADR 0002](../adr/0002-archive-password-persistence.md)).

## Residual risk

- `lopdf` and `symphonia` decompress streams without a size limit of their own; the caps
  above bound the input size, not the decoded size. Fuzzing is the check here.
- symphonia 0.5 panics on two malformed inputs the fuzzer found (an MKV variable-length
  integer, an AAC ADTS header). Contained, not fixed: once playback has started, rodio
  decodes on its own audio thread, where a panic stops the sound.
- ffmpeg, poppler and LibreOffice parse in their own processes; their vulnerabilities are
  theirs, but a crash there costs one fingerprint or preview, not the app.

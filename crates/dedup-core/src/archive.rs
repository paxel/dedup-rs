//! Archive member indexing, browsing, extraction and coverage. An inherited
//! `backup_2019.zip` that contains nothing you don't already have loose is the
//! single biggest redundancy class on old disks — but its bytes differ from the
//! loose files, so content dedup never sees it. This indexes each archive's
//! members by content identity (size + BLAKE3) into a side table (during the
//! normal scan), reports what fraction of an archive is already present
//! elsewhere ([`repo_archive_coverage`]), lists members for browsing
//! ([`list_entries`]), and extracts a member's contents ([`extract_member`]).
//! The source archive is never modified.
//!
//! Handled one level deep (zip, tar, tar.gz/tgz); nested archives are opaque
//! members. An encrypted zip is indexed shallowly — member names and sizes are
//! known, but a member's content stays **locked** (no hash) until unlocked.

use crate::store::{self, ArchiveMember, ContentKey, ContentState, Store, StoreError};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

enum Kind {
    Zip,
    Tar,
    TarGz,
}

fn detect_kind(rel_path: &str, mime: Option<&str>) -> Option<Kind> {
    let lower = rel_path.to_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return Some(Kind::TarGz);
    }
    if lower.ends_with(".tar") || mime == Some("application/x-tar") {
        return Some(Kind::Tar);
    }
    if lower.ends_with(".zip") || mime == Some("application/zip") {
        return Some(Kind::Zip);
    }
    None
}

/// Whether a file looks like a supported archive (by extension or MIME).
pub fn is_archive(rel_path: &str, mime: Option<&str>) -> bool {
    detect_kind(rel_path, mime).is_some()
}

/// Read `r` fully, returning `(size, blake3)` — the content identity used to
/// match a member against loose repo content.
fn hash_reader<R: Read>(mut r: R) -> Option<(u64, [u8; 32])> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = r.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Some((size, *hasher.finalize().as_bytes()))
}

/// One member as seen when *browsing* an archive: name, size, and whether it is
/// locked (encrypted). Cheap to produce — read from the archive's directory
/// without hashing or decompressing contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
    pub locked: bool,
}

/// List an archive's file members for browsing — names, sizes and lock state
/// only, no content hashing. `None` when the archive can't be opened at all.
pub fn list_entries(path: &Path, rel_path: &str, mime: Option<&str>) -> Option<Vec<ArchiveEntry>> {
    match detect_kind(rel_path, mime)? {
        Kind::Zip => zip_entries(path),
        Kind::Tar => tar_entries(std::fs::File::open(path).ok()?),
        Kind::TarGz => {
            let file = std::fs::File::open(path).ok()?;
            tar_entries(flate2::read::GzDecoder::new(file))
        }
    }
}

fn zip_entries(path: &Path) -> Option<Vec<ArchiveEntry>> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let mut out = Vec::new();
    for i in 0..archive.len() {
        if let Ok(e) = archive.by_index_raw(i)
            && e.is_file()
        {
            out.push(ArchiveEntry {
                name: e.name().to_string(),
                size: e.size(),
                locked: e.encrypted(),
            });
        }
    }
    Some(out)
}

fn tar_entries<R: Read>(reader: R) -> Option<Vec<ArchiveEntry>> {
    let mut archive = tar::Archive::new(reader);
    let mut out = Vec::new();
    for entry in archive.entries().ok()? {
        let Ok(entry) = entry else { continue };
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let name = entry
            .path()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        out.push(ArchiveEntry {
            name,
            size: entry.header().size().unwrap_or(0),
            locked: false, // tar has no per-member encryption
        });
    }
    Some(out)
}

/// Extract one member of an archive to `dest` (contents only; the source
/// archive is never modified). `password` decrypts an encrypted zip member.
/// Errors if the member is not found or cannot be read/decrypted.
pub fn extract_member(
    archive_path: &Path,
    rel_path: &str,
    mime: Option<&str>,
    member_name: &str,
    dest: &Path,
    password: Option<&str>,
) -> std::io::Result<()> {
    let kind = detect_kind(rel_path, mime).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "unsupported archive")
    })?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(dest)?;
    match kind {
        Kind::Zip => {
            let file = std::fs::File::open(archive_path)?;
            let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
                .map_err(std::io::Error::other)?;
            let mut member = match password {
                Some(pw) => archive
                    .by_name_decrypt(member_name, pw.as_bytes())
                    .map_err(std::io::Error::other)?,
                None => archive
                    .by_name(member_name)
                    .map_err(std::io::Error::other)?,
            };
            std::io::copy(&mut member, &mut out)?;
        }
        Kind::Tar | Kind::TarGz => {
            let file = std::fs::File::open(archive_path)?;
            let found = if matches!(kind, Kind::TarGz) {
                extract_tar_member(flate2::read::GzDecoder::new(file), member_name, &mut out)?
            } else {
                extract_tar_member(file, member_name, &mut out)?
            };
            if !found {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "member not found",
                ));
            }
        }
    }
    Ok(())
}

/// A `dest` path that does not already exist: if it does, insert `_1`, `_2`, …
/// before the extension. The forensic rule — an extract never overwrites an
/// existing file.
fn non_colliding(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let ext = dest.extension().map(|e| e.to_string_lossy().into_owned());
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    let mut i = 1;
    loop {
        let name = match &ext {
            Some(e) => format!("{stem}_{i}.{e}"),
            None => format!("{stem}_{i}"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
        i += 1;
    }
}

/// Extract every **readable** member of an archive into `dest_dir`, preserving
/// each member's path within the archive, never overwriting an existing file
/// (collisions get a `_N` suffix). Locked members are skipped. Returns how many
/// members were written. The source archive is never modified.
pub fn extract_all(
    archive_path: &Path,
    rel_path: &str,
    mime: Option<&str>,
    dest_dir: &Path,
    password: Option<&str>,
) -> std::io::Result<usize> {
    let entries = list_entries(archive_path, rel_path, mime).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "cannot open archive")
    })?;
    let mut written = 0;
    for entry in entries {
        if entry.locked && password.is_none() {
            continue;
        }
        // Guard against path traversal: keep only normal components.
        let rel = sanitize_member_path(&entry.name);
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = non_colliding(&dest_dir.join(&rel));
        if extract_member(archive_path, rel_path, mime, &entry.name, &dest, password).is_ok() {
            written += 1;
        }
    }
    Ok(written)
}

/// Keep only normal path components of a member name, dropping any `..` or
/// absolute-root parts — an archive must never write outside `dest_dir`.
fn sanitize_member_path(name: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in Path::new(name).components() {
        if let std::path::Component::Normal(c) = comp {
            out.push(c);
        }
    }
    out
}

fn extract_tar_member<R: Read>(
    reader: R,
    member_name: &str,
    out: &mut std::fs::File,
) -> std::io::Result<bool> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name == member_name {
            std::io::copy(&mut entry, out)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `password` unlocks this archive — verified by fully reading its
/// first encrypted member (so AES authentication actually checks the password;
/// for legacy ZipCrypto the check is the format's inherent 1/256-weak one).
/// Only zip archives carry per-member encryption here.
pub fn verify_password(
    archive_path: &Path,
    rel_path: &str,
    mime: Option<&str>,
    password: &str,
) -> bool {
    if !matches!(detect_kind(rel_path, mime), Some(Kind::Zip)) {
        return false;
    }
    let Ok(file) = std::fs::File::open(archive_path) else {
        return false;
    };
    let Ok(mut archive) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        return false;
    };
    for i in 0..archive.len() {
        let is_enc = archive
            .by_index_raw(i)
            .map(|e| e.is_file() && e.encrypted())
            .unwrap_or(false);
        if is_enc {
            return match archive.by_index_decrypt(i, password.as_bytes()) {
                Ok(mut e) => std::io::copy(&mut e, &mut std::io::sink()).is_ok(),
                Err(_) => false,
            };
        }
    }
    false
}

/// Re-read an archive's members with `password`, hashing the (now decryptable)
/// contents so previously-locked members gain their real content identity.
fn zip_members_pw(path: &Path, password: &str) -> Option<Vec<ArchiveMember>> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let mut members = Vec::new();
    for i in 0..archive.len() {
        let (name, size, encrypted, is_file) = match archive.by_index_raw(i) {
            Ok(e) => (e.name().to_string(), e.size(), e.encrypted(), e.is_file()),
            Err(_) => continue,
        };
        if !is_file {
            continue;
        }
        let opened = if encrypted {
            archive.by_index_decrypt(i, password.as_bytes())
        } else {
            archive.by_index(i)
        };
        match opened {
            Ok(mut entry) => {
                if let Some((size, hash)) = hash_reader(&mut entry) {
                    members.push(ArchiveMember {
                        rel_path: name,
                        size,
                        hash: Some(hash),
                        locked: false,
                    });
                }
            }
            // Still couldn't read (wrong password for this member): keep it
            // recorded as locked rather than dropping it.
            Err(_) => members.push(ArchiveMember {
                rel_path: name,
                size,
                hash: None,
                locked: encrypted,
            }),
        }
    }
    Some(members)
}

/// Export the first encrypted member's WinZip-AES parameters in hashcat's
/// `$zip2$` format (mode 13600), so serious recovery can be handed off to
/// hashcat/John. `None` for a non-zip, an unencrypted zip, or legacy ZipCrypto
/// (which uses the different `$pkzip2$` format — not produced here). The source
/// archive is only read, never modified.
pub fn export_hashcat_hash(
    archive_path: &Path,
    rel_path: &str,
    mime: Option<&str>,
) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    if !matches!(detect_kind(rel_path, mime), Some(Kind::Zip)) {
        return None;
    }
    // Locate the first encrypted member and its on-disk offsets.
    let (header_start, data_start, comp_size) = {
        let file = std::fs::File::open(archive_path).ok()?;
        let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
        let mut found = None;
        for i in 0..archive.len() {
            if let Ok(e) = archive.by_index_raw(i)
                && e.is_file()
                && e.encrypted()
            {
                found = Some((e.header_start(), e.data_start()?, e.compressed_size()));
                break;
            }
        }
        found?
    };

    let mut file = std::fs::File::open(archive_path).ok()?;
    // Parse the local file header's extra fields for the 0x9901 AES record,
    // which carries the encryption strength (→ salt length, hashcat mode).
    file.seek(SeekFrom::Start(header_start)).ok()?;
    let mut lfh = [0u8; 30];
    file.read_exact(&mut lfh).ok()?;
    if &lfh[0..4] != b"PK\x03\x04" {
        return None;
    }
    let name_len = u16::from_le_bytes([lfh[26], lfh[27]]) as u64;
    let extra_len = u16::from_le_bytes([lfh[28], lfh[29]]) as usize;
    file.seek(SeekFrom::Start(header_start + 30 + name_len))
        .ok()?;
    let mut extra = vec![0u8; extra_len];
    file.read_exact(&mut extra).ok()?;
    let mut strength = None;
    let mut p = 0;
    while p + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[p], extra[p + 1]]);
        let sz = u16::from_le_bytes([extra[p + 2], extra[p + 3]]) as usize;
        // AES extra: version(2) vendor(2="AE") strength(1) compression(2).
        if id == 0x9901 && sz >= 7 && p + 4 + 5 <= extra.len() {
            strength = Some(extra[p + 4 + 4]);
            break;
        }
        p += 4 + sz;
    }
    let (mode, salt_len) = match strength? {
        1 => (1u8, 8usize),
        2 => (2, 12),
        3 => (3, 16),
        _ => return None,
    };

    // At data_start: [salt][2-byte verifier][ciphertext][10-byte auth].
    file.seek(SeekFrom::Start(data_start)).ok()?;
    let mut blob = vec![0u8; comp_size as usize];
    file.read_exact(&mut blob).ok()?;
    if blob.len() < salt_len + 2 + 10 {
        return None;
    }
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let salt = &blob[..salt_len];
    let verifier = &blob[salt_len..salt_len + 2];
    let data = &blob[salt_len + 2..blob.len() - 10];
    let auth = &blob[blob.len() - 10..];
    Some(format!(
        "$zip2$*0*{mode}*0*{}*{}*{}*{}*{}*$/zip2$",
        hex(salt),
        hex(verifier),
        data.len(),
        hex(data),
        hex(auth),
    ))
}

/// Whether hashcat is available on the PATH (the "launch hashcat" affordance
/// only appears when it is; otherwise the user exports the hash and runs it
/// elsewhere, exactly as the tool degrades without ffmpeg).
pub fn hashcat_available() -> bool {
    std::process::Command::new("hashcat")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Simple candidate variants of a base word for a weak-password attempt: the
/// word, its capitalisation, and common digit/suffix mutations. Bounded and
/// small — this is for weak, human-chosen passwords, not exhaustive search.
fn candidates_for(word: &str) -> Vec<String> {
    let mut v = vec![word.to_string()];
    let mut chars = word.chars();
    if let Some(first) = chars.next() {
        v.push(first.to_uppercase().collect::<String>() + chars.as_str());
    }
    let bases = [word.to_string(), v.get(1).cloned().unwrap_or_default()];
    for base in bases.iter().filter(|b| !b.is_empty()) {
        for suffix in [
            "1", "12", "123", "1234", "!", "?", "0", "2", "01", "69", "00", "007", "2020",
        ] {
            v.push(format!("{base}{suffix}"));
        }
    }
    v.dedup();
    v
}

/// Try to recover a weak archive password: first the `extra` candidates (the
/// session spray-book / already-known passwords), then each `wordlist` word and
/// its simple mutations. Returns the password on success. Honest ceiling — a
/// strong password will not fall to this; that is hashcat's job.
pub fn recover_password<F: Fn() -> bool>(
    archive_path: &Path,
    rel_path: &str,
    mime: Option<&str>,
    extra: &[String],
    wordlist: &[String],
    is_cancelled: F,
) -> Option<String> {
    let try_one = |c: &str| verify_password(archive_path, rel_path, mime, c);
    for c in extra {
        if is_cancelled() {
            return None;
        }
        if try_one(c) {
            return Some(c.clone());
        }
    }
    for w in wordlist {
        if is_cancelled() {
            return None;
        }
        for c in candidates_for(w) {
            if try_one(&c) {
                return Some(c);
            }
        }
    }
    None
}

/// A small built-in wordlist of very common passwords, for the zero-config
/// "try the easy possibilities" recovery. Real jobs supply their own wordlist.
pub fn builtin_wordlist() -> Vec<String> {
    [
        "password", "passwort", "123456", "qwerty", "letmein", "admin", "welcome", "monkey",
        "dragon", "master", "shadow", "superman", "family", "photos", "backup", "private",
        "secret", "hunter", "iloveyou", "abc", "test", "guest", "root", "changeme",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Verify `password` and, on success, re-index the archive's members with their
/// now-decryptable contents (replacing the LOCKED placeholders). Returns whether
/// the archive was unlocked. The source archive is never modified.
pub fn unlock_and_reindex(
    store: &Store,
    repo: &str,
    rel: &str,
    mime: Option<&str>,
    password: &str,
) -> Result<bool, StoreError> {
    let meta = store.get_repo(repo)?;
    let abs = Path::new(&meta.abs_path).join(rel);
    if !verify_password(&abs, rel, mime, password) {
        return Ok(false);
    }
    if let Some(members) = zip_members_pw(&abs, password) {
        let db = store.open_repo_db(repo)?;
        store::set_archive_members(&db, rel, &members)?;
    }
    Ok(true)
}

/// List an archive's file members with their content identity, or `None` when
/// the archive can't be read (encrypted, corrupt, unsupported).
pub fn list_members(path: &Path, rel_path: &str, mime: Option<&str>) -> Option<Vec<ArchiveMember>> {
    match detect_kind(rel_path, mime)? {
        Kind::Zip => zip_members(path),
        Kind::Tar => tar_members(std::fs::File::open(path).ok()?),
        Kind::TarGz => {
            let file = std::fs::File::open(path).ok()?;
            tar_members(flate2::read::GzDecoder::new(file))
        }
    }
}

fn zip_members(path: &Path) -> Option<Vec<ArchiveMember>> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let mut members = Vec::new();
    for i in 0..archive.len() {
        // Read the member's metadata without decrypting or decompressing: a
        // zip's directory carries names and sizes unencrypted even when the
        // contents are encrypted, so a locked member is still listed.
        let (name, size, encrypted, is_file) = match archive.by_index_raw(i) {
            Ok(e) => (e.name().to_string(), e.size(), e.encrypted(), e.is_file()),
            Err(_) => continue,
        };
        if !is_file {
            continue;
        }
        if encrypted {
            // Shallow: name + size known, contents locked until unlocked.
            members.push(ArchiveMember {
                rel_path: name,
                size,
                hash: None,
                locked: true,
            });
            continue;
        }
        // Readable member: hash its contents for the coverage match.
        match archive.by_index(i) {
            Ok(mut entry) => {
                if let Some((size, hash)) = hash_reader(&mut entry) {
                    members.push(ArchiveMember {
                        rel_path: name,
                        size,
                        hash: Some(hash),
                        locked: false,
                    });
                }
            }
            // Readable per the directory but unreadable in practice (corrupt):
            // list it shallowly rather than dropping it.
            Err(_) => members.push(ArchiveMember {
                rel_path: name,
                size,
                hash: None,
                locked: false,
            }),
        }
    }
    Some(members)
}

fn tar_members<R: Read>(reader: R) -> Option<Vec<ArchiveMember>> {
    let mut archive = tar::Archive::new(reader);
    let mut members = Vec::new();
    for entry in archive.entries().ok()? {
        let mut entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let name = entry
            .path()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if let Some((size, hash)) = hash_reader(&mut entry) {
            members.push(ArchiveMember {
                rel_path: name,
                size,
                hash: Some(hash),
                locked: false,
            });
        }
    }
    Some(members)
}

/// Where a piece of content was found inside an archive: which repo's archive,
/// and the member's path within it. Powers the Duplicates "evidence rows" and
/// the tiered delete-safety check (is a file's only other copy inside a zip?).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveOccurrence {
    pub repo: String,
    pub archive_rel: String,
    pub member_name: String,
}

/// Index every **readable** archive member across `repos` by its content
/// identity, so a loose file's content can be looked up to see which archives
/// also contain it. Locked members (unknown content) are excluded.
pub fn members_by_content(
    store: &Store,
    repos: &[&str],
) -> Result<HashMap<ContentKey, Vec<ArchiveOccurrence>>, StoreError> {
    let mut map: HashMap<ContentKey, Vec<ArchiveOccurrence>> = HashMap::new();
    for &repo in repos {
        let db = store.open_repo_db(repo)?;
        store::for_each_archive_members(&db, |archive_rel, members| {
            for m in members {
                if let Some(hash) = m.hash {
                    map.entry((m.size, hash))
                        .or_default()
                        .push(ArchiveOccurrence {
                            repo: repo.to_string(),
                            archive_rel: archive_rel.to_string(),
                            member_name: m.rel_path,
                        });
                }
            }
            Ok(())
        })?;
    }
    Ok(map)
}

/// Coverage of one archive: how many of its members' contents already exist in
/// the reference repos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub rel_path: String,
    pub members: usize,
    pub present: usize,
    /// How many members are locked (encrypted, contents unknown). A locked
    /// member can never be `present`, so an archive with any locked member is
    /// never redundant until it is unlocked.
    pub locked: usize,
    /// Every member is present elsewhere (and there is at least one member):
    /// the archive is safe to delete.
    pub redundant: bool,
}

impl Coverage {
    /// Whether the archive has locked (encrypted) members whose contents are
    /// not yet known.
    pub fn has_locked(&self) -> bool {
        self.locked > 0
    }
}

impl Coverage {
    /// Percent of members already present elsewhere (0 for an empty archive).
    pub fn percent(&self) -> f64 {
        if self.members == 0 {
            0.0
        } else {
            self.present as f64 / self.members as f64 * 100.0
        }
    }
}

/// Report coverage for every indexed archive in `archive_repo` against the
/// union of `references`' loose content. Archive members are indexed as part of
/// the normal scan, so this reports whatever the repo's last scan populated.
pub fn repo_archive_coverage(
    store: &Store,
    archive_repo: &str,
    references: &[&str],
) -> Result<Vec<Coverage>, StoreError> {
    let mut merged: HashMap<ContentKey, ContentState> = HashMap::new();
    for name in references {
        let db = store.open_repo_db(name)?;
        for (key, state) in store::read_content_index(&db)? {
            let slot = merged.entry(key).or_default();
            slot.present |= state.present;
            slot.missing |= state.missing;
        }
    }

    let db = store.open_repo_db(archive_repo)?;
    let mut out = Vec::new();
    store::for_each_archive_members(&db, |rel_path, members| {
        let total = members.len();
        let present = members
            .iter()
            .filter(|m| {
                // A locked member has no hash and can never match loose content.
                m.hash
                    .and_then(|h| merged.get(&(m.size, h)))
                    .is_some_and(|state| state.present)
            })
            .count();
        let locked = members.iter().filter(|m| m.locked).count();
        out.push(Coverage {
            rel_path: rel_path.to_string(),
            members: total,
            present,
            locked,
            redundant: total > 0 && present == total,
        });
        Ok(())
    })?;
    // Most-covered first, so fully-redundant archives surface at the top.
    out.sort_by(|a, b| {
        b.percent()
            .partial_cmp(&a.percent())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    Ok(out)
}

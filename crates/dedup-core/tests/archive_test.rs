//! Archive member indexing + coverage. Member indexing folds into the normal
//! scan (a changed archive is read once, an unchanged one skipped); a zip whose
//! members all exist loose is reported 100% redundant.

use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::io::Write;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

/// A zip with one plaintext member and one AES-256-encrypted member.
fn write_mixed_encrypted_zip(
    path: &std::path::Path,
    plain: (&str, &[u8]),
    encrypted: (&str, &[u8]),
    password: &str,
) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let plain_opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file(plain.0, plain_opts).unwrap();
    zip.write_all(plain.1).unwrap();
    let enc_opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .with_aes_encryption(zip::AesMode::Aes256, password);
    zip.start_file(encrypted.0, enc_opts).unwrap();
    zip.write_all(encrypted.1).unwrap();
    zip.finish().unwrap();
}

/// The scan itself indexes an archive's members — no separate `index` pass —
/// and coverage reports redundancy against loose content.
#[test]
fn scanning_indexes_archive_members_and_reports_coverage() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;

    // "loose" repo holds two files; "arch" repo holds a zip of the same two
    // plus one extra file not present loose.
    let loose_dir = tmp.path().join("loose");
    std::fs::create_dir_all(&loose_dir)?;
    std::fs::write(loose_dir.join("a.txt"), b"alpha")?;
    std::fs::write(loose_dir.join("b.txt"), b"beta")?;
    store.create_repo("loose", &loose_dir.to_string_lossy())?;
    update_repo(&store, "loose", 1, &NoProgress, &CancellationToken::new())?;

    let arch_dir = tmp.path().join("arch");
    std::fs::create_dir_all(&arch_dir)?;
    write_zip(
        &arch_dir.join("all.zip"),
        &[("a.txt", b"alpha"), ("b.txt", b"beta")],
    );
    write_zip(
        &arch_dir.join("partial.zip"),
        &[("a.txt", b"alpha"), ("new.txt", b"unique content")],
    );
    store.create_repo("arch", &arch_dir.to_string_lossy())?;
    // A plain scan — no explicit index_repo_archives call — populates members.
    update_repo(&store, "arch", 1, &NoProgress, &CancellationToken::new())?;

    let report = dedup_core::archive::repo_archive_coverage(&store, "arch", &["loose"])?;
    let all = report.iter().find(|c| c.rel_path == "all.zip").unwrap();
    assert_eq!((all.members, all.present), (2, 2));
    assert!(all.redundant, "all.zip is fully redundant");

    let partial = report.iter().find(|c| c.rel_path == "partial.zip").unwrap();
    assert_eq!((partial.members, partial.present), (2, 1));
    assert!(!partial.redundant, "partial.zip keeps unique content");
    Ok(())
}

/// An encrypted member is indexed shallowly — its name and size are recorded,
/// marked LOCKED with no content hash — instead of being silently dropped. A
/// locked member never counts toward coverage, so the archive is not redundant
/// even when its readable member matches loose content.
#[test]
fn encrypted_member_indexes_shallowly_as_locked() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;

    // A loose copy of the plaintext member, so it *would* be redundant if the
    // whole archive were readable.
    let loose = tmp.path().join("loose");
    std::fs::create_dir_all(&loose)?;
    std::fs::write(loose.join("readme.txt"), b"plain contents")?;
    store.create_repo("loose", &loose.to_string_lossy())?;
    update_repo(&store, "loose", 1, &NoProgress, &CancellationToken::new())?;

    let arch = tmp.path().join("arch");
    std::fs::create_dir_all(&arch)?;
    write_mixed_encrypted_zip(
        &arch.join("locked.zip"),
        ("readme.txt", b"plain contents"),
        ("secret.txt", b"top secret contents"),
        "hunter2",
    );
    store.create_repo("arch", &arch.to_string_lossy())?;
    update_repo(&store, "arch", 1, &NoProgress, &CancellationToken::new())?;

    // Both members are indexed; one is locked with a known name and size.
    let db = store.open_repo_db("arch")?;
    let mut found: Vec<(String, u64, bool, bool)> = Vec::new();
    dedup_core::store::for_each_archive_members(&db, |rel, members| {
        if rel == "locked.zip" {
            for m in members {
                found.push((m.rel_path, m.size, m.locked, m.hash.is_some()));
            }
        }
        Ok(())
    })?;
    found.sort();
    assert_eq!(found.len(), 2, "both members indexed, none dropped");
    let secret = found.iter().find(|(n, ..)| n == "secret.txt").unwrap();
    assert_eq!(secret.1, 19, "the locked member's size is known");
    assert!(secret.2, "secret.txt is marked locked");
    assert!(!secret.3, "a locked member has no content hash");
    let readme = found.iter().find(|(n, ..)| n == "readme.txt").unwrap();
    assert!(!readme.2, "the plaintext member is not locked");
    assert!(readme.3, "and has a content hash");

    // Coverage: 2 members, 1 present (the readable one matches loose), 1 locked,
    // not redundant — the locked member blocks it.
    let cov = dedup_core::archive::repo_archive_coverage(&store, "arch", &["loose"])?;
    let c = cov.iter().find(|c| c.rel_path == "locked.zip").unwrap();
    assert_eq!((c.members, c.present, c.locked), (2, 1, 1));
    assert!(
        !c.redundant,
        "a locked member keeps the archive non-redundant"
    );
    assert!(c.has_locked());
    Ok(())
}

/// Extracting an archive into a repo folder, then scanning, makes its members
/// loose indexed content — recovery re-enters triage. The source zip is
/// untouched, and a name collision never overwrites an existing file.
#[test]
fn extract_all_into_a_repo_then_scan_indexes_the_members() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    let zip = tmp.path().join("backup.zip");
    write_zip(
        &zip,
        &[("docs/note.txt", b"the note"), ("photo.bin", b"PIXELS!")],
    );
    let zip_before = std::fs::read(&zip)?;

    // A repo with a pre-existing file that collides with a member's name.
    let repo_dir = tmp.path().join("recovered");
    std::fs::create_dir_all(repo_dir.join("docs"))?;
    std::fs::write(repo_dir.join("docs/note.txt"), b"i was here first")?;
    store.create_repo("recovered", &repo_dir.to_string_lossy())?;

    let n = dedup_core::archive::extract_all(
        &zip,
        "backup.zip",
        Some("application/zip"),
        &repo_dir,
        None,
    )?;
    assert_eq!(n, 2, "both members written");

    // The pre-existing file is untouched; the colliding member landed beside it.
    assert_eq!(
        std::fs::read(repo_dir.join("docs/note.txt"))?,
        b"i was here first"
    );
    assert!(
        repo_dir.join("docs/note_1.txt").exists(),
        "the collision got a _1 suffix"
    );
    assert_eq!(std::fs::read(&zip)?, zip_before, "source zip untouched");

    // Scanning the repo now indexes the extracted members as loose content.
    update_repo(
        &store,
        "recovered",
        1,
        &NoProgress,
        &CancellationToken::new(),
    )?;
    assert!(
        store.get_file_entry("recovered", "photo.bin")?.is_some(),
        "the extracted member is now loose indexed content"
    );
    Ok(())
}

/// Unlocking with the right password re-indexes the encrypted member's real
/// content (it stops being locked); a wrong password does nothing. The working
/// password can be stored encrypted and read back only with the app passphrase.
#[test]
fn unlock_reindexes_the_encrypted_member_and_password_persists_encrypted() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    let dir = tmp.path().join("arch");
    std::fs::create_dir_all(&dir)?;
    write_mixed_encrypted_zip(
        &dir.join("locked.zip"),
        ("readme.txt", b"plain"),
        ("secret.txt", b"classified"),
        "sesame",
    );
    let zip_before = std::fs::read(dir.join("locked.zip"))?;
    store.create_repo("arch", &dir.to_string_lossy())?;
    update_repo(&store, "arch", 1, &NoProgress, &CancellationToken::new())?;

    // Wrong password: no change, still locked.
    assert!(!dedup_core::archive::unlock_and_reindex(
        &store,
        "arch",
        "locked.zip",
        Some("application/zip"),
        "wrong",
    )?);
    let cov = dedup_core::archive::repo_archive_coverage(&store, "arch", &[])?;
    assert_eq!(
        cov.iter()
            .find(|c| c.rel_path == "locked.zip")
            .unwrap()
            .locked,
        1
    );

    // Right password: the member is re-indexed with its real content, no longer locked.
    assert!(dedup_core::archive::unlock_and_reindex(
        &store,
        "arch",
        "locked.zip",
        Some("application/zip"),
        "sesame",
    )?);
    let cov = dedup_core::archive::repo_archive_coverage(&store, "arch", &[])?;
    let c = cov.iter().find(|c| c.rel_path == "locked.zip").unwrap();
    assert_eq!(c.locked, 0, "no locked members after unlock");
    assert_eq!(c.members, 2);
    assert_eq!(
        std::fs::read(dir.join("locked.zip"))?,
        zip_before,
        "archive untouched"
    );

    // The working password persists encrypted, readable only with the passphrase.
    let db = store.open_repo_db("arch")?;
    let blob = dedup_core::secret::encrypt("app-pass", "sesame").expect("encrypt");
    dedup_core::store::set_archive_password(&db, "locked.zip", &blob)?;
    let stored = dedup_core::store::get_archive_password(&db, "locked.zip")?.expect("stored");
    assert!(
        !stored.windows(6).any(|w| w == b"sesame"),
        "the stored password is not plaintext"
    );
    assert_eq!(
        dedup_core::secret::decrypt("app-pass", &stored).as_deref(),
        Some("sesame"),
        "the passphrase recovers the working password"
    );
    assert_eq!(dedup_core::secret::decrypt("bad-pass", &stored), None);
    Ok(())
}

/// Built-in recovery finds a weak password (a wordlist word plus a common
/// digit suffix) but honestly gives up on a strong one.
#[test]
fn recover_password_finds_a_weak_password_and_gives_up_on_a_strong_one() -> TestResult {
    // A small wordlist keeps the PBKDF2-per-guess cost of this test bounded.
    let list = vec!["apple".to_string(), "dragon".to_string()];
    let tmp = tempfile::tempdir()?;
    let weak = tmp.path().join("weak.zip");
    write_mixed_encrypted_zip(&weak, ("a.txt", b"x"), ("b.txt", b"y"), "dragon1");

    // "dragon1" is a mutation of the wordlist word "dragon".
    let found = dedup_core::archive::recover_password(
        &weak,
        "weak.zip",
        Some("application/zip"),
        &[],
        &list,
        || false,
    );
    assert_eq!(found.as_deref(), Some("dragon1"), "weak password recovered");
    // The built-in list is available for the zero-config attempt.
    assert!(dedup_core::archive::builtin_wordlist().contains(&"dragon".to_string()));

    // A strong password is not in the list and won't fall.
    let strong = tmp.path().join("strong.zip");
    write_mixed_encrypted_zip(&strong, ("a.txt", b"x"), ("b.txt", b"y"), "Xk9$mQ2!zLp#7");
    let found = dedup_core::archive::recover_password(
        &strong,
        "strong.zip",
        Some("application/zip"),
        &[],
        &list,
        || false,
    );
    assert_eq!(found, None, "a strong password is honestly not recovered");

    // A known password (spray-book) is tried first and found.
    let found = dedup_core::archive::recover_password(
        &strong,
        "strong.zip",
        Some("application/zip"),
        &["Xk9$mQ2!zLp#7".to_string()],
        &[],
        || false,
    );
    assert_eq!(
        found.as_deref(),
        Some("Xk9$mQ2!zLp#7"),
        "a known password is tried first"
    );
    Ok(())
}

/// The hashcat `$zip2$` export has the right shape for an AES-256 zip: mode 3,
/// a 16-byte salt, a 2-byte verifier, a 10-byte auth code, and a data length
/// consistent with the member's encrypted size. (Acceptance by hashcat itself
/// is verified by a human; this pins the structure.)
#[test]
fn export_hashcat_hash_has_the_zip2_shape_for_aes256() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = tmp.path().join("locked.zip");
    write_mixed_encrypted_zip(
        &zip,
        ("a.txt", b"x"),
        ("secret.txt", b"classified data here"),
        "pw",
    );

    let h = dedup_core::archive::export_hashcat_hash(&zip, "locked.zip", Some("application/zip"))
        .expect("an AES member produces a hash");
    assert!(
        h.starts_with("$zip2$*0*3*0*"),
        "type 0, AES-256 mode 3: {h}"
    );
    assert!(h.ends_with("*$/zip2$"), "well-formed trailer: {h}");
    let fields: Vec<&str> = h
        .trim_start_matches("$zip2$*")
        .trim_end_matches("*$/zip2$")
        .split('*')
        .collect();
    // type, mode, magic, salt, verifier, len, data, auth
    assert_eq!(fields.len(), 8, "eight fields: {fields:?}");
    assert_eq!(fields[3].len(), 32, "16-byte salt as 32 hex chars");
    assert_eq!(fields[4].len(), 4, "2-byte password verifier");
    assert_eq!(fields[7].len(), 20, "10-byte auth code");
    let data_len: usize = fields[5].parse().expect("decimal data length");
    assert_eq!(
        fields[6].len(),
        data_len * 2,
        "data length matches the hex data"
    );

    // A plaintext zip has nothing to export.
    let plain = tmp.path().join("plain.zip");
    write_zip(&plain, &[("a.txt", b"hello")]);
    assert!(
        dedup_core::archive::export_hashcat_hash(&plain, "plain.zip", Some("application/zip"))
            .is_none()
    );
    Ok(())
}

/// `members_by_content` maps content identity to the archives that contain it,
/// so a loose file can be looked up to find its archived copies. Locked members
/// (unknown content) never appear.
#[test]
fn members_by_content_maps_content_to_its_archives() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    let dir = tmp.path().join("arch");
    std::fs::create_dir_all(&dir)?;
    write_zip(&dir.join("a.zip"), &[("photo.jpg", b"PIXELS")]);
    write_zip(
        &dir.join("b.zip"),
        &[("copy.jpg", b"PIXELS"), ("only.txt", b"unique")],
    );
    write_mixed_encrypted_zip(
        &dir.join("c.zip"),
        ("plain.txt", b"unique"),
        ("secret", b"PIXELS"),
        "pw",
    );
    store.create_repo("arch", &dir.to_string_lossy())?;
    update_repo(&store, "arch", 1, &NoProgress, &CancellationToken::new())?;

    let map = dedup_core::archive::members_by_content(&store, &["arch"])?;

    // "PIXELS" (6 bytes) is inside a.zip and b.zip as readable members — but the
    // encrypted copy in c.zip is locked and must not appear.
    let hash = *blake3::hash(b"PIXELS").as_bytes();
    let occ = map.get(&(6, hash)).expect("PIXELS content is indexed");
    let mut archives: Vec<&str> = occ.iter().map(|o| o.archive_rel.as_str()).collect();
    archives.sort();
    assert_eq!(
        archives,
        vec!["a.zip", "b.zip"],
        "readable copies only, no locked one"
    );

    // "unique" is in b.zip (loose member) and c.zip (plaintext member).
    let uhash = *blake3::hash(b"unique").as_bytes();
    assert_eq!(map.get(&(6, uhash)).map(|v| v.len()), Some(2));
    Ok(())
}

/// An unchanged archive is skipped on re-scan (its member list persists), and a
/// changed archive is re-read and its member list replaced wholesale.
#[test]
fn unchanged_archive_is_skipped_changed_archive_is_reread() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    let dir = tmp.path().join("arch");
    std::fs::create_dir_all(&dir)?;
    write_zip(
        &dir.join("box.zip"),
        &[("one.txt", b"1"), ("two.txt", b"2")],
    );
    store.create_repo("arch", &dir.to_string_lossy())?;

    let stats = update_repo(&store, "arch", 1, &NoProgress, &CancellationToken::new())?;
    assert_eq!(stats.added, 1, "the zip is a new file");
    let cov = dedup_core::archive::repo_archive_coverage(&store, "arch", &[])?;
    assert_eq!(
        cov.iter()
            .find(|c| c.rel_path == "box.zip")
            .unwrap()
            .members,
        2
    );

    // Re-scan with nothing changed: the archive is not re-read (reported
    // unchanged), and its member list is still there.
    let stats = update_repo(&store, "arch", 1, &NoProgress, &CancellationToken::new())?;
    assert_eq!(stats.updated, 0, "unchanged archive is not re-hashed");
    assert_eq!(stats.unchanged, 1);
    let cov = dedup_core::archive::repo_archive_coverage(&store, "arch", &[])?;
    assert_eq!(
        cov.iter()
            .find(|c| c.rel_path == "box.zip")
            .unwrap()
            .members,
        2
    );

    // Replace the zip with a three-member one (new content hash), re-scan: its
    // member list is replaced wholesale.
    std::thread::sleep(std::time::Duration::from_millis(10));
    write_zip(
        &dir.join("box.zip"),
        &[("one.txt", b"1"), ("two.txt", b"2"), ("three.txt", b"3")],
    );
    let stats = update_repo(&store, "arch", 1, &NoProgress, &CancellationToken::new())?;
    assert_eq!(stats.updated, 1, "the changed archive is re-hashed");
    let cov = dedup_core::archive::repo_archive_coverage(&store, "arch", &[])?;
    assert_eq!(
        cov.iter()
            .find(|c| c.rel_path == "box.zip")
            .unwrap()
            .members,
        3,
        "member list replaced with the new archive's members"
    );
    Ok(())
}

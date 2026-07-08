//! Integration tests for `dedup diff ...` and `dedup repo dupes` running the
//! real binary against an isolated HOME.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Sandbox {
    home: TempDir,
}

impl Sandbox {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            home: TempDir::new()?,
        })
    }

    fn dedup(&self) -> Result<Command, Box<dyn std::error::Error>> {
        let mut cmd = Command::cargo_bin("dedup")?;
        cmd.env("HOME", self.home.path());
        Ok(cmd)
    }

    fn write(&self, rel: &str, content: &[u8]) -> TestResult {
        let path = self.home.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    fn path(&self, rel: &str) -> String {
        self.home.path().join(rel).to_string_lossy().into_owned()
    }

    /// Create the repo dir, register it, and index it.
    fn repo(&self, name: &str) -> TestResult {
        std::fs::create_dir_all(self.home.path().join(name))?;
        self.dedup()?
            .args(["repo", "create", name, &self.path(name)])
            .assert()
            .success();
        self.dedup()?
            .args(["repo", "update", name])
            .assert()
            .success();
        Ok(())
    }
}

#[test]
fn diff_print_reports_new_files() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("A/only_a.txt", b"unique")?;
    sb.write("A/both.txt", b"shared")?;
    sb.write("B/other.txt", b"shared")?;
    sb.repo("A")?;
    sb.repo("B")?;

    sb.dedup()?
        .args(["diff", "print", "A", "B"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("New: only_a.txt").and(predicate::str::contains(
                "1 new, 1 equal, 0 deleted in reference",
            )),
        );
    Ok(())
}

#[test]
fn diff_sync_copies_and_reports() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("A/data.txt", b"payload")?;
    sb.repo("A")?;
    sb.repo("B")?;

    sb.dedup()?
        .args(["diff", "sync", "A", "B"])
        .assert()
        .success()
        .stdout(predicate::str::contains("copied: 1"));

    assert_eq!(
        std::fs::read(sb.home.path().join("B/data.txt"))?,
        b"payload"
    );
    Ok(())
}

#[test]
fn diff_rm_deletes_files_known_to_reference() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("A/dupe.txt", b"shared")?;
    sb.write("A/unique.txt", b"mine")?;
    sb.write("B/copy.txt", b"shared")?;
    sb.repo("A")?;
    sb.repo("B")?;

    sb.dedup()?
        .args(["diff", "rm", "A", "B"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted 1 files from 'A'."));
    assert!(!sb.home.path().join("A/dupe.txt").exists());
    assert!(sb.home.path().join("A/unique.txt").exists());
    Ok(())
}

#[test]
fn diff_cp_into_places_files_under_subdir() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("A/photos/2020/a.jpg", b"img")?;
    sb.repo("A")?;
    sb.repo("B")?;
    let target = sb.path("out");

    sb.dedup()?
        .args(["diff", "cp", "A", "B", &target, "--into", "imports/batch1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Copied 1 files to"));

    assert_eq!(
        std::fs::read(sb.home.path().join("out/imports/batch1/photos/2020/a.jpg"))?,
        b"img"
    );
    // Source is left untouched by a copy.
    assert!(sb.home.path().join("A/photos/2020/a.jpg").exists());
    Ok(())
}

#[test]
fn diff_mv_into_places_files_under_subdir_and_removes_source() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("A/docs/note.txt", b"hi")?;
    sb.repo("A")?;
    sb.repo("B")?;
    let target = sb.path("out");

    sb.dedup()?
        .args(["diff", "mv", "A", "B", &target, "-i", "archive"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Moved 1 files to"));

    assert_eq!(
        std::fs::read(sb.home.path().join("out/archive/docs/note.txt"))?,
        b"hi"
    );
    assert!(!sb.home.path().join("A/docs/note.txt").exists());
    Ok(())
}

#[test]
fn diff_with_invalid_filter_fails() -> TestResult {
    let sb = Sandbox::new()?;
    sb.repo("A")?;
    sb.repo("B")?;

    sb.dedup()?
        .args(["diff", "print", "A", "B", "--filter", "bogus:x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Unknown filter"));
    Ok(())
}

#[test]
fn dupes_prints_groups_and_summary() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("A/one.txt", b"same bytes")?;
    sb.write("A/two.txt", b"same bytes")?;
    sb.repo("A")?;

    sb.dedup()?
        .args(["repo", "dupes", "A"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("one.txt")
                .and(predicate::str::contains("two.txt"))
                .and(predicate::str::contains("1 duplicate groups")),
        );
    Ok(())
}

#[test]
fn dupes_delete_keeps_one_copy() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("A/first.txt", b"identical")?;
    sb.write("A/second.txt", b"identical")?;
    sb.repo("A")?;

    sb.dedup()?
        .args(["repo", "dupes", "A", "--delete"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted 1 duplicate files"));

    let first = sb.home.path().join("A/first.txt").exists();
    let second = sb.home.path().join("A/second.txt").exists();
    assert!(
        first != second,
        "exactly one of the two copies must survive"
    );
    Ok(())
}

#[test]
fn dupes_finds_cross_repo_groups() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("A/a.bin", b"cross-repo content")?;
    sb.write("B/b.bin", b"cross-repo content")?;
    sb.repo("A")?;
    sb.repo("B")?;

    sb.dedup()?
        .args(["repo", "dupes", "--all"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("a.bin")
                .and(predicate::str::contains("b.bin"))
                .and(predicate::str::contains("1 duplicate groups")),
        );
    Ok(())
}

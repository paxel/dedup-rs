//! Integration tests for the `dedup repo` CLI commands.
//!
//! Each test runs the real binary against an isolated config dir by pointing
//! `HOME` at a tempdir, so tests never touch `~/.config/dedup` and can run in
//! parallel.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A sandbox holding an isolated HOME and data directories to index.
struct Sandbox {
    home: TempDir,
}

impl Sandbox {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            home: TempDir::new()?,
        })
    }

    /// A `dedup` command with HOME pointing into the sandbox.
    fn dedup(&self) -> Result<Command, Box<dyn std::error::Error>> {
        let mut cmd = Command::cargo_bin("dedup")?;
        cmd.env("HOME", self.home.path());
        Ok(cmd)
    }

    /// Create a data directory inside the sandbox and return its path as a string.
    fn data_dir(&self, name: &str) -> Result<String, Box<dyn std::error::Error>> {
        let dir = self.home.path().join(name);
        std::fs::create_dir_all(&dir)?;
        Ok(dir.to_string_lossy().into_owned())
    }
}

#[test]
fn version_prints() -> TestResult {
    Sandbox::new()?
        .dedup()?
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("dedup"));
    Ok(())
}

#[test]
fn ls_on_empty_registry_prints_hint() -> TestResult {
    Sandbox::new()?
        .dedup()?
        .args(["repo", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No repositories registered"));
    Ok(())
}

#[test]
fn create_then_ls_shows_repo() -> TestResult {
    let sb = Sandbox::new()?;
    let data = sb.data_dir("photos")?;

    sb.dedup()?
        .args(["repo", "create", "photos", &data])
        .assert()
        .success()
        .stdout(predicate::str::contains("photos"));

    sb.dedup()?
        .args(["repo", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("photos"));
    Ok(())
}

#[test]
fn create_duplicate_name_fails() -> TestResult {
    let sb = Sandbox::new()?;
    let data = sb.data_dir("photos")?;

    sb.dedup()?
        .args(["repo", "create", "photos", &data])
        .assert()
        .success();

    sb.dedup()?
        .args(["repo", "create", "photos", &data])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
    Ok(())
}

#[test]
fn rm_removes_repo() -> TestResult {
    let sb = Sandbox::new()?;
    let data = sb.data_dir("photos")?;

    sb.dedup()?
        .args(["repo", "create", "photos", &data])
        .assert()
        .success();
    sb.dedup()?
        .args(["repo", "rm", "photos"])
        .assert()
        .success();

    sb.dedup()?
        .args(["repo", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No repositories registered"));
    Ok(())
}

#[test]
fn rm_unknown_repo_fails() -> TestResult {
    Sandbox::new()?
        .dedup()?
        .args(["repo", "rm", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
    Ok(())
}

#[test]
fn mv_renames_repo() -> TestResult {
    let sb = Sandbox::new()?;
    let data = sb.data_dir("data")?;

    sb.dedup()?
        .args(["repo", "create", "photos", &data])
        .assert()
        .success();
    sb.dedup()?
        .args(["repo", "mv", "photos", "pictures"])
        .assert()
        .success();

    sb.dedup()?
        .args(["repo", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pictures").and(predicate::str::contains("photos").not()));
    Ok(())
}

#[test]
fn mv_to_existing_name_fails() -> TestResult {
    let sb = Sandbox::new()?;
    let a = sb.data_dir("a")?;
    let b = sb.data_dir("b")?;

    sb.dedup()?
        .args(["repo", "create", "a", &a])
        .assert()
        .success();
    sb.dedup()?
        .args(["repo", "create", "b", &b])
        .assert()
        .success();

    sb.dedup()?
        .args(["repo", "mv", "a", "b"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
    Ok(())
}

#[test]
fn rel_changes_repo_path() -> TestResult {
    let sb = Sandbox::new()?;
    let data = sb.data_dir("old-place")?;
    let new_data = sb.data_dir("new-place")?;

    sb.dedup()?
        .args(["repo", "create", "photos", &data])
        .assert()
        .success();
    sb.dedup()?
        .args(["repo", "rel", "photos", &new_data])
        .assert()
        .success();

    sb.dedup()?
        .args(["repo", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("new-place"));
    Ok(())
}

#[test]
fn rel_unknown_repo_fails() -> TestResult {
    Sandbox::new()?
        .dedup()?
        .args(["repo", "rel", "nope", "/tmp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
    Ok(())
}

#[test]
fn ls_handles_multibyte_paths() -> TestResult {
    let sb = Sandbox::new()?;
    // Long enough to trigger path truncation in `ls`, with multi-byte chars
    // sitting on the truncation boundaries.
    let data = sb.data_dir("Ürlaubsfötos-Sommer-2026-ÄxÖfen-Übermut")?;

    sb.dedup()?
        .args(["repo", "create", "fotos", &data])
        .assert()
        .success();

    sb.dedup()?.args(["repo", "ls"]).assert().success();
    Ok(())
}

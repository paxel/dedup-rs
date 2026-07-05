//! Integration tests for `dedup repo update` running the real binary
//! against an isolated HOME.

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
}

#[test]
fn update_indexes_files_and_ls_shows_counts() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("data/a.txt", b"hello")?;
    sb.write("data/sub/b.txt", b"world")?;
    let data = sb.home.path().join("data").to_string_lossy().into_owned();

    sb.dedup()?
        .args(["repo", "create", "docs", &data])
        .assert()
        .success();

    sb.dedup()?
        .args(["repo", "update", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added: 2"));

    sb.dedup()?
        .args(["repo", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("10 B"));

    // Second run on the unchanged tree hashes nothing.
    sb.dedup()?
        .args(["repo", "update", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unchanged: 2").and(predicate::str::contains("added: 0")));
    Ok(())
}

#[test]
fn update_all_updates_every_repo() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("one/a.txt", b"aaa")?;
    sb.write("two/b.txt", b"bbb")?;
    let one = sb.home.path().join("one").to_string_lossy().into_owned();
    let two = sb.home.path().join("two").to_string_lossy().into_owned();

    sb.dedup()?
        .args(["repo", "create", "one", &one])
        .assert()
        .success();
    sb.dedup()?
        .args(["repo", "create", "two", &two])
        .assert()
        .success();

    sb.dedup()?
        .args(["repo", "update", "--all"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Updating 'one'")
                .and(predicate::str::contains("Updating 'two'")),
        );
    Ok(())
}

#[test]
fn update_without_names_or_all_fails() -> TestResult {
    Sandbox::new()?
        .dedup()?
        .args(["repo", "update"])
        .assert()
        .failure();
    Ok(())
}

#[test]
fn update_unknown_repo_fails() -> TestResult {
    Sandbox::new()?
        .dedup()?
        .args(["repo", "update", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
    Ok(())
}

#[test]
fn update_all_with_empty_registry_fails() -> TestResult {
    Sandbox::new()?
        .dedup()?
        .args(["repo", "update", "--all"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No repositories registered"));
    Ok(())
}

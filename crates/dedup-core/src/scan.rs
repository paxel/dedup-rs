//! Important-file scanner: flag likely-critical files — crypto wallets, key
//! material, password vaults, and identity/financial documents — so a caretaker
//! reviews them first, before the disks are wiped. Everything here is advisory
//! and read-only; nothing touches or moves the flagged files.
//!
//! Rules are filename/extension/path patterns plus cheap content probes on
//! small files. Every flag states its reason, and rules are tuned toward
//! precision so a photo library yields (near-)zero false positives.

use crate::store::{self, Store, StoreError};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Wallet,
    Key,
    Vault,
    Identity,
    Financial,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Wallet => "Wallets",
            Category::Key => "Keys",
            Category::Vault => "Vaults",
            Category::Identity => "Identity",
            Category::Financial => "Financial",
        }
    }
}

/// A flagged file with the reason it was flagged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    pub repo: String,
    pub rel_path: String,
    pub category: Category,
    pub reason: String,
}

/// Content probes only read files up to this size (headers / small secrets).
const PROBE_CAP: u64 = 1024 * 1024;
/// Seed-phrase probe only considers small text files.
const SEED_CAP: u64 = 16 * 1024;

/// Scan every non-missing file in the given repos, returning the flags found.
/// Recompute-on-demand: no results are persisted.
pub fn scan_repos(store: &Store, repo_names: &[String]) -> Result<Vec<Flag>, StoreError> {
    let mut flags = Vec::new();
    for name in repo_names {
        let root = PathBuf::from(&store.get_repo(name)?.abs_path);
        let db = store.open_repo_db(name)?;
        store::for_each_file_entry(&db, |rel_path, entry| {
            if entry.missing {
                return Ok(());
            }
            if let Some((category, reason)) = classify(rel_path, entry.size, &root.join(rel_path)) {
                flags.push(Flag {
                    repo: name.clone(),
                    rel_path: rel_path.to_string(),
                    category,
                    reason,
                });
            }
            Ok(())
        })?;
    }
    Ok(flags)
}

/// Classify one file. Filename/path rules run first (no I/O); content probes on
/// small files strengthen or add flags. `abs_path` is the on-disk location.
pub fn classify(rel_path: &str, size: u64, abs_path: &Path) -> Option<(Category, String)> {
    let lower = rel_path.replace('\\', "/").to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);

    // --- Filename / extension / path rules --------------------------------
    if name == "wallet.dat" {
        return Some((Category::Wallet, "filename matches wallet.dat".into()));
    }
    if name.ends_with(".wallet") {
        return Some((Category::Wallet, "wallet file extension".into()));
    }
    if name.ends_with(".kdbx") || name.ends_with(".kdb") {
        return Some((Category::Vault, "KeePass database".into()));
    }
    if name.ends_with(".1pif") {
        return Some((Category::Vault, "1Password export".into()));
    }
    if lower.contains("/.gnupg/") || lower.starts_with(".gnupg/") {
        return Some((Category::Key, "GnuPG keyring path".into()));
    }
    let ssh_key_name = matches!(name, "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519");
    if ssh_key_name || lower.contains("/.ssh/") || lower.starts_with(".ssh/") {
        if name.ends_with(".pub") {
            // A public key is not sensitive on its own.
        } else {
            return Some((Category::Key, "SSH key material".into()));
        }
    }
    for ext in [".pem", ".p12", ".pfx", ".gpg", ".asc", ".key"] {
        if name.ends_with(ext) {
            return Some((Category::Key, format!("key/cert file ({ext})")));
        }
    }
    if let Some(hit) = keyword_flag(name) {
        return Some(hit);
    }

    // --- Content probes (small files only) --------------------------------
    if size <= PROBE_CAP
        && let Ok(bytes) = std::fs::read(abs_path)
    {
        if is_berkeley_db(&bytes) {
            return Some((Category::Wallet, "Berkeley DB (wallet.dat family)".into()));
        }
        if let Some(reason) = openssh_key_reason(&bytes) {
            return Some((Category::Key, reason));
        }
        if let Some(reason) = keystore_json_reason(&bytes) {
            return Some((Category::Wallet, reason));
        }
        if size <= SEED_CAP
            && let Ok(text) = std::str::from_utf8(&bytes)
            && seed_phrase_span(text)
        {
            return Some((
                Category::Wallet,
                "contains a BIP-39 seed phrase (12+ wordlist words)".into(),
            ));
        }
    }
    None
}

/// Distinctive identity/financial filename keywords (multilingual). Matched as
/// whole tokens of the filename stem so common substrings don't false-positive.
fn keyword_flag(name: &str) -> Option<(Category, String)> {
    const FINANCIAL: &[&str] = &[
        "steuer",
        "steuererklärung",
        "steuererklaerung",
        "tax",
        "invoice",
        "rechnung",
        "kontoauszug",
        "iban",
        "versicherung",
        "insurance",
    ];
    const IDENTITY: &[&str] = &[
        "testament",
        "vollmacht",
        "passport",
        "reisepass",
        "ausweis",
        "urkunde",
        "geburtsurkunde",
    ];
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let tokens: Vec<&str> = stem.split(|c: char| !c.is_alphanumeric()).collect();
    let has = |set: &[&str]| tokens.iter().any(|t| set.contains(t));
    if has(FINANCIAL) {
        return Some((
            Category::Financial,
            "financial-document filename keyword".into(),
        ));
    }
    if has(IDENTITY) {
        return Some((Category::Identity, "vital-record filename keyword".into()));
    }
    None
}

/// Berkeley DB magic (0x00053162) at offset 12, either endianness — the format
/// Bitcoin Core's `wallet.dat` uses.
fn is_berkeley_db(bytes: &[u8]) -> bool {
    if bytes.len() < 16 {
        return false;
    }
    let at = &bytes[12..16];
    at == [0x00, 0x05, 0x31, 0x62] || at == [0x62, 0x31, 0x05, 0x00]
}

fn openssh_key_reason(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(200)];
    let text = String::from_utf8_lossy(head);
    for marker in [
        "BEGIN OPENSSH PRIVATE KEY",
        "BEGIN RSA PRIVATE KEY",
        "BEGIN EC PRIVATE KEY",
        "BEGIN DSA PRIVATE KEY",
        "BEGIN PGP PRIVATE KEY",
    ] {
        if text.contains(marker) {
            return Some(format!("private key header ({marker})"));
        }
    }
    None
}

/// Ethereum keystore / Electrum wallet JSON, recognized by their key fields.
fn keystore_json_reason(bytes: &[u8]) -> Option<String> {
    if bytes.len() > 256 * 1024 {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let has = |k: &str| text.contains(k);
    if has("\"cipher\"") && has("\"kdfparams\"") && (has("\"crypto\"") || has("\"Crypto\"")) {
        return Some("Ethereum keystore JSON (crypto/cipher/kdfparams)".into());
    }
    if has("\"seed_version\"") || (has("\"wallet_type\"") && has("\"keystore\"")) {
        return Some("Electrum wallet JSON".into());
    }
    None
}

/// Whether `text` contains a run of at least 12 consecutive whitespace-separated
/// tokens that are all BIP-39 words — the shape (and vocabulary) of a mnemonic
/// seed phrase. The 12-in-a-row bar keeps prose from tripping it.
fn seed_phrase_span(text: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static WORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    let set = WORDS.get_or_init(|| {
        bip39::Language::English
            .word_list()
            .iter()
            .copied()
            .collect()
    });
    let mut run = 0usize;
    for token in text.split(|c: char| !c.is_ascii_alphabetic()) {
        if token.is_empty() {
            continue;
        }
        if set.contains(token.to_lowercase().as_str()) {
            run += 1;
            if run >= 12 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classify a file by name only (no content), for the filename-rule cases.
    fn by_name(rel: &str) -> Option<(Category, String)> {
        // A nonexistent path → content probes are skipped, exercising the
        // filename rules in isolation.
        classify(rel, 10, Path::new("/nonexistent-dedup-scan-test"))
    }

    #[test]
    fn filename_rules_flag_by_category() {
        assert_eq!(by_name("btc/wallet.dat").unwrap().0, Category::Wallet);
        assert_eq!(by_name("keys/mine.wallet").unwrap().0, Category::Wallet);
        assert_eq!(by_name("vault/passwords.kdbx").unwrap().0, Category::Vault);
        assert_eq!(by_name("home/.ssh/id_ed25519").unwrap().0, Category::Key);
        assert_eq!(by_name("certs/server.pem").unwrap().0, Category::Key);
        assert_eq!(
            by_name("docs/Steuer_2021.pdf").unwrap().0,
            Category::Financial
        );
        assert_eq!(
            by_name("legal/Testament.docx").unwrap().0,
            Category::Identity
        );

        // A public key and ordinary photos are not flagged.
        assert!(by_name("home/.ssh/id_ed25519.pub").is_none());
        assert!(by_name("photos/PXL_20230830_family.jpg").is_none());
        assert!(
            by_name("photos/willow_lake.jpg").is_none(),
            "no substring FP"
        );
    }

    #[test]
    fn content_probes_flag_secrets() {
        let dir = tempfile::tempdir().unwrap();

        // Ethereum keystore JSON.
        let ks = dir.path().join("UTC--2021--addr.json");
        std::fs::write(
            &ks,
            br#"{"version":3,"crypto":{"cipher":"aes-128-ctr","kdfparams":{"n":8192}}}"#,
        )
        .unwrap();
        let (cat, reason) = classify("UTC--2021--addr.json", 80, &ks).unwrap();
        assert_eq!(cat, Category::Wallet);
        assert!(reason.contains("keystore"));

        // OpenSSH private key by header (name doesn't match any rule).
        let key = dir.path().join("backup_blob");
        std::fs::write(&key, b"-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n").unwrap();
        assert_eq!(classify("backup_blob", 40, &key).unwrap().0, Category::Key);

        // BIP-39 seed phrase in a small text file.
        let seed = dir.path().join("notes.txt");
        let phrase = "abandon ability able about above absent absorb abstract \
                      absurd abuse access accident";
        std::fs::write(&seed, phrase).unwrap();
        let (cat, reason) = classify("notes.txt", phrase.len() as u64, &seed).unwrap();
        assert_eq!(cat, Category::Wallet);
        assert!(reason.contains("seed phrase"));

        // Ordinary prose is not a seed phrase.
        let prose = dir.path().join("diary.txt");
        std::fs::write(
            &prose,
            b"today i went to the lake and had a lovely picnic with friends",
        )
        .unwrap();
        assert!(classify("diary.txt", 60, &prose).is_none());
    }
}

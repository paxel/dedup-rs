//! Stage-4 ordering: browse the sanitized corpus by *when it happened* and
//! export it into a dated `<year>/<month>/` tree. Dates use the best-known
//! signal — EXIF capture time, falling back to file mtime — so it works
//! (degraded) even without EXIF. Export copies, never moves (v1).

use crate::diff::{DiffAction, DiffError, DiffEvent, DiffRun};
use crate::filter::{self, FileFilter};
use crate::store::{self, FileEntry, Store, StoreError};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// One year/month bucket with its file count and total bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub year: i64,
    pub month: u32,
    pub count: u64,
    pub bytes: u64,
}

/// Bucket every non-missing file passing `filter` by (year, month) of its
/// best-known date. Streams the index; buckets come back chronologically.
pub fn timeline_buckets(
    store: &Store,
    repo_names: &[String],
    filter: Option<&str>,
) -> Result<Vec<Bucket>, StoreError> {
    let filter = FileFilter::parse(filter).map_err(|e| StoreError::Serialization(e.to_string()))?;
    let mut map: BTreeMap<(i64, u32), (u64, u64)> = BTreeMap::new();
    for name in repo_names {
        let db = store.open_repo_db(name)?;
        let annotated = filter::AnnotatedFilter::new(&db, &filter)?;
        store::for_each_file_entry(&db, |rel_path, entry| {
            if entry.missing || !annotated.matches(rel_path, &entry) {
                return Ok(());
            }
            let (y, m, _) = filter::ms_to_ymd(filter::best_date_ms(&entry));
            let slot = map.entry((y, m)).or_default();
            slot.0 += 1;
            slot.1 += entry.size;
            Ok(())
        })?;
    }
    Ok(map
        .into_iter()
        .map(|((year, month), (count, bytes))| Bucket {
            year,
            month,
            count,
            bytes,
        })
        .collect())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportStats {
    pub copied: u64,
    /// Files whose content already exists at a destination (a re-run).
    pub skipped: u64,
    pub errors: u64,
}

/// Copy every non-missing file passing `filter` into
/// `target_dir/<year>/<month>/<filename>`, using the best-known date. Names
/// that collide get a numeric suffix; a destination that already holds the
/// same content is skipped, so re-running an export is idempotent. Existing
/// files are never overwritten. Never moves.
pub fn export_by_date(
    store: &Store,
    repo_names: &[String],
    target_dir: &Path,
    filter: Option<&str>,
) -> Result<ExportStats, StoreError> {
    let filter = FileFilter::parse(filter).map_err(|e| StoreError::Serialization(e.to_string()))?;
    let mut stats = ExportStats::default();
    for name in repo_names {
        let root = PathBuf::from(&store.get_repo(name)?.abs_path);
        let db = store.open_repo_db(name)?;
        let annotated = filter::AnnotatedFilter::new(&db, &filter)?;
        // Collect first so the read transaction isn't held during file I/O.
        let mut files: Vec<(String, i64, u64, [u8; 32])> = Vec::new();
        store::for_each_file_entry(&db, |rel_path, entry| {
            if !entry.missing && annotated.matches(rel_path, &entry) {
                files.push((
                    rel_path.to_string(),
                    filter::best_date_ms(&entry),
                    entry.size,
                    entry.hash,
                ));
            }
            Ok(())
        })?;

        for (rel_path, date_ms, size, hash) in files {
            let (year, month, _) = filter::ms_to_ymd(date_ms);
            let dir = target_dir
                .join(year.to_string())
                .join(format!("{month:02}"));
            if std::fs::create_dir_all(&dir).is_err() {
                stats.errors += 1;
                continue;
            }
            let file_name = Path::new(&rel_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| rel_path.replace('/', "_"));
            match dest_for(&dir, &file_name, size, &hash) {
                Dest::Copy(dest) => match std::fs::copy(root.join(&rel_path), &dest) {
                    Ok(_) => stats.copied += 1,
                    Err(_) => stats.errors += 1,
                },
                Dest::AlreadyThere => stats.skipped += 1,
                Dest::Exhausted => stats.errors += 1,
            }
        }
    }
    Ok(stats)
}

enum Dest {
    /// Copy to this free path.
    Copy(PathBuf),
    /// A destination already holds this exact content (size + BLAKE3).
    AlreadyThere,
    /// No free candidate name found; nothing is ever overwritten.
    Exhausted,
}

/// Pick a destination in `dir` for `file_name`: the plain name, else ` (2)`,
/// ` (3)`… before the extension. An existing candidate with the same content
/// short-circuits to [`Dest::AlreadyThere`] so re-runs don't duplicate.
fn dest_for(dir: &Path, file_name: &str, size: u64, hash: &[u8; 32]) -> Dest {
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (file_name.to_string(), String::new()),
    };
    for n in 1..10_000 {
        let candidate = if n == 1 {
            dir.join(file_name)
        } else {
            dir.join(format!("{stem} ({n}){ext}"))
        };
        if !candidate.exists() {
            return Dest::Copy(candidate);
        }
        if is_same_content(&candidate, size, hash) {
            return Dest::AlreadyThere;
        }
    }
    Dest::Exhausted
}

// ---------------------------------------------------------------------------
// Rule-based in-repo reorganization (the GUI "ORGANIZE" grooming command).
//
// Each file is matched against an ordered list of rules; the first rule whose
// filter matches renders a target *relative path* from a template, and the file
// is moved there inside the same repo (index kept in sync). Files matching no
// rule are left untouched. Nothing is ever overwritten: a target occupied by
// different content is suffixed ` (2)`, same content is skipped.
// ---------------------------------------------------------------------------

/// One organize rule: files passing `filter` are moved to the path rendered by
/// `template`. `filter` of `None` matches everything (a catch-all).
#[derive(Debug, Clone)]
pub struct OrganizeRule {
    pub filter: Option<String>,
    pub template: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrganizeStats {
    /// Files actually moved to a new path.
    pub moved: u64,
    /// Files skipped (identity path, unmatched, or content already at target).
    pub skipped: u64,
    /// Files that could not be moved (I/O errors); the run continues past them.
    pub errors: u64,
    pub cancelled: bool,
}

/// The default template: reproduce a file's original relative path unchanged
/// (a no-op), so a fresh rule starts from identity and the user edits from there.
pub const DEFAULT_TEMPLATE: &str = "{o-path}/{o-name}";

/// Resolve a single template token to its value for `entry` at `rel_path`.
/// Unknown or unavailable tokens resolve to the empty string.
fn resolve_token(token: &str, entry: &FileEntry, rel_path: &str) -> String {
    let path = Path::new(rel_path);
    let sanitize = |s: String| -> String {
        s.chars()
            .map(|c| {
                if c == '/' || c == '\\' || c.is_control() {
                    '_'
                } else {
                    c
                }
            })
            .collect()
    };
    match token {
        // Original-file parts (as much of the source as possible).
        "o-path" => path
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|s| !s.is_empty() && s != ".")
            .unwrap_or_default(),
        "o-name" => path
            .file_name()
            .map(|n| sanitize(n.to_string_lossy().into_owned()))
            .unwrap_or_default(),
        "o-stem" => path
            .file_stem()
            .map(|n| sanitize(n.to_string_lossy().into_owned()))
            .unwrap_or_default(),
        "o-ext" => path
            .extension()
            .map(|e| sanitize(e.to_string_lossy().to_lowercase()))
            .unwrap_or_default(),
        // Best-known date (EXIF capture time, else mtime).
        "year" | "month" | "day" => {
            let (y, m, d) = filter::ms_to_ymd(filter::best_date_ms(entry));
            match token {
                "year" => format!("{y:04}"),
                "month" => format!("{m:02}"),
                _ => format!("{d:02}"),
            }
        }
        "mime" => entry
            .mime
            .as_deref()
            .map(|m| sanitize(m.to_string()))
            .unwrap_or_default(),
        "mimetop" => entry
            .mime
            .as_deref()
            .and_then(|m| m.split('/').next())
            .map(|m| sanitize(m.to_string()))
            .unwrap_or_default(),
        "camera" => entry
            .exif
            .as_ref()
            .and_then(|e| e.camera.clone())
            .map(sanitize)
            .unwrap_or_default(),
        "origin" => entry.origin.clone().map(sanitize).unwrap_or_default(),
        "size" => size_bucket(entry.size).to_string(),
        "w" => entry
            .img_size
            .map(|(w, _)| w.to_string())
            .unwrap_or_default(),
        "h" => entry
            .img_size
            .map(|(_, h)| h.to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Coarse human size bucket used by the `{size}` token.
fn size_bucket(bytes: u64) -> &'static str {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    match bytes {
        b if b < 100 * KB => "tiny",
        b if b < 10 * MB => "small",
        b if b < 100 * MB => "medium",
        b if b < GB => "large",
        _ => "huge",
    }
}

/// Render a `template` for one file into a raw (un-normalized) relative path.
/// `{token|alt|"literal"}` tries each alternative left to right: a token wins if
/// it resolves non-empty, a `"quoted"` literal always wins (its content, even if
/// empty). Text outside braces — including `/` separators — is copied verbatim.
fn render_template(template: &str, entry: &FileEntry, rel_path: &str) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // Unclosed brace: treat the remainder literally.
            out.push_str(&rest[open..]);
            return out;
        };
        let expr = &after[..close];
        out.push_str(&resolve_placeholder(expr, entry, rel_path));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Resolve one `a|b|"lit"` placeholder body to its chosen value.
fn resolve_placeholder(expr: &str, entry: &FileEntry, rel_path: &str) -> String {
    for alt in expr.split('|') {
        let alt = alt.trim();
        if alt.len() >= 2 && alt.starts_with('"') && alt.ends_with('"') {
            // A literal always terminates the choice.
            return alt[1..alt.len() - 1].to_string();
        }
        let value = resolve_token(alt, entry, rel_path);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

/// Normalize a rendered path into a safe repo-relative path: trim each segment
/// (dropping trailing dots/spaces), drop empty/`.` segments, and reject any
/// `..` segment (which would escape the repo). Returns `None` if nothing usable
/// remains.
fn normalize_rel(rendered: &str) -> Option<String> {
    let mut segments = Vec::new();
    for raw in rendered.split('/') {
        let seg = raw.trim().trim_end_matches(['.', ' ']);
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return None;
        }
        segments.push(seg);
    }
    if segments.is_empty() {
        None
    } else {
        Some(segments.join("/"))
    }
}

/// The compiled form of a rule (its parsed filter) alongside its template.
struct CompiledRule {
    filter: FileFilter,
    template: String,
}

fn compile_rules(rules: &[OrganizeRule]) -> Result<Vec<CompiledRule>, DiffError> {
    rules
        .iter()
        .map(|r| {
            Ok(CompiledRule {
                filter: FileFilter::parse(r.filter.as_deref())?,
                template: r.template.clone(),
            })
        })
        .collect()
}

/// The target relative path a file would organize to, or `None` if no rule
/// matches, the template renders to nothing, or it already sits there.
///
/// Rules match on file identity only (no annotation lookup), so a `tag:`
/// condition in a rule filter never matches here — organize rules are a
/// separate feature from the annotation-aware tab wizard.
fn planned_target(rules: &[CompiledRule], rel_path: &str, entry: &FileEntry) -> Option<String> {
    let rule = rules.iter().find(|r| r.filter.matches(rel_path, entry))?;
    let target = normalize_rel(&render_template(&rule.template, entry, rel_path))?;
    (target != rel_path).then_some(target)
}

/// Plan the reorganization: the `(from, to)` relative-path pairs that would move
/// (identity paths and unmatched files excluded), in index order. The `to` is
/// the template target *before* any collision suffixing (which happens at apply
/// time); used for a preview.
pub fn plan_organize(
    store: &Store,
    repo: &str,
    rules: &[OrganizeRule],
) -> Result<Vec<(String, String)>, DiffError> {
    let compiled = compile_rules(rules)?;
    let db = store.open_repo_db(repo)?;
    let mut moves = Vec::new();
    store::for_each_file_entry(&db, |rel_path, entry: FileEntry| {
        if !entry.missing
            && let Some(target) = planned_target(&compiled, rel_path, &entry)
        {
            moves.push((rel_path.to_string(), target));
        }
        Ok(())
    })?;
    Ok(moves)
}

/// Apply the reorganization: move each planned file inside the repo, keeping the
/// index in sync. Collisions never overwrite — a target holding different
/// content is suffixed ` (2)`, ` (3)`…; a target already holding the same
/// content is skipped. Best effort: I/O errors are counted and the run
/// continues. Index writes are batched (with a final flush applied even on
/// cancel/failure). Per-file progress is reported through `run`.
pub fn organize_apply(
    store: &Store,
    repo: &str,
    rules: &[OrganizeRule],
    run: &DiffRun<'_>,
) -> Result<OrganizeStats, DiffError> {
    let plan = plan_organize(store, repo, rules)?;
    let db = store.open_repo_db(repo)?;
    let root = PathBuf::from(&store.get_repo(repo)?.abs_path);
    let total = plan.len() as u64;

    let mut stats = OrganizeStats::default();
    // Targets already taken this run, so two files never race for one path.
    let mut claimed: HashSet<String> = HashSet::new();
    // Buffered index changes: paths to drop and (path, entry) to add.
    let mut to_remove: Vec<String> = Vec::new();
    let mut to_add: Vec<(String, FileEntry)> = Vec::new();
    let mut since_flush = 0u64;

    let flush = |db: &redb::Database,
                 remove: &mut Vec<String>,
                 add: &mut Vec<(String, FileEntry)>|
     -> Result<(), StoreError> {
        if !add.is_empty() {
            store::apply_entries(db, add.iter().map(|(p, e)| (p.as_str(), e)))?;
            add.clear();
        }
        if !remove.is_empty() {
            store::remove_entries(db, remove.iter().map(String::as_str))?;
            remove.clear();
        }
        Ok(())
    };

    for (from_rel, target_rel) in &plan {
        if run.cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }
        // Re-read the current entry; skip if it vanished since planning.
        let Some(entry) = store::get_entry(&db, from_rel)? else {
            stats.skipped += 1;
            continue;
        };
        match resolve_target(&root, target_rel, from_rel, &entry, &claimed) {
            Resolution::Skip => stats.skipped += 1,
            Resolution::Move(final_rel) => {
                let from_abs = root.join(from_rel);
                let to_abs = root.join(&final_rel);
                if let Some(parent) = to_abs.parent()
                    && std::fs::create_dir_all(parent).is_err()
                {
                    stats.errors += 1;
                    continue;
                }
                if std::fs::rename(&from_abs, &to_abs).is_err() {
                    run.progress.on(DiffEvent::Error {
                        path: from_abs.to_string_lossy().into_owned(),
                        message: "could not move file".to_string(),
                    });
                    stats.errors += 1;
                    continue;
                }
                claimed.insert(final_rel.clone());
                to_remove.push(from_rel.clone());
                to_add.push((final_rel, entry));
                stats.moved += 1;
                since_flush += 1;
                run.progress.on(DiffEvent::Progress {
                    action: DiffAction::Move,
                    done: stats.moved,
                    total,
                    rel_path: from_rel.clone(),
                });
                if since_flush >= INDEX_BATCH {
                    flush(&db, &mut to_remove, &mut to_add)?;
                    since_flush = 0;
                }
            }
        }
    }

    // Reflect everything already moved on disk, even on cancel/failure.
    flush(&db, &mut to_remove, &mut to_add)?;
    Ok(stats)
}

/// Flush the organize index batch this often (mirrors the diff/scan pipelines).
const INDEX_BATCH: u64 = 200;

enum Resolution {
    /// Move the source to this final (possibly suffixed) relative path.
    Move(String),
    /// Leave the source where it is (target already holds this content).
    Skip,
}

/// Pick the final target path for a move, never overwriting: the plain target if
/// free, else ` (2)`, ` (3)`… before the extension. A candidate already holding
/// the *same* content (or already claimed this run) is treated as satisfied and
/// the move is skipped.
fn resolve_target(
    root: &Path,
    target_rel: &str,
    source_rel: &str,
    entry: &FileEntry,
    claimed: &HashSet<String>,
) -> Resolution {
    let (stem, ext) = split_stem_ext(target_rel);
    for n in 1..10_000 {
        let candidate = if n == 1 {
            target_rel.to_string()
        } else if ext.is_empty() {
            format!("{stem} ({n})")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        // A path already taken this run is not available.
        if claimed.contains(&candidate) {
            continue;
        }
        // Moving a file onto its own current path is a no-op.
        if candidate == source_rel {
            return Resolution::Skip;
        }
        let abs = root.join(&candidate);
        if !abs.exists() {
            return Resolution::Move(candidate);
        }
        // Occupied: same content means it's already organized; otherwise suffix.
        if is_same_content(&abs, entry.size, &entry.hash) {
            return Resolution::Skip;
        }
    }
    // Exhausted every candidate: never overwrite, so skip.
    Resolution::Skip
}

/// Split a relative path into its `(stem, ext)` where `ext` excludes the dot
/// (empty when the final component has no extension). Suffixes go before `ext`.
fn split_stem_ext(rel: &str) -> (String, String) {
    let path = Path::new(rel);
    match path.extension() {
        Some(ext) => {
            let ext = ext.to_string_lossy().into_owned();
            let stem = rel[..rel.len() - ext.len() - 1].to_string();
            (stem, ext)
        }
        None => (rel.to_string(), String::new()),
    }
}

/// Whether the file at `path` has exactly this size and BLAKE3 hash.
fn is_same_content(path: &Path, size: u64, hash: &[u8; 32]) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() != size {
        return false;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    if hasher.update_reader(file).is_err() {
        return false;
    }
    hasher.finalize().as_bytes() == hash
}

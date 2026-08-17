//! Register the app in the desktop menu on Linux.
//!
//! A brew or tarball install (and a downloaded AppImage) is just a binary —
//! nothing writes the `.desktop` entry or the icon, so the app never appears
//! in the application menu, and on Wayland the window shows a generic icon
//! (the compositor resolves the icon by matching the window's `app_id` to an
//! installed `dedup.desktop`). The GUI closes that gap itself: on startup it
//! writes/refreshes the icon and launcher in the *user's* XDG data dirs,
//! exactly what `packaging/install-icon.sh` does for a repo checkout.
//!
//! Rules: only files carrying our `X-Dedup-Managed` marker are ever
//! overwritten (a hand-written or distro-installed launcher is left alone),
//! nothing is written when the content is already current, and every failure
//! is swallowed by the caller — a missing menu entry is a cosmetic gap, never
//! worth failing startup over.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The embedded app icon, byte-identical to `packaging/`'s source of truth.
const ICON: &[u8] = include_bytes!("../assets/icon.png");

/// Marker key claiming a desktop file as ours to update.
const MARKER: &str = "X-Dedup-Managed=true";

/// Register icon + launcher for the running binary; call from a background
/// thread. Best-effort: errors (and non-Linux-style environments without a
/// home) are ignored.
pub fn register() {
    let Some(data_home) = data_home() else {
        return;
    };
    let Some(exec) = exec_path() else {
        return;
    };
    if let Ok(true) = register_at(&data_home, &exec) {
        refresh_caches(&data_home);
    }
}

/// `$XDG_DATA_HOME`, else `$HOME/.local/share`.
fn data_home() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME")
        && !x.is_empty()
    {
        return Some(PathBuf::from(x));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
}

/// What the launcher should execute: the AppImage file itself when running
/// from one (`$APPIMAGE` — the mount point the binary actually runs from is
/// gone after exit), otherwise the running executable — via the stable brew
/// symlink when the binary lives in a Homebrew Cellar.
fn exec_path() -> Option<PathBuf> {
    if let Some(ai) = std::env::var_os("APPIMAGE")
        && !ai.is_empty()
    {
        return Some(PathBuf::from(ai));
    }
    let exe = std::env::current_exe().ok()?;
    Some(stable_brew_path(&exe).unwrap_or(exe))
}

/// The stable `<prefix>/bin/<name>` symlink for a binary running out of a
/// Homebrew Cellar, verified to point back at that binary; `None` for
/// everything else. `current_exe` resolves to the *versioned* Cellar
/// directory (`…/Cellar/dedup/0.3.0/bin/dedup`), which the next
/// `brew upgrade` deletes — a launcher pinned there dies with it, and a dead
/// launcher can never self-heal, because the app it would start is gone.
/// Brew repoints `<prefix>/bin` on every upgrade, so that path stays alive.
fn stable_brew_path(exe: &Path) -> Option<PathBuf> {
    let prefix = exe
        .ancestors()
        .find(|a| a.file_name() == Some("Cellar".as_ref()))?
        .parent()?;
    let candidate = prefix.join("bin").join(exe.file_name()?);
    (fs::canonicalize(&candidate).ok()? == fs::canonicalize(exe).ok()?).then_some(candidate)
}

/// Write icon and launcher under `data_home` for `exec`. Returns whether
/// anything was written (callers refresh desktop caches only then).
fn register_at(data_home: &Path, exec: &Path) -> io::Result<bool> {
    let mut wrote = false;

    let icon_path = data_home.join("icons/hicolor/256x256/apps/dedup.png");
    if fs::read(&icon_path).ok().as_deref() != Some(ICON) {
        write_atomic(&icon_path, ICON)?;
        wrote = true;
    }

    let desktop_path = data_home.join("applications/dedup.desktop");
    let desired = desktop_entry(exec);
    match fs::read_to_string(&desktop_path) {
        Ok(current) if !current.contains(MARKER) => {} // not ours — never touch
        Ok(current) if current == desired => {}        // already current
        _ => {
            write_atomic(&desktop_path, desired.as_bytes())?;
            wrote = true;
        }
    }
    Ok(wrote)
}

/// The launcher content, mirroring `packaging/dedup.desktop` with the Exec
/// (and TryExec, so a moved-away AppImage hides instead of erroring) pointed
/// at the actual binary.
fn desktop_entry(exec: &Path) -> String {
    let exec_str = exec.to_string_lossy();
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=dedup\n\
         GenericName=Deduplication Triage\n\
         Comment=Manage your (media) files\n\
         TryExec={exec_str}\n\
         Exec={}\n\
         Icon=dedup\n\
         Terminal=false\n\
         Categories=Utility;FileTools;\n\
         StartupWMClass=dedup\n\
         {MARKER}\n",
        quote_exec(&exec_str),
    )
}

/// Quote a path for a desktop-file `Exec=` line: double-quoted, with the
/// spec's reserved characters backslash-escaped. A plain path without spaces
/// or specials stays unquoted for readability.
fn quote_exec(path: &str) -> String {
    let plain = path
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':' | '@'));
    if plain {
        return path.to_owned();
    }
    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    for c in path.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Create parent dirs and write via a same-directory temp file + rename, so a
/// crash can't leave a half-written launcher behind.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    io::Write::write_all(&mut tmp, bytes)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Poke the desktop environment's caches so the new entry shows up without a
/// re-login. Every tool is optional; absence and failure are both fine (GNOME
/// and KDE watch these directories anyway, sooner or later).
fn refresh_caches(data_home: &Path) {
    let run = |cmd: &str, args: &[&std::ffi::OsStr]| {
        let _ = std::process::Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    };
    let icons = data_home.join("icons/hicolor");
    let apps = data_home.join("applications");
    run(
        "gtk-update-icon-cache",
        &["-f".as_ref(), "-t".as_ref(), icons.as_os_str()],
    );
    run("update-desktop-database", &[apps.as_os_str()]);
    run("kbuildsycoca6", &[]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_register_writes_icon_and_marked_launcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = Path::new("/opt/brew/bin/dedup");
        assert!(register_at(dir.path(), exec).expect("register"));
        let icon = fs::read(dir.path().join("icons/hicolor/256x256/apps/dedup.png"))
            .expect("icon written");
        assert_eq!(icon, ICON);
        let desktop = fs::read_to_string(dir.path().join("applications/dedup.desktop"))
            .expect("desktop written");
        assert!(desktop.contains("Exec=/opt/brew/bin/dedup"));
        assert!(desktop.contains("TryExec=/opt/brew/bin/dedup"));
        assert!(desktop.contains(MARKER));
        assert!(desktop.contains("StartupWMClass=dedup"));
    }

    #[test]
    fn second_register_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = Path::new("/usr/local/bin/dedup");
        assert!(register_at(dir.path(), exec).expect("first"));
        assert!(
            !register_at(dir.path(), exec).expect("second"),
            "identical content is not rewritten"
        );
    }

    #[test]
    fn moved_binary_updates_the_launcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(register_at(dir.path(), Path::new("/old/dedup")).expect("first"));
        assert!(register_at(dir.path(), Path::new("/new/dedup")).expect("moved"));
        let desktop = fs::read_to_string(dir.path().join("applications/dedup.desktop"))
            .expect("desktop present");
        assert!(desktop.contains("Exec=/new/dedup"));
    }

    #[test]
    fn foreign_launcher_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let desktop_path = dir.path().join("applications/dedup.desktop");
        fs::create_dir_all(desktop_path.parent().expect("parent")).expect("mkdir");
        let foreign = "[Desktop Entry]\nName=my dedup\nExec=/home/me/custom\n";
        fs::write(&desktop_path, foreign).expect("seed");
        register_at(dir.path(), Path::new("/opt/dedup")).expect("register");
        assert_eq!(
            fs::read_to_string(&desktop_path).expect("still there"),
            foreign,
            "a launcher without our marker is never overwritten"
        );
    }

    /// A binary in a versioned Cellar directory registers the brew prefix's
    /// stable `bin` symlink — the one path a `brew upgrade` keeps alive — and
    /// anything else registers as itself.
    #[test]
    fn brew_cellar_binary_registers_the_stable_bin_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cellar_bin = dir.path().join("Cellar/dedup/0.3.0/bin");
        fs::create_dir_all(&cellar_bin).expect("mkdir cellar");
        let exe = cellar_bin.join("dedup");
        fs::write(&exe, b"binary").expect("write exe");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("mkdir bin");
        // Brew's relative link: bin/dedup -> ../Cellar/dedup/0.3.0/bin/dedup
        std::os::unix::fs::symlink("../Cellar/dedup/0.3.0/bin/dedup", bin.join("dedup"))
            .expect("symlink");
        assert_eq!(
            stable_brew_path(&exe),
            Some(bin.join("dedup")),
            "the launcher points at the symlink brew repoints on upgrade"
        );

        // A prefix whose bin symlink names a *different* binary is not ours.
        let foreign = dir.path().join("Cellar/other/1.0/bin");
        fs::create_dir_all(&foreign).expect("mkdir foreign");
        fs::write(foreign.join("dedup"), b"other").expect("write foreign");
        assert_eq!(
            stable_brew_path(&foreign.join("dedup")),
            None,
            "a bin symlink pointing elsewhere is not claimed"
        );

        // No Cellar in the path: register the binary itself.
        assert_eq!(stable_brew_path(Path::new("/usr/local/bin/dedup")), None);
    }

    #[test]
    fn exec_paths_with_specials_are_quoted() {
        assert_eq!(quote_exec("/opt/brew/bin/dedup"), "/opt/brew/bin/dedup");
        assert_eq!(
            quote_exec("/home/me/My Apps/dedup.AppImage"),
            "\"/home/me/My Apps/dedup.AppImage\""
        );
        assert_eq!(quote_exec("/tmp/a\"b"), "\"/tmp/a\\\"b\"");
        assert_eq!(quote_exec("/tmp/$HOME"), "\"/tmp/\\$HOME\"");
    }
}

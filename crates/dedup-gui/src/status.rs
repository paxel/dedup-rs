//! Repository location + reachability classification (Linux).
//!
//! Two questions collapsed into one enum: does the repo live on a network
//! mount, and is its folder reachable right now? The reachability probe runs on
//! a throwaway thread with a timeout, because a dead NFS mount makes `stat()`
//! block indefinitely — we must never let that hang the UI.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

/// Where a repo lives and whether it can be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// Local disk, reachable.
    Local,
    /// Network mount, reachable.
    Remote,
    /// Network mount, not currently reachable.
    Offline,
    /// Local path that is no longer accessible.
    Missing,
}

impl Location {
    /// Whether the folder can currently be walked. Scans/checks are pointless
    /// (and may hang) when it can't.
    pub fn reachable(self) -> bool {
        matches!(self, Location::Local | Location::Remote)
    }
}

/// Time budget for the reachability probe before we declare the mount down.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Classify a repo path: is it on a network filesystem, and is it reachable?
pub fn classify(path: &str) -> Location {
    match (is_network_mount(path), is_reachable(path)) {
        (true, true) => Location::Remote,
        (true, false) => Location::Offline,
        (false, true) => Location::Local,
        (false, false) => Location::Missing,
    }
}

/// Probe whether `path` is a readable directory, giving up after
/// [`PROBE_TIMEOUT`]. The probe runs on a detached thread: if the mount is dead
/// the `stat` blocks forever, so we abandon the thread rather than wait on it.
fn is_reachable(path: &str) -> bool {
    let (tx, rx) = mpsc::channel();
    let owned = path.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(Path::new(&owned).is_dir());
    });
    matches!(rx.recv_timeout(PROBE_TIMEOUT), Ok(true))
}

/// Filesystem types served over the network.
fn is_network_fs(fstype: &str) -> bool {
    matches!(
        fstype,
        "nfs"
            | "nfs4"
            | "cifs"
            | "smb3"
            | "smbfs"
            | "9p"
            | "ceph"
            | "afs"
            | "ncpfs"
            | "glusterfs"
            | "fuse.glusterfs"
            | "sshfs"
            | "fuse.sshfs"
            | "fuse.rclone"
            | "davfs"
            | "fuse.davfs2"
    )
}

/// Find the filesystem type of the mount that contains `path` by reading
/// `/proc/self/mountinfo`, and report whether it is a network filesystem.
/// Returns `false` if mountinfo is unavailable (e.g. non-Linux).
fn is_network_mount(path: &str) -> bool {
    let Ok(content) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    mountinfo_is_network(&content, path)
}

/// Pure core of [`is_network_mount`], split out for testing: pick the mount
/// whose mount point is the longest prefix of `path` and classify its fstype.
fn mountinfo_is_network(content: &str, path: &str) -> bool {
    let mut best: Option<(usize, bool)> = None;
    for line in content.lines() {
        // `<...fields...> - <fstype> <source> <superopts>`; the mount point is
        // field index 4 in the part before the " - " separator.
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let Some(mount_point) = left.split_whitespace().nth(4).map(unescape_octal) else {
            continue;
        };
        let Some(fstype) = right.split_whitespace().next() else {
            continue;
        };
        if path_has_prefix(path, &mount_point) {
            let len = mount_point.len();
            if best.is_none_or(|(best_len, _)| len > best_len) {
                best = Some((len, is_network_fs(fstype)));
            }
        }
    }
    best.map(|(_, network)| network).unwrap_or(false)
}

/// True if `mount_point` is `path` itself or an ancestor directory of it.
fn path_has_prefix(path: &str, mount_point: &str) -> bool {
    let path = path.trim_end_matches('/');
    let mp = mount_point.trim_end_matches('/');
    if mp.is_empty() {
        return true; // root "/" contains everything
    }
    path == mp
        || path
            .strip_prefix(mp)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// mountinfo escapes space, tab, newline and backslash as octal (`\040` etc).
fn unescape_octal(field: &str) -> String {
    if !field.contains('\\') {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len());
    let bytes = field.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let oct = &field[i + 1..i + 4];
            if let Ok(code) = u8::from_str_radix(oct, 8) {
                out.push(code as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
21 30 0:20 / /proc rw - proc proc rw
30 0 8:2 / / rw - ext4 /dev/sda2 rw
40 30 0:40 / /mnt/nas rw - nfs4 server:/export rw
41 30 0:41 / /mnt/usb rw - vfat /dev/sdb1 rw
42 40 0:42 / /mnt/nas/deep rw - cifs //srv/share rw";

    #[test]
    fn local_path_is_not_network() {
        assert!(!mountinfo_is_network(MOUNTINFO, "/home/user/photos"));
    }

    #[test]
    fn network_mount_is_detected() {
        assert!(mountinfo_is_network(MOUNTINFO, "/mnt/nas/albums"));
    }

    #[test]
    fn longest_prefix_wins() {
        // /mnt/nas/deep is cifs even though /mnt/nas (nfs4) is also a prefix.
        assert!(mountinfo_is_network(MOUNTINFO, "/mnt/nas/deep/x"));
        // A vfat USB stick is not network.
        assert!(!mountinfo_is_network(MOUNTINFO, "/mnt/usb/dcim"));
    }

    #[test]
    fn octal_escapes_are_decoded() {
        assert_eq!(unescape_octal(r"/mnt/my\040nas"), "/mnt/my nas");
        assert_eq!(unescape_octal("/plain/path"), "/plain/path");
    }

    #[test]
    fn prefix_matching_respects_boundaries() {
        assert!(path_has_prefix("/mnt/nas/x", "/mnt/nas"));
        assert!(path_has_prefix("/mnt/nas", "/mnt/nas"));
        // Not a real ancestor: /mnt/nastier must not match /mnt/nas.
        assert!(!path_has_prefix("/mnt/nastier/x", "/mnt/nas"));
        assert!(path_has_prefix("/anything", "/"));
    }
}

//! Running the external tools that parse untrusted files (ffmpeg, ffprobe,
//! poppler) so a crafted file cannot stall a scan or steer the tool
//! (docs/security/threat-model.md, threats 3 and 4): every run has a deadline
//! and an output cap and is killed past either, every path handed over is
//! absolute so a name starting with `-` never reads as an option, and ffmpeg
//! and ffprobe may open local files and pipes only, never a network URL a
//! playlist inside the file names.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long ffprobe may take to read a file's headers.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(30);
/// How long a single decode (one video frame, a PDF page, the audio a
/// fingerprint needs) may take.
pub const DECODE_DEADLINE: Duration = Duration::from_secs(60);
/// How long extracting a whole audio track to a WAV may take.
pub const EXTRACT_DEADLINE: Duration = Duration::from_secs(600);
/// The most stdout a run may produce before it is killed.
pub const OUTPUT_CAP: u64 = 256 * 1024 * 1024;

/// The ffmpeg / ffprobe input options that confine a run to local files and
/// pipes. They go before `-i` (ffmpeg) or the input (ffprobe).
pub const LOCAL_ONLY: [&str; 2] = ["-protocol_whitelist", "file,pipe"];

/// What a finished run produced. stderr is discarded.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

/// Runs `cmd` to completion within `deadline`, collecting at most
/// [`OUTPUT_CAP`] bytes of stdout. `None` when it cannot start, overruns the
/// deadline or the cap (it is killed then), or its output cannot be read.
pub fn run(cmd: &mut Command, deadline: Duration) -> Option<Output> {
    run_capped(cmd, deadline, OUTPUT_CAP)
}

fn run_capped(cmd: &mut Command, deadline: Duration, cap: u64) -> Option<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drained on its own thread, so a chatty tool never blocks on a full pipe
    // while this one waits; one byte past the cap marks an overrun.
    let reader = child.stdout.take().map(|stdout| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let read = stdout.take(cap.saturating_add(1)).read_to_end(&mut out);
            read.ok().map(|_| out)
        })
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => break None,
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let stdout = match reader {
        Some(handle) => handle.join().ok()??,
        None => Vec::new(),
    };
    (stdout.len() as u64 <= cap).then_some(Output { status, stdout })
}

/// `path` as an absolute path, so a file named `-x` reaches a tool as a file.
/// Falls back to a `./`-prefixed relative path when the current directory is
/// unknown.
pub fn path_arg(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| Path::new(".").join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_run_collects_stdout_and_status() {
        let out = run(
            Command::new("sh").args(["-c", "printf hello"]),
            DECODE_DEADLINE,
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello");
    }

    #[cfg(unix)]
    #[test]
    fn a_run_past_its_deadline_is_killed() {
        let started = Instant::now();
        let out = run(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(200),
        );
        assert!(out.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "killed, not awaited"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_run_past_its_output_cap_is_refused() {
        let script = "head -c 2000 /dev/zero";
        let over = run_capped(
            Command::new("sh").args(["-c", script]),
            DECODE_DEADLINE,
            1000,
        );
        assert!(over.is_none());
        let fits = run_capped(
            Command::new("sh").args(["-c", script]),
            DECODE_DEADLINE,
            2000,
        );
        assert_eq!(fits.map(|o| o.stdout.len()), Some(2000));
    }

    /// An endless writer is stopped by the cap even before its deadline.
    #[cfg(unix)]
    #[test]
    fn an_endless_writer_is_stopped() {
        let started = Instant::now();
        let out = run_capped(
            Command::new("sh").args(["-c", "cat /dev/zero"]),
            Duration::from_secs(5),
            1000,
        );
        assert!(out.is_none());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A playlist inside a scanned file names a network URL; ffprobe never
    /// connects to it. Current ffmpeg already refuses a segment on another
    /// protocol than its playlist; the local-only whitelist keeps that true
    /// for builds and demuxers that do not.
    #[test]
    fn ffprobe_never_follows_a_playlist_onto_the_network() {
        if !crate::fingerprint::ffmpeg_available() {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("holiday.m3u8");
        std::fs::write(
            &fake,
            format!("#EXTM3U\n#EXTINF:10,\nhttp://127.0.0.1:{port}/segment.ts\n#EXT-X-ENDLIST\n"),
        )
        .unwrap();
        assert_eq!(crate::fingerprint::media_duration_secs(&fake), None);
        assert!(
            listener.accept().is_err(),
            "ffprobe connected to the URL inside the file"
        );
    }

    #[test]
    fn a_dash_name_becomes_an_absolute_path() {
        let arg = path_arg(Path::new("-v.pdf"));
        assert!(arg.is_absolute(), "{arg:?}");
        assert!(arg.ends_with("-v.pdf"));
    }
}

//! One writer at a time, across processes.
//!
//! The app and the scheduled job are separate processes pointing at the same
//! SQLite file. Nothing stopped them running together, and when they did both
//! stalled: each held part of what the other needed and both sat at zero CPU
//! until one was killed. That is not a rare race — the nightly job fires at
//! 19:30 whether or not the window is open, so it is the normal case.
//!
//! SQLite's own busy handling does not solve it: it serialises *statements*,
//! while a backfill or a scan is thousands of transactions that together mean
//! "I am rewriting the database". This lock expresses that larger unit.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Off Windows a lock file's age is all there is to go on: one nobody has
/// refreshed for this long is assumed to belong to a process that died. Every
/// write operation in the app finishes well inside it. (On Windows the holder
/// keeps the file open, so a file that opens has no owner and age never comes
/// into it; see `WriteLock`.)
///
/// All the binaries in a folder must be built together. A holder from the build
/// before this one writes the file and closes it at once, which a new build
/// cannot tell from the leftover of a dead process.
const STALE_AFTER: Duration = Duration::from_secs(30 * 60);

/// How long "access denied" is waited out. It is what a file looks like in the
/// instant the previous holder's delete is being processed, but a lock file that
/// is read-only or otherwise unwritable would look the same for ever, and should
/// be reported rather than waited on for the whole of `wait`.
const DENIED_PATIENCE: Duration = Duration::from_secs(10);

/// Windows' "another process has the file open without sharing".
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;

fn lock_path() -> PathBuf {
    crate::config::data_dir().join("writer.lock")
}

/// Held for as long as one process is the writer. Released on drop, including
/// on a normal panic unwind.
///
/// On Windows the holder keeps the file open with no sharing. The OS closes that
/// handle when the process ends however it ends — killed, crashed, closed
/// mid-download, power cut — so the next copy can tell a lock that is held from
/// one that was left behind, and takes the latter over at once instead of
/// refusing for half an hour.
#[derive(Debug)]
pub struct WriteLock {
    path: PathBuf,
    file: Option<File>,
}

impl WriteLock {
    /// Become the writer, waiting up to `wait` for the current one to finish.
    ///
    /// `Ok(None)` means someone else still holds it — a caller that cannot
    /// proceed should say so and stop, not force its way in.
    pub fn acquire(wait: Duration, holder: &str) -> Result<Option<Self>> {
        Self::acquire_at(lock_path(), wait, holder)
    }

    /// The path is a parameter so tests get their own lock file. Sharing the
    /// real one would make them fight each other under the default parallel
    /// test runner — exactly the bug this type exists to prevent.
    fn acquire_at(path: PathBuf, wait: Duration, holder: &str) -> Result<Option<Self>> {
        let deadline = std::time::Instant::now() + wait;
        let mut denied_since: Option<std::time::Instant> = None;

        loop {
            match Self::open(&path) {
                Ok(mut file) => {
                    // Purely diagnostic: if a lock ever needs explaining, the
                    // file says who holds it.
                    let _ = writeln!(file, "{holder} pid={}", std::process::id());
                    return Ok(Some(Self { path, file: Some(file) }));
                }
                Err(e) => match Self::classify(&e) {
                    // Open elsewhere right now: a live holder, whatever the
                    // file's age says.
                    Held::Alive => denied_since = None,
                    // Present, with nothing to say whether anyone has it.
                    Held::Unknown => {
                        denied_since = None;
                        if Self::is_stale(&path) && std::fs::remove_file(&path).is_ok() {
                            continue;
                        }
                    }
                    // Mid-delete by the previous holder, so the next try should
                    // succeed; if it keeps being denied, the file is the problem.
                    Held::Vanishing => {
                        let since = *denied_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() > DENIED_PATIENCE {
                            return Err(e).with_context(|| format!("opening {}", path.display()));
                        }
                    }
                    Held::Not => {
                        return Err(e).with_context(|| format!("creating {}", path.display()));
                    }
                },
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    #[cfg(windows)]
    fn open(path: &PathBuf) -> std::io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        // share_mode(0): while this handle exists nobody else can open the file,
        // and nobody can delete it from under us.
        OpenOptions::new().write(true).create(true).truncate(true).share_mode(0).open(path)
    }

    #[cfg(not(windows))]
    fn open(path: &PathBuf) -> std::io::Result<File> {
        OpenOptions::new().write(true).create_new(true).open(path)
    }

    fn classify(e: &std::io::Error) -> Held {
        match (e.kind(), e.raw_os_error()) {
            (_, Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)) => Held::Alive,
            (std::io::ErrorKind::AlreadyExists, _) => Held::Unknown,
            (std::io::ErrorKind::PermissionDenied, _) => Held::Vanishing,
            _ => Held::Not,
        }
    }

    fn is_stale(path: &PathBuf) -> bool {
        let Ok(meta) = std::fs::metadata(path) else { return true };
        let Ok(modified) = meta.modified() else { return false };
        SystemTime::now()
            .duration_since(modified)
            .map(|age| age > STALE_AFTER)
            .unwrap_or(false)
    }
}

/// What a failed attempt to take the lock tells us.
enum Held {
    Alive,
    Unknown,
    Vanishing,
    Not,
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        // Close first: with no sharing, an open handle also blocks the delete.
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own lock file, named after itself.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("spider-writelock-{name}.lock"));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn take(path: &PathBuf, holder: &str) -> Option<WriteLock> {
        WriteLock::acquire_at(path.clone(), Duration::from_millis(0), holder).unwrap()
    }

    #[test]
    fn a_second_acquire_fails_while_the_first_is_held() {
        let path = scratch("held");
        let first = take(&path, "a").expect("first acquire should succeed");
        assert!(take(&path, "b").is_none(), "two writers must never be granted at once");

        drop(first);
        assert!(take(&path, "c").is_some(), "the lock must be released on drop");
    }

    #[test]
    fn a_stale_lock_is_reclaimed() {
        let path = scratch("stale");
        std::fs::write(&path, "a process that died pid=1").unwrap();
        // Backdate it past the staleness window.
        let old = SystemTime::now() - STALE_AFTER - Duration::from_secs(60);
        OpenOptions::new().write(true).open(&path).unwrap().set_modified(old).unwrap();

        assert!(
            take(&path, "d").is_some(),
            "a lock left behind by a crash must not block the app forever"
        );
    }

    #[test]
    fn a_fresh_lock_is_not_treated_as_stale() {
        let path = scratch("fresh");
        let _held = take(&path, "e").unwrap();
        assert!(!WriteLock::is_stale(&path), "a lock taken moments ago is alive");
    }

    #[test]
    fn waiting_gives_up_and_reports_rather_than_forcing() {
        let path = scratch("wait");
        let _held = take(&path, "f").unwrap();
        let start = std::time::Instant::now();
        let result = WriteLock::acquire_at(path.clone(), Duration::from_millis(900), "g").unwrap();
        assert!(result.is_none(), "it must report failure, never seize a held lock");
        assert!(start.elapsed() >= Duration::from_millis(800), "it should have waited");
    }

    /// The reason the handle is held open: a copy that was closed or killed in
    /// the middle of a download leaves its lock file behind, and the next one
    /// must not sit out the staleness window because of it.
    #[cfg(windows)]
    #[test]
    fn a_lock_file_left_by_a_dead_process_is_taken_over_at_once() {
        let path = scratch("dead");
        // Fresh, so the age rule alone would call it live; nothing has it open.
        std::fs::write(&path, "desktop app pid=999999").unwrap();
        assert!(take(&path, "h").is_some(), "an unowned lock file must not block the next run");
    }
}

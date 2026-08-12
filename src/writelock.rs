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
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// A lock left behind by a crash is assumed dead after this long. Every write
/// operation in the app finishes well inside it.
const STALE_AFTER: Duration = Duration::from_secs(30 * 60);

fn lock_path() -> PathBuf {
    crate::config::data_dir().join("writer.lock")
}

/// Held for as long as one process is the writer. Released on drop, including
/// on a normal panic unwind.
#[derive(Debug)]
pub struct WriteLock {
    path: PathBuf,
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

        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    // Purely diagnostic: if a stale lock ever needs explaining,
                    // the file says who left it and when.
                    let _ = writeln!(file, "{holder} pid={}", std::process::id());
                    return Ok(Some(Self { path }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::is_stale(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("creating {}", path.display()));
                }
            }
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

impl Drop for WriteLock {
    fn drop(&mut self) {
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
}

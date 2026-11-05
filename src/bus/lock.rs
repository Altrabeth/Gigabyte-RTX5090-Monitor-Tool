//! The exclusive file lock that keeps the daemon and uploads from interleaving
//! on the bus.
//!
//! Take it around the bus access and nothing else: [`BusLock`] is a guard, so
//! the lock lasts as long as the value does. The daemon holds one per write
//! and skips the tick on [`Error::LockBusy`]; an upload holds one throughout.
//!
//! Dry-run must not call into this module. It touches no bus, so it has nothing
//! to exclude, and it has to work where the lock file was never installed.
//!
//! There is one lock path and no fallback. Two processes on different paths
//! would both believe they own the bus, so a path that cannot be opened is
//! [`Error::Lock`], never a reason to try another one.

use std::env;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{flock, FlockOperation};
use rustix::io::Errno;

use crate::error::{Error, Result};
use crate::panel::Sleep;

/// The installed lock file. Non-root users cannot create it, so
/// `dist/aorus-lcd.tmpfiles` does.
const DEFAULT_LOCK_PATH: &str = "/run/aorus-lcd.lock";

/// Names an environment variable that replaces the installed path, for tests
/// and for manual verification before the tmpfiles entry is installed. Every
/// process sharing the bus must see the same value, or they will not exclude
/// each other.
pub const LOCK_PATH_ENV: &str = "AORUS_LCD_LOCK";

/// How often a waiting [`BusLock::acquire`] retries. A retry is one syscall, and
/// the wait is at most this much longer than the holder's hold.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The lock path to use: [`LOCK_PATH_ENV`] if it is set, otherwise the installed
/// file.
pub fn lock_path() -> PathBuf {
    resolve_path(env::var_os(LOCK_PATH_ENV))
}

/// An empty value is kept as it is, so that it fails to open. Treating it as
/// unset would quietly move the process to a different lock from the one the
/// caller configured.
fn resolve_path(configured: Option<OsString>) -> PathBuf {
    configured.map_or_else(|| PathBuf::from(DEFAULT_LOCK_PATH), PathBuf::from)
}

/// Exclusive ownership of the bus, released when dropped or when the process
/// dies, however it dies.
///
/// `flock` belongs to the open file, so two `BusLock`s exclude each other even
/// within one process.
#[derive(Debug)]
#[must_use = "the bus lock is released as soon as this guard is dropped"]
pub struct BusLock {
    file: File,
}

impl BusLock {
    /// Takes the lock at `path` without waiting.
    ///
    /// # Errors
    ///
    /// [`Error::LockBusy`] if another holder has it, [`Error::Lock`] if the file
    /// cannot be created, opened or locked.
    pub fn try_acquire(path: &Path) -> Result<Self> {
        let file = open_lock_file(path)?;
        if try_flock(&file, path)? {
            Ok(Self { file })
        } else {
            Err(busy(path))
        }
    }

    /// Takes the lock at `path`, waiting up to `timeout` for it through `sleep`.
    ///
    /// The wait is counted in requested sleeps rather than read from a clock, so
    /// a test double's sleeps use up the timeout without any real delay. Time
    /// spent oversleeping is not counted; on this scale it does not matter.
    ///
    /// `flock` has no timeout of its own and a blocking call can only be broken
    /// by a signal, which would need a process-wide alarm alongside the daemon's
    /// handlers. So this polls without blocking.
    ///
    /// # Errors
    ///
    /// [`Error::LockBusy`] if the lock was still held when `timeout` ran out,
    /// [`Error::Lock`] if the file cannot be created, opened or locked.
    pub fn acquire(path: &Path, timeout: Duration, sleep: &mut dyn Sleep) -> Result<Self> {
        let file = open_lock_file(path)?;
        let mut waited = Duration::ZERO;
        loop {
            if try_flock(&file, path)? {
                return Ok(Self { file });
            }
            if waited >= timeout {
                return Err(busy(path));
            }
            let nap = POLL_INTERVAL.min(timeout - waited);
            sleep.sleep(nap);
            waited += nap;
        }
    }
}

impl Drop for BusLock {
    /// Unlocks explicitly rather than relying on the close. A child being spawned
    /// on another thread briefly shares this open file until its `exec`, and a
    /// close alone would leave the lock held for that long. An error here changes
    /// nothing, because closing the file follows immediately.
    fn drop(&mut self) {
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

/// Opens the lock file, creating it if the directory allows that, which is what
/// makes a [`LOCK_PATH_ENV`] path work without the tmpfiles entry.
///
/// The file only ever serves as a lock: its contents are left alone and it is
/// never removed, because a process locking a deleted file excludes nobody.
/// `std` opens it close-on-exec, so spawned commands do not inherit the lock.
fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| lock_error(path, source))
}

/// Returns whether the lock was taken. Only "someone else holds it" is `false`:
/// any other failure must stay an error, or the daemon would skip ticks forever
/// on a lock that can never work.
fn try_flock(file: &File, path: &Path) -> Result<bool> {
    match flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(Errno::WOULDBLOCK) => Ok(false),
        Err(errno) => Err(lock_error(path, errno.into())),
    }
}

fn lock_error(path: &Path, source: std::io::Error) -> Error {
    Error::Lock {
        path: path.to_owned(),
        source,
    }
}

fn busy(path: &Path) -> Error {
    Error::LockBusy {
        path: path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::{RecordingSleep, ThreadSleep};
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;
    use tempfile::TempDir;

    /// How [`killing_the_holder_process_releases_the_lock`] re-executes this
    /// test binary as a second process.
    const HOLDER_TEST: &str = "bus::lock::tests::holder_process";

    /// What [`holder_process`] prints once it owns the lock.
    const HELD_MARKER: &str = "holder owns the lock";

    const SIGKILL: i32 = 9;

    /// A bound for waits that end as soon as the lock is free. It is only ever
    /// reached by a failing test, so it can be generous.
    const GENEROUS: Duration = Duration::from_secs(30);

    fn temp_lock() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.lock");
        (dir, path)
    }

    fn assert_busy(result: Result<BusLock>, expected: &Path) {
        match result {
            Err(Error::LockBusy { path }) => assert_eq!(path, expected),
            other => panic!("expected LockBusy, got {other:?}"),
        }
    }

    /// Releases the lock it holds during the `naps`th sleep, which lets a test
    /// fix the moment of release relative to the waiter.
    struct ReleaseDuringSleep {
        holder: Option<BusLock>,
        naps: usize,
        taken: usize,
    }

    impl Sleep for ReleaseDuringSleep {
        fn sleep(&mut self, _duration: Duration) {
            self.taken += 1;
            if self.taken == self.naps {
                self.holder = None;
            }
        }
    }

    #[test]
    fn a_second_holder_is_refused_without_waiting() {
        let (_dir, path) = temp_lock();
        let _held = BusLock::try_acquire(&path).unwrap();

        assert_busy(BusLock::try_acquire(&path), &path);
    }

    #[test]
    fn dropping_the_guard_releases_the_lock() {
        let (_dir, path) = temp_lock();

        let held = BusLock::try_acquire(&path).unwrap();
        drop(held);

        drop(BusLock::try_acquire(&path).unwrap());
    }

    // Stands in for a child between fork and exec, which shares the open file.
    #[test]
    fn dropping_the_guard_releases_the_lock_while_the_file_is_still_shared() {
        let (_dir, path) = temp_lock();
        let held = BusLock::try_acquire(&path).unwrap();
        let _shared = held.file.try_clone().unwrap();

        drop(held);

        drop(BusLock::try_acquire(&path).unwrap());
    }

    #[test]
    fn a_free_lock_is_taken_without_sleeping() {
        let (_dir, path) = temp_lock();
        let mut sleep = RecordingSleep::default();

        drop(BusLock::acquire(&path, GENEROUS, &mut sleep).unwrap());

        assert!(sleep.delays.is_empty());
    }

    #[test]
    fn a_waiter_times_out_having_slept_exactly_the_timeout() {
        let (_dir, path) = temp_lock();
        let _held = BusLock::try_acquire(&path).unwrap();
        let mut sleep = RecordingSleep::default();
        let timeout = POLL_INTERVAL * 2 + Duration::from_millis(3);

        assert_busy(BusLock::acquire(&path, timeout, &mut sleep), &path);

        assert_eq!(
            sleep.delays,
            [POLL_INTERVAL, POLL_INTERVAL, Duration::from_millis(3)]
        );
    }

    #[test]
    fn a_zero_timeout_never_sleeps() {
        let (_dir, path) = temp_lock();
        let _held = BusLock::try_acquire(&path).unwrap();
        let mut sleep = RecordingSleep::default();

        assert_busy(BusLock::acquire(&path, Duration::ZERO, &mut sleep), &path);

        assert!(sleep.delays.is_empty());
    }

    #[test]
    fn a_waiter_proceeds_once_the_holder_releases() {
        let (_dir, path) = temp_lock();
        let mut sleep = ReleaseDuringSleep {
            holder: Some(BusLock::try_acquire(&path).unwrap()),
            naps: 3,
            taken: 0,
        };

        drop(BusLock::acquire(&path, GENEROUS, &mut sleep).unwrap());

        assert_eq!(
            sleep.taken, 3,
            "it must wait until the release, and no more"
        );
    }

    #[test]
    fn a_waiter_sees_a_release_during_its_last_sleep() {
        let (_dir, path) = temp_lock();
        let mut sleep = ReleaseDuringSleep {
            holder: Some(BusLock::try_acquire(&path).unwrap()),
            naps: 1,
            taken: 0,
        };

        drop(BusLock::acquire(&path, POLL_INTERVAL, &mut sleep).unwrap());
    }

    #[test]
    fn the_real_clock_waits_out_the_timeout() {
        let (_dir, path) = temp_lock();
        let _held = BusLock::try_acquire(&path).unwrap();
        let timeout = Duration::from_millis(20);
        let start = Instant::now();

        assert_busy(BusLock::acquire(&path, timeout, &mut ThreadSleep), &path);

        assert!(start.elapsed() >= timeout);
    }

    /// Not a test of its own: the second process for
    /// [`killing_the_holder_process_releases_the_lock`]. It reads the path from
    /// the environment, as the binary does, and stays alive until its parent
    /// kills it or goes away, so it cannot outlive a failed run.
    #[test]
    #[ignore = "helper process, started by killing_the_holder_process_releases_the_lock"]
    fn holder_process() {
        if env::var_os(LOCK_PATH_ENV).is_none() {
            return;
        }
        let _held = BusLock::try_acquire(&lock_path()).unwrap();
        writeln!(io::stdout(), "{HELD_MARKER}").unwrap();
        io::stdin().read_to_end(&mut Vec::new()).unwrap();
    }

    #[test]
    fn killing_the_holder_process_releases_the_lock() {
        let (_dir, path) = temp_lock();
        let mut holder = Command::new(env::current_exe().unwrap())
            .args(["--exact", HOLDER_TEST, "--ignored", "--nocapture"])
            .env(LOCK_PATH_ENV, &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();

        let stdout = BufReader::new(holder.stdout.take().unwrap());
        let held = stdout
            .lines()
            .any(|line| line.unwrap().contains(HELD_MARKER));
        assert!(held, "the holder process exited without taking the lock");
        assert_busy(BusLock::try_acquire(&path), &path);

        holder.kill().unwrap();
        let status = holder.wait().unwrap();

        assert_eq!(status.signal(), Some(SIGKILL));
        drop(BusLock::acquire(&path, GENEROUS, &mut ThreadSleep).unwrap());
    }

    #[test]
    fn an_unopenable_path_fails_and_points_at_the_install() {
        let (dir, _) = temp_lock();
        let path = dir.path().join("missing").join("test.lock");

        let error = BusLock::try_acquire(&path).unwrap_err();

        assert!(error.to_string().contains("tmpfiles.d"), "{error}");
        assert!(error.to_string().contains("i2c group"), "{error}");
        assert!(error.to_string().contains("AORUS_LCD_LOCK"), "{error}");
        match error {
            Error::Lock {
                path: reported,
                source,
            } => {
                assert_eq!(reported, path);
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected Lock, got {other:?}"),
        }
    }

    #[test]
    fn a_waiter_does_not_wait_on_an_unopenable_path() {
        let (dir, _) = temp_lock();
        let path = dir.path().join("missing").join("test.lock");
        let mut sleep = RecordingSleep::default();

        let result = BusLock::acquire(&path, GENEROUS, &mut sleep);

        assert!(matches!(result, Err(Error::Lock { .. })), "{result:?}");
        assert!(sleep.delays.is_empty());
    }

    #[test]
    fn the_lock_file_is_never_truncated() {
        let (_dir, path) = temp_lock();
        std::fs::write(&path, "kept").unwrap();

        drop(BusLock::try_acquire(&path).unwrap());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
    }

    #[test]
    fn the_default_path_is_the_installed_file() {
        assert_eq!(resolve_path(None), Path::new("/run/aorus-lcd.lock"));
    }

    #[test]
    fn the_environment_replaces_the_path() {
        let path = resolve_path(Some("/somewhere/else.lock".into()));

        assert_eq!(path, Path::new("/somewhere/else.lock"));
    }

    #[test]
    fn an_empty_path_does_not_fall_back_to_the_default() {
        let path = resolve_path(Some(OsString::new()));

        assert!(matches!(
            BusLock::try_acquire(&path),
            Err(Error::Lock { .. })
        ));
    }
}

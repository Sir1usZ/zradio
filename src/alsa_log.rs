use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Soft cap for `alsa.log`. Past this, stderr is sent to `/dev/null`.
pub const STDERR_LOG_CAP: u64 = 8 * 1024 * 1024;

pub fn stderr_log_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zradio").join("alsa.log")
}

pub fn redirect_stderr_to_log() {
    if let Ok(guard) = install_capped_stderr(&stderr_log_path(), STDERR_LOG_CAP) {
        // Lives until process exit. Drop would steal stderr back to /dev/null.
        std::mem::forget(guard);
    }
}

/// Install a bounded stderr sink at `path`.
///
/// Writes go to the file until `cap` bytes. After that, stderr is
/// `dup2`'d onto `/dev/null` so alsa-lib C writes cannot grow the file.
pub fn install_capped_stderr(path: &Path, cap: u64) -> io::Result<CappedStderr> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)?;
    let start = file.metadata()?.len();
    if start >= cap {
        divert_stderr_to_null();
        return Ok(CappedStderr {
            stop: None,
            worker: None,
        });
    }

    let (mut read_end, write_end) = io::pipe()?;
    dup2_fd(write_end.as_raw_fd(), 2);
    drop(write_end);

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = thread::Builder::new()
        .name("alsa-log".into())
        .spawn(move || {
            pump_capped(&mut read_end, &mut file, cap, start, &worker_stop);
        })?;

    Ok(CappedStderr {
        stop: Some(stop),
        worker: Some(worker),
    })
}

pub struct CappedStderr {
    stop: Option<Arc<AtomicBool>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for CappedStderr {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
        }
        // Closing the pipe write end (fd 2) unblocks the worker.
        divert_stderr_to_null();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn pump_capped(
    read_end: &mut impl Read,
    file: &mut File,
    cap: u64,
    mut written: u64,
    stop: &AtomicBool,
) {
    let mut buf = [0u8; 8192];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match read_end.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = cap.saturating_sub(written);
                if remaining == 0 {
                    divert_stderr_to_null();
                    drain_rest(read_end, stop);
                    break;
                }
                let nwrite = (n as u64).min(remaining) as usize;
                if file.write_all(&buf[..nwrite]).is_err() {
                    divert_stderr_to_null();
                    drain_rest(read_end, stop);
                    break;
                }
                written += nwrite as u64;
                if written >= cap {
                    divert_stderr_to_null();
                    drain_rest(read_end, stop);
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                divert_stderr_to_null();
                break;
            }
        }
    }
}

fn drain_rest(read_end: &mut impl Read, stop: &AtomicBool) {
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::Relaxed) {
        match read_end.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

fn divert_stderr_to_null() {
    if let Ok(null) = OpenOptions::new().write(true).open("/dev/null") {
        dup2_fd(null.as_raw_fd(), 2);
    }
}

fn dup2_fd(old: i32, new: i32) {
    // SAFETY: `old` is an open fd; `new` is stderr (2) or a caller-chosen slot.
    let _ = unsafe { libc_dup2(old, new) };
}

extern "C" {
    #[link_name = "dup2"]
    fn libc_dup2(oldfd: i32, newfd: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};

    static STDERR_LOCK: Mutex<()> = Mutex::new(());
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_log() -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("zradio-alsa-log-{}-{n}.log", std::process::id()))
    }

    fn wait_size_at_least(path: &Path, min: u64, timeout: Duration) -> u64 {
        let start = Instant::now();
        loop {
            let len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if len >= min || start.elapsed() >= timeout {
                return len;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_size_stable(path: &Path, timeout: Duration) -> u64 {
        let start = Instant::now();
        let mut last = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        while start.elapsed() < timeout {
            thread::sleep(Duration::from_millis(20));
            let now = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if now == last {
                return now;
            }
            last = now;
        }
        last
    }

    fn write_stderr(bytes: &[u8]) {
        // Direct write to fd 2, matching alsa-lib / eprintln! after dup2.
        let mut stderr = unsafe { File::from_raw_fd(2) };
        let _ = stderr.write_all(bytes);
        let _ = stderr.flush();
        std::mem::forget(stderr);
    }

    fn with_saved_stderr<F: FnOnce()>(f: F) {
        let _lock = STDERR_LOCK.lock().expect("stderr lock");
        let saved = unsafe { libc_dup(2) };
        assert!(saved >= 0, "dup stderr");
        f();
        unsafe {
            libc_dup2(saved, 2);
            libc_close(saved);
        }
    }

    extern "C" {
        #[link_name = "dup"]
        fn libc_dup(fd: i32) -> i32;
        #[link_name = "close"]
        fn libc_close(fd: i32) -> i32;
    }

    #[test]
    fn install_writes_stderr_into_the_log() {
        let path = temp_log();
        let _ = fs::remove_file(&path);
        with_saved_stderr(|| {
            let _guard = install_capped_stderr(&path, 1024).expect("install");
            write_stderr(b"audio stream: POLLERR\n");
            let len = wait_size_at_least(&path, 21, Duration::from_secs(1));
            assert!(len >= 21, "log stayed at {len}");
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("POLLERR"), "{body:?}");
        });
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn install_stops_growing_the_file_once_cap_is_hit() {
        let path = temp_log();
        let _ = fs::remove_file(&path);
        with_saved_stderr(|| {
            let cap = 64u64;
            let _guard = install_capped_stderr(&path, cap).expect("install");
            write_stderr(&[b'x'; 200]);
            thread::sleep(Duration::from_millis(50));
            write_stderr(&[b'y'; 200]);
            let len = wait_size_stable(&path, Duration::from_millis(400));
            assert!(len <= cap, "log grew to {len}, cap {cap}");
            assert!(len > 0, "nothing was written before the cap");
        });
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn install_does_not_append_when_existing_file_already_at_cap() {
        let path = temp_log();
        let _ = fs::remove_file(&path);
        fs::write(&path, vec![b'z'; 32]).unwrap();
        with_saved_stderr(|| {
            let _guard = install_capped_stderr(&path, 32).expect("install");
            write_stderr(b"should-not-land\n");
            thread::sleep(Duration::from_millis(80));
            let len = fs::metadata(&path).unwrap().len();
            assert_eq!(len, 32);
            let body = fs::read(&path).unwrap();
            assert_eq!(body, vec![b'z'; 32]);
        });
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn child_writes_to_inherited_stderr_stop_at_the_cap() {
        // Simulate alsa-lib: a writer on the inherited stderr fd.
        let path = temp_log();
        let _ = fs::remove_file(&path);
        with_saved_stderr(|| {
            let cap = 128u64;
            let _guard = install_capped_stderr(&path, cap).expect("install");
            let mut child = Command::new("sh")
                .arg("-c")
                .arg("dd if=/dev/zero bs=64 count=8 status=none >&2")
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("dd");
            let _ = child.wait();
            let len = wait_size_stable(&path, Duration::from_millis(400));
            assert!(len <= cap, "child wrote {len} past cap {cap}");
            assert!(len > 0, "child wrote nothing");
        });
        let _ = fs::remove_file(&path);
    }
}

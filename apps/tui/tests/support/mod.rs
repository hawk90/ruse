//! PTY end-to-end harness (#896): run the real `ruse` binary on a real pseudo-terminal, type raw bytes, and
//! observe what only a process boundary can show: disk contents, exit status, and terminal teardown.
//!
//! Isolation: the child gets an explicit environment with a scratch `HOME` and no inherited `TMUX` or
//! `COLORTERM`. Safety: every wait is bounded, and `Drop` kills and reaps only this harness's own child.

#![cfg(unix)]
#![allow(dead_code)] // Each test binary uses a subset.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const QUIET: Duration = Duration::from_millis(200);
const MAX_WAIT: Duration = Duration::from_secs(10);

/// A unique scratch directory, removed on drop.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ruse-pty-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("home")).expect("create scratch dir");
        Scratch(dir)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    pub fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.path(name);
        std::fs::write(&p, bytes).expect("write scratch file");
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The real binary, running on a PTY.
pub struct Editor {
    master: File,
    pid: libc::pid_t,
    output: Arc<Mutex<Vec<u8>>>,
    status: Option<i32>,
}

fn cstring(bytes: &[u8]) -> CString {
    CString::new(bytes).expect("no interior NUL")
}

impl Editor {
    /// Start `ruse <args>` in `scratch`, and wait until its first frame has been drawn.
    pub fn start(scratch: &Scratch, args: &[&Path]) -> Self {
        // Build everything in the PARENT: nothing may allocate between fork and exec.
        let exe = cstring(env!("CARGO_BIN_EXE_ruse").as_bytes());
        let mut arg_store = vec![exe.clone()];
        arg_store.extend(args.iter().map(|a| cstring(a.as_os_str().as_bytes())));
        let mut argv: Vec<*const libc::c_char> = arg_store.iter().map(|a| a.as_ptr()).collect();
        argv.push(ptr::null());
        let home = scratch.0.join("home");
        let env_store: Vec<CString> = [
            b"TERM=xterm-256color".to_vec(),
            b"LANG=C.UTF-8".to_vec(),
            [b"HOME=".as_slice(), home.as_os_str().as_bytes()].concat(),
            [b"XDG_STATE_HOME=".as_slice(), home.as_os_str().as_bytes()].concat(),
            [b"XDG_CONFIG_HOME=".as_slice(), home.as_os_str().as_bytes()].concat(),
        ]
        .iter()
        .map(|e| cstring(e))
        .collect();
        let mut envp: Vec<*const libc::c_char> = env_store.iter().map(|e| e.as_ptr()).collect();
        envp.push(ptr::null());
        let cwd = cstring(scratch.0.as_os_str().as_bytes());

        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let size_ptr: *mut libc::winsize = &mut size; // Fits both the apple and linux signatures.
        let mut fd = -1;
        // SAFETY: `fd` is a valid out-pointer, name and termios are NULL (defaults), winsize is initialised.
        let pid = unsafe { libc::forkpty(&mut fd, ptr::null_mut(), ptr::null_mut(), size_ptr) };
        assert!(pid >= 0, "forkpty failed");
        if pid == 0 {
            // SAFETY: only async-signal-safe calls; all pointers were built before the fork.
            unsafe {
                libc::chdir(cwd.as_ptr());
                libc::execve(exe.as_ptr(), argv.as_ptr() as _, envp.as_ptr() as _);
                libc::_exit(127);
            }
        }
        // SAFETY: forkpty returned a fresh master fd that nothing else owns.
        let master = unsafe { File::from_raw_fd(fd) };
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut reader = master.try_clone().expect("clone master fd");
        let sink = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            // Linux reports EIO, not EOF, after the child exits; both end the stream.
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let mut editor = Editor {
            master,
            pid,
            output,
            status: None,
        };
        editor.wait_for_first_frame();
        editor
    }

    fn wait_for_first_frame(&mut self) {
        let start = Instant::now();
        while self.output_len() == 0 {
            assert!(start.elapsed() < MAX_WAIT, "ruse drew nothing");
            std::thread::sleep(Duration::from_millis(10));
        }
        self.settle();
    }

    fn output_len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    /// Everything the child has written to the terminal so far, lossily decoded.
    pub fn screen_bytes(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Wait until output has been quiet for a moment, or the child has exited.
    pub fn settle(&mut self) {
        let start = Instant::now();
        let mut last = self.output_len();
        let mut changed = Instant::now();
        while start.elapsed() < MAX_WAIT && changed.elapsed() < QUIET {
            std::thread::sleep(Duration::from_millis(10));
            let now = self.output_len();
            if now != last {
                last = now;
                changed = Instant::now();
            }
        }
    }

    /// Type `keys`, then settle. A lone ESC followed at once by another byte would decode as Alt, so the
    /// input is split after each ESC the way a person's typing is.
    pub fn keys(&mut self, keys: &str) {
        for chunk in keys.split_inclusive('\u{1b}') {
            // A write error means the child already exited; `exit_status` reports that outcome.
            let _ = self.master.write_all(chunk.as_bytes());
            let _ = self.master.flush();
            self.settle();
        }
    }

    /// The exit status if the child has exited, without waiting.
    pub fn try_status(&mut self) -> Option<i32> {
        if self.status.is_none() {
            let mut raw = 0;
            // SAFETY: `pid` is this harness's own child and `raw` is a valid out-pointer.
            if unsafe { libc::waitpid(self.pid, &mut raw, libc::WNOHANG) } == self.pid {
                self.status = Some(if libc::WIFEXITED(raw) {
                    libc::WEXITSTATUS(raw)
                } else {
                    -libc::WTERMSIG(raw)
                });
            }
        }
        self.status
    }

    /// Wait for the child to exit and return its status; a signal `n` is reported as `-n`.
    pub fn exit_status(&mut self) -> i32 {
        let start = Instant::now();
        loop {
            if let Some(status) = self.try_status() {
                return status;
            }
            assert!(start.elapsed() < MAX_WAIT, "ruse did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Send `sig` to this harness's own child (only while it is unreaped, so the pid cannot be reused).
    pub fn signal(&mut self, sig: libc::c_int) {
        if self.try_status().is_none() {
            // SAFETY: `pid` is this harness's own child, not yet reaped.
            unsafe {
                libc::kill(self.pid, sig);
            }
        }
    }

    pub fn is_running(&mut self) -> bool {
        self.try_status().is_none()
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        if self.try_status().is_none() {
            // SAFETY: signals and reaps only this harness's own unreaped child.
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
                libc::waitpid(self.pid, ptr::null_mut(), 0);
            }
        }
    }
}

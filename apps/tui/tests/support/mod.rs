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

/// Read `path` until it holds `want` or a bounded deadline passes, then return what it holds. On a loaded CI
/// runner a write can land after the editor's output went quiet, so a single read right after `:w` races.
pub fn wait_for_file(path: &Path, want: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let on_disk = std::fs::read(path).unwrap_or_default();
        if on_disk == want || Instant::now() >= deadline {
            return on_disk;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The real binary, running on a PTY.
pub struct Editor {
    master: File,
    pid: libc::pid_t,
    output: Arc<Mutex<Vec<u8>>>,
    status: Option<i32>,
    /// Asks the reader thread to stop and drop its duplicate of the master, so [`Editor::hang_up`] can close
    /// every master handle.
    reader_stop: Arc<std::sync::atomic::AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
}

fn cstring(bytes: &[u8]) -> CString {
    CString::new(bytes).expect("no interior NUL")
}

impl Editor {
    /// Start `ruse <args>` in `scratch` with `env` (`NAME=value`) ahead of the defaults, so it can override
    /// them, and wait until its first frame has been drawn.
    pub fn start_env(scratch: &Scratch, args: &[&Path], env: &[&str]) -> Self {
        Self::spawn(scratch, args, env, b"")
    }

    /// Start `ruse <args>` with `early` already typed: written to the terminal right after the fork, so
    /// it is waiting in the input queue when the editor starts and its startup probe reads the terminal.
    pub fn start_typed(scratch: &Scratch, args: &[&Path], early: &[u8]) -> Self {
        Self::spawn(scratch, args, &[], early)
    }

    fn spawn(scratch: &Scratch, args: &[&Path], env: &[&str], early: &[u8]) -> Self {
        // Build everything in the PARENT: nothing may allocate between fork and exec.
        let exe = cstring(env!("CARGO_BIN_EXE_ruse").as_bytes());
        let mut arg_store = vec![exe.clone()];
        arg_store.extend(args.iter().map(|a| cstring(a.as_os_str().as_bytes())));
        let mut argv: Vec<*const libc::c_char> = arg_store.iter().map(|a| a.as_ptr()).collect();
        argv.push(ptr::null());
        let home = scratch.0.join("home");
        let env_store: Vec<CString> = env
            .iter()
            .map(|e| e.as_bytes().to_vec())
            .chain([
                b"TERM=xterm-256color".to_vec(),
                b"LANG=C.UTF-8".to_vec(),
                [b"HOME=".as_slice(), home.as_os_str().as_bytes()].concat(),
                [b"XDG_STATE_HOME=".as_slice(), home.as_os_str().as_bytes()].concat(),
                [b"XDG_CONFIG_HOME=".as_slice(), home.as_os_str().as_bytes()].concat(),
            ])
            .map(|e| cstring(&e))
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
        let mut master = unsafe { File::from_raw_fd(fd) };
        if !early.is_empty() {
            // Small enough to fit the input queue at once, so this cannot block.
            let _ = master.write_all(early);
            let _ = master.flush();
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut reader = master.try_clone().expect("clone master fd");
        let sink = Arc::clone(&output);
        let reader_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = Arc::clone(&reader_stop);
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut pfd = libc::pollfd {
                fd: std::os::unix::io::AsRawFd::as_raw_fd(&reader),
                events: libc::POLLIN,
                revents: 0,
            };
            // Poll with a timeout rather than block in read, so a stop request is seen (`hang_up`).
            while !stop.load(Ordering::SeqCst) {
                // SAFETY: `pfd` is a valid pollfd for an fd this thread owns.
                if unsafe { libc::poll(&mut pfd, 1, 20) } <= 0 {
                    continue; // timeout or EINTR
                }
                // Linux reports EIO, not EOF, after the child exits; both end the stream.
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        let mut editor = Editor {
            master,
            pid,
            output,
            status: None,
            reader_stop,
            reader: Some(reader),
        };
        // The terminal echoes keys typed before raw mode, so with `early` input the first output is that
        // echo: wait for the editor's own screen instead.
        let drawn: &[u8] = if early.is_empty() {
            b""
        } else {
            b"\x1b[?1049h"
        };
        editor.wait_for_first_frame(drawn);
        editor
    }

    fn wait_for_first_frame(&mut self, drawn: &[u8]) {
        let start = Instant::now();
        while self.output_len() == 0
            || !self
                .output
                .lock()
                .unwrap()
                .windows(drawn.len().max(1))
                .any(|w| drawn.is_empty() || w == drawn)
        {
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

/// Gap between raw chunks: long enough that the child reads each one separately, so a sequence split
/// across chunks reaches the decoder split.
const CHUNK_GAP: Duration = Duration::from_millis(40);

impl Editor {
    /// Start `ruse <args>` in `scratch` with the default environment.
    pub fn start(scratch: &Scratch, args: &[&Path]) -> Self {
        Self::start_env(scratch, args, &[])
    }

    /// Write each chunk as its own read for the child (pausing between them), then settle. Unlike
    /// [`Editor::keys`], nothing is split at ESC: the bytes arrive exactly as chunked here.
    pub fn raw_chunks(&mut self, chunks: &[&[u8]]) {
        for (i, chunk) in chunks.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(CHUNK_GAP);
            }
            self.write_bounded(chunk);
        }
        self.settle();
    }

    /// Write `bytes` to the child's terminal and return at once, without settling: for input that must
    /// arrive while the child is busy (CTRL-C during a long command).
    pub fn send(&mut self, bytes: &[u8]) {
        self.write_bounded(bytes);
    }

    /// Wait for the child to exit within `limit` and return its status, or `None` if it is still running
    /// then.
    pub fn exit_within(&mut self, limit: Duration) -> Option<i32> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if let Some(status) = self.try_status() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.try_status()
    }

    /// Write `bytes` to the child's terminal, failing the test instead of blocking forever when the child
    /// stops reading. Small writes go in one piece (so a short sequence lands in one read); large ones in
    /// `PIECE`-byte pieces, each after a bounded wait for room in the PTY input queue.
    fn write_bounded(&mut self, bytes: &[u8]) {
        use std::os::unix::io::AsRawFd;
        const PIECE: usize = 256;
        let start = Instant::now();
        for piece in bytes.chunks(PIECE) {
            let mut pfd = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            loop {
                // SAFETY: `pfd` is a valid, initialised pollfd; poll touches only that one struct.
                let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
                if ready > 0 {
                    break;
                }
                if !self.is_running() {
                    return; // `exit_status` reports the outcome.
                }
                assert!(start.elapsed() < MAX_WAIT, "ruse stopped reading its input");
            }
            // A write error means the child already exited; `exit_status` reports that outcome.
            if self.master.write_all(piece).is_err() {
                return;
            }
        }
        let _ = self.master.flush();
    }
}

/// The outcome of a run of the binary with no terminal: exit status and stderr.
pub struct Detached {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Run `ruse <args>` with `stdin` and `stdout` as given, bounded in time (killed after `MAX_WAIT`).
/// Nothing is drawn, so this suits the paths that exit before entering the terminal.
pub fn run_detached(
    args: &[&str],
    stdin: std::process::Stdio,
    stdout: std::process::Stdio,
) -> Detached {
    use std::process::{Command, Stdio};
    let scratch = Scratch::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ruse"))
        .args(args)
        .current_dir(&scratch.0)
        .env_clear()
        .env("TERM", "xterm-256color")
        .env("HOME", scratch.0.join("home"))
        .env("XDG_STATE_HOME", scratch.0.join("home"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ruse");
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait ruse") {
            break s;
        }
        if start.elapsed() > MAX_WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ruse {args:?} did not exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_end(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    Detached {
        status: status.code().unwrap_or(-1),
        stdout,
        stderr,
    }
}

/// A fresh pseudo-terminal, as (master, slave). The slave can stand in as a child's stdin or stdout, so a
/// test can give the binary a terminal on one side only.
pub fn open_pty() -> (File, File) {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: both out-pointers are valid; name, termios and winsize may be NULL.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    // SAFETY: openpty returned two fresh fds that nothing else owns.
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

impl Editor {
    /// Close the terminal from the outside, as closing a terminal window does (#897): every master handle
    /// goes, so the kernel hangs up the child's session with SIGHUP. Keys can no longer be sent afterwards;
    /// the output read so far stays readable.
    pub fn hang_up(&mut self) {
        use std::os::unix::io::AsRawFd;
        self.reader_stop.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join(); // drops the reader's duplicate of the master
        }
        let null = File::open("/dev/null").expect("open /dev/null");
        // SAFETY: the master fd is this harness's own and nothing else uses it now; dup2 closes the master
        // and leaves the fd pointing at /dev/null, so later writes and the final drop stay harmless.
        unsafe {
            libc::dup2(null.as_raw_fd(), self.master.as_raw_fd());
        }
    }
}

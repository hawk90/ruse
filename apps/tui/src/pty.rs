//! Unix pseudo-terminal (F-011 slice 1). `UnixPty::spawn` forks a shell attached to a PTY and streams its
//! output over a bounded `mpsc` channel from a dedicated reader thread — the async seam that lets the editor
//! render shell output without blocking on a keypress. The bound is the backpressure (#770): when the editor
//! falls behind, the reader stops reading, the PTY fills, and the child blocks in `write`. Unix-only (via
//! the `libc` we already depend on, like the F-010 capability probe); Windows ConPTY is a later slice behind
//! the same [`Pty`] boundary (`DEP-PTY`).
//!
//! `unsafe` is confined to this module. `forkpty` sets the child's controlling terminal up itself, so the
//! child only needs an `execvp`; the shell `CString` is built in the PARENT before the fork so that nothing
//! between fork and exec allocates (async-signal-safety: the editor may already be multi-threaded).

#![cfg(unix)]

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::ptr;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode a key event as the bytes xterm sends on the child's stdin (F-011, normal cursor-key mode):
/// printable chars (UTF-8), Enter/Tab/Backspace/Esc, `Ctrl-<key>` control codes, `Alt` as an ESC prefix
/// (xterm `metaSendsEscape`), and the cursor/editing/function keys as their CSI / SS3 sequences, with the
/// xterm modifier parameter (`CSI 1;<mod> A`, `CSI 5;<mod> ~`) when Shift/Alt/Ctrl is held (#784).
/// `None` for keys with no terminal encoding.
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let shift = mods.contains(KeyModifiers::SHIFT);
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    // xterm's modifier parameter: 1 + Shift(1) + Alt(2) + Ctrl(4); 1 means "no modifier".
    let param = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    // `<intro><final>` unmodified (`CSI A`, `SS3 P`), `CSI 1;<mod><final>` modified.
    let letter = |intro: &[u8], fin: u8| {
        if param == 1 {
            [intro, &[fin]].concat()
        } else {
            format!("\x1b[1;{param}{}", fin as char).into_bytes()
        }
    };
    // `CSI <n>~` unmodified, `CSI <n>;<mod>~` modified.
    let tilde = |n: u8| {
        if param == 1 {
            format!("\x1b[{n}~").into_bytes()
        } else {
            format!("\x1b[{n};{param}~").into_bytes()
        }
    };
    // The keys below whose Alt form is the plain form behind an ESC; Shift is already in a char's case.
    let base = match key.code {
        // A chord with no control byte (`C-1`, `C-.`) sends the plain char, as xterm does by default.
        KeyCode::Char(c) if ctrl => ctrl_byte(c).map_or_else(|| utf8(c), |b| vec![b]),
        KeyCode::Char(c) => utf8(c),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab if shift => return Some(b"\x1b[Z".to_vec()),
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::BackTab => return Some(b"\x1b[Z".to_vec()),
        KeyCode::Up => return Some(letter(b"\x1b[", b'A')),
        KeyCode::Down => return Some(letter(b"\x1b[", b'B')),
        KeyCode::Right => return Some(letter(b"\x1b[", b'C')),
        KeyCode::Left => return Some(letter(b"\x1b[", b'D')),
        KeyCode::Home => return Some(letter(b"\x1b[", b'H')),
        KeyCode::End => return Some(letter(b"\x1b[", b'F')),
        KeyCode::F(n @ 1..=4) => return Some(letter(b"\x1bO", b'P' + (n - 1))),
        KeyCode::Insert => return Some(tilde(2)),
        KeyCode::Delete => return Some(tilde(3)),
        KeyCode::PageUp => return Some(tilde(5)),
        KeyCode::PageDown => return Some(tilde(6)),
        KeyCode::F(n @ 5..=12) => {
            return Some(tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)]))
        }
        _ => return None,
    };
    Some(if alt {
        [&[0x1b], &base[..]].concat()
    } else {
        base
    })
}

fn utf8(c: char) -> Vec<u8> {
    let mut b = [0u8; 4];
    c.encode_utf8(&mut b).as_bytes().to_vec()
}

/// The C0 byte xterm sends for `Ctrl-<c>`, also in the `Ctrl-2`..`Ctrl-8` form crossterm decodes from a
/// legacy terminal (#603); `None` for a char with no control byte.
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        lc @ 'a'..='z' => lc as u8 - b'a' + 1, // C-a = 0x01 … C-z = 0x1a
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// The platform PTY boundary (`DEP-PTY` exit strategy). Slice 1 ships only [`UnixPty`]; a future ConPTY impl
/// slots in behind this trait without touching the session.
pub trait Pty {
    /// Forward raw bytes to the child's stdin (key forwarding).
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Tell the child its window is now `rows × cols` (so it reflows / re-renders). F-011 slice 2.
    fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()>;
}

/// Output chunks (each at most [`READ_CHUNK`] bytes) the channel holds before the reader blocks (#770): a
/// flooding child can queue at most 256 KiB here ahead of the editor, plus the kernel's own PTY buffer.
pub const CHANNEL_CHUNKS: usize = 64;
/// The reader's read size, and so the largest chunk on the channel.
pub const READ_CHUNK: usize = 4096;
/// How often a reader blocked on a full channel re-checks whether `Drop` asked it to stop.
const FULL_RETRY: Duration = Duration::from_millis(10);

/// How long the child may act on SIGHUP before the reaper escalates to SIGKILL (#771).
const HANGUP_GRACE: Duration = Duration::from_millis(500);
/// How long the reaper waits to reap the child after SIGKILL before it gives up (a process stuck in an
/// uninterruptible sleep cannot be reaped; leaking that zombie beats freezing the editor).
const KILL_GRACE: Duration = Duration::from_secs(2);

/// A shell running on a Unix PTY. Owns the master fd (as a [`File`]), the reader thread, and the write end
/// of the reader's wake pipe; `Drop` stops the reader, closes the master, hangs up the child, and hands it
/// to the reaper thread, which kills it if it must and reaps it, so a child that ignores SIGHUP cannot
/// freeze the editor (#771, #1775).
pub struct UnixPty {
    /// `None` only inside `Drop`, which closes it before hanging up the child.
    master: Option<File>,
    child_pid: libc::pid_t,
    reader: Option<JoinHandle<()>>,
    /// Closing this wakes the reader's `poll` (POLLHUP on the read end), so the reader exits even while a
    /// process that outlives the child still holds the slave open and no EOF ever comes.
    wake: Option<File>,
}

impl UnixPty {
    /// Fork `$SHELL` (or `/bin/sh`) on a fresh PTY sized `rows × cols`, returning the handle and the receiver
    /// its reader thread streams output chunks into. The channel disconnects (reader thread ends) on EOF —
    /// i.e. when the child exits.
    pub fn spawn(rows: u16, cols: u16) -> io::Result<(UnixPty, Receiver<Vec<u8>>)> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Self::spawn_argv(&[shell.as_str()], rows, cols)
    }

    /// Fork `argv` (`argv[0]` looked up on `PATH`) on a fresh PTY sized `rows × cols`.
    fn spawn_argv(argv: &[&str], rows: u16, cols: u16) -> io::Result<(UnixPty, Receiver<Vec<u8>>)> {
        // Build argv in the PARENT: no allocation may happen between fork and exec.
        let c_args = argv
            .iter()
            .map(|a| CString::new(*a))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argv has an interior NUL"))?;
        let Some(c_prog) = c_args.first() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
        };
        let c_argv: Vec<*const libc::c_char> = c_args
            .iter()
            .map(|a| a.as_ptr())
            .chain(std::iter::once(ptr::null()))
            .collect();
        // The reader's wake pipe (close-on-exec), made before the fork like everything else that allocates.
        let (wake_rx, wake_tx) = wake_pipe()?;

        let mut winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let mut master: RawFd = -1;
        // Pass raw pointers (not `&mut`) so the call fits BOTH signatures: apple's `*mut winsize` and linux's
        // `*const winsize` (a `*mut` coerces to `*const`), without clippy's `unnecessary_mut_passed` firing.
        let winp: *mut libc::winsize = &mut winsize;
        // SAFETY: `master` is a valid out-pointer; name/termios are NULL (defaults); winsize is initialised.
        let pid = unsafe { libc::forkpty(&mut master, ptr::null_mut(), ptr::null_mut(), winp) };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // CHILD: `forkpty` already made the slave our controlling terminal on fds 0/1/2. Only
            // async-signal-safe calls here. `c_args`/`c_argv` live at the same COW addresses as the parent;
            // the wake pipe is close-on-exec, so the exec'd program never holds it.
            // SAFETY: c_argv is NUL-terminated and points at valid CStrings; on success execvp never returns.
            unsafe {
                libc::execvp(c_prog.as_ptr(), c_argv.as_ptr());
                libc::_exit(127); // exec failed (e.g. no such shell)
            }
        }

        // PARENT: keep the master; the child inherited it but it is close-on-exec so it is already gone there.
        // SAFETY: `master` is a fresh, owned fd returned by forkpty.
        let master = unsafe { File::from_raw_fd(master) };
        // Own the child BEFORE anything else can fail (#1754): from here every early return drops `pty`,
        // whose `Drop` hangs the child up, kills it if it must, and reaps it. `reader` stays `None` until
        // the reader thread exists, which `Drop` handles.
        let mut pty = UnixPty {
            master: Some(master),
            child_pid: pid,
            reader: None,
            wake: Some(wake_tx),
        };
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CHUNKS);
        let reader_fd = match pty.master.as_ref() {
            Some(m) => fault(FaultPoint::CloneMaster).and_then(|()| m.try_clone())?,
            None => return Err(io::ErrorKind::BrokenPipe.into()),
        };
        pty.reader = Some(spawn_reader(reader_fd, wake_rx, tx)?);
        Ok((pty, rx))
    }
}

impl Pty for UnixPty {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.master.as_mut() {
            Some(m) => m.write_all(bytes),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        let mut winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Raw pointer (not `&mut`) to dodge the cross-platform `unnecessary_mut_passed` clippy lint, per the
        // slice-1 forkpty hazard. SAFETY: `master` is a live PTY master fd; `winp` points at an init winsize.
        let winp: *mut libc::winsize = &mut winsize;
        let Some(master) = self.master.as_ref() else {
            return Err(io::ErrorKind::BrokenPipe.into());
        };
        let r = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, winp) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for UnixPty {
    /// Bounded teardown that never waits on the child (#771, #1775): it runs on the main thread when a
    /// terminal buffer closes (`:bd!`) as well as at exit, so the grace periods run on the reaper thread.
    fn drop(&mut self) {
        // 1. Stop the reader. Closing the wake pipe's write end wakes its `poll`, so it exits even while a
        //    background process still holds the slave (on Linux a master read only fails with EIO once EVERY
        //    slave fd is closed, so EOF alone may never come). The reader blocks nowhere else, so this join
        //    returns promptly.
        drop(self.wake.take());
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        // 2. Close the master (the reader's dup is already closed): the slave side sees a hangup, as when a
        //    terminal emulator window closes.
        drop(self.master.take());
        // 3. Hang up the child's process group (forkpty made the child a session leader, so its pgid is its
        //    pid), then hand it to the reaper, which gives it a moment, SIGKILLs the group and reaps it.
        //    `trap '' HUP` does not survive this.
        let pid = self.child_pid;
        signal_child(pid, libc::SIGHUP);
        if !reaper::hand_off(pid) {
            // No reaper thread could be started: reap here, bounded, as before #1775.
            reap_bounded(pid);
        }
    }
}

/// Wait out the hangup grace, SIGKILL the group if the child is still there, and reap it, giving up after
/// the kill grace. The synchronous fallback for when no reaper thread can be started.
fn reap_bounded(pid: libc::pid_t) {
    if reap_within(pid, HANGUP_GRACE) {
        return;
    }
    signal_child(pid, libc::SIGKILL);
    let _ = reap_within(pid, KILL_GRACE);
}

/// Wait (at most one full escalation, plus slack) for every child handed to the reaper to be reaped or
/// given up on. Called once at exit, after the terminals are dropped, so a child that ignores SIGHUP is
/// still SIGKILLed before the editor exits, as when `Drop` did it inline (#1775).
pub fn finish_reaping() {
    reaper::wait_idle(HANGUP_GRACE + KILL_GRACE + Duration::from_millis(100));
}

/// The one reaper thread (#1775): `Drop` hands it each hung-up child, and it runs the bounded
/// HUP-then-KILL escalation for all of them at once, off the event loop. Started on the first hand-off and
/// kept for the life of the process, so closing any number of terminals costs one thread.
mod reaper {
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
    use std::time::{Duration, Instant};

    use super::{signal_child, HANGUP_GRACE, KILL_GRACE};

    /// How often the reaper polls `waitpid` while it has children in hand.
    const POLL: Duration = Duration::from_millis(5);

    /// The reaper's inbox, or `None` when its thread could not be started.
    static INBOX: OnceLock<Option<Sender<libc::pid_t>>> = OnceLock::new();
    /// Children handed off and not yet reaped or given up on.
    static PENDING: Mutex<usize> = Mutex::new(0);
    /// Signalled whenever `PENDING` drops.
    static IDLE: Condvar = Condvar::new();

    /// Give `pid` (already sent SIGHUP) to the reaper. `false` when there is no reaper thread, so the
    /// caller must reap it itself.
    pub(super) fn hand_off(pid: libc::pid_t) -> bool {
        let inbox = INBOX.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("pty-reaper".into())
                .spawn(move || run(&rx))
                .ok()
                .map(|_| tx)
        });
        let Some(tx) = inbox else {
            return false;
        };
        *PENDING.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        if tx.send(pid).is_ok() {
            return true;
        }
        done(); // the reaper is gone (it never exits, but never leak the count)
        false
    }

    /// Block until no handed-off child is pending, or `limit` passes.
    pub(super) fn wait_idle(limit: Duration) {
        let pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = IDLE
            .wait_timeout_while(pending, limit, |n| *n > 0)
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// One child handed off: reaped, or given up on.
    fn done() {
        let mut n = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        *n = n.saturating_sub(1);
        IDLE.notify_all();
    }

    /// Where one child stands in the escalation.
    struct Child {
        pid: libc::pid_t,
        /// Whether it has been sent SIGKILL (the SIGHUP came from `Drop`).
        killed: bool,
        /// When the current grace period ends.
        deadline: Instant,
    }

    /// Whether `pid` is reaped now (or is no longer our child), without blocking.
    fn reaped(pid: libc::pid_t) -> bool {
        let mut status = 0;
        // SAFETY: `pid` is our own child; `status` is a valid out-pointer; WNOHANG never blocks.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        // `r < 0` is ECHILD (nothing left to reap) or EINTR, which the next poll retries.
        r == pid
            || (r < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted)
    }

    /// The reaper loop: take new children, reap the ones that exited, SIGKILL the ones whose hangup grace
    /// ran out, and give up on the ones that outlast the kill grace (a process stuck in an uninterruptible
    /// sleep cannot be reaped; leaking that zombie beats holding it forever).
    fn run(inbox: &Receiver<libc::pid_t>) {
        let mut children: Vec<Child> = Vec::new();
        loop {
            let next = if children.is_empty() {
                inbox.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                inbox.recv_timeout(POLL)
            };
            match next {
                Ok(pid) => children.push(Child {
                    pid,
                    killed: false,
                    deadline: Instant::now() + HANGUP_GRACE,
                }),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) if children.is_empty() => return,
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(POLL),
            }
            let now = Instant::now();
            children.retain_mut(|c| {
                if reaped(c.pid) {
                    done();
                    return false;
                }
                if now < c.deadline {
                    return true;
                }
                if c.killed {
                    done(); // give up: the zombie leaks
                    return false;
                }
                signal_child(c.pid, libc::SIGKILL);
                c.killed = true;
                c.deadline = now + KILL_GRACE;
                true
            });
        }
    }
}

/// Send `sig` to the child's process group and to the child itself (in case it left its group).
fn signal_child(pid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: `pid` is our own child and the leader of its own process group; kill touches no memory.
    unsafe {
        libc::kill(-pid, sig);
        libc::kill(pid, sig);
    }
}

/// Poll `waitpid(pid, WNOHANG)` until the child is reaped or `limit` passes. `true` once it is reaped (or is
/// no longer our child), `false` on timeout.
fn reap_within(pid: libc::pid_t, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        let mut status = 0;
        // SAFETY: `pid` is our own child; `status` is a valid out-pointer.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return true;
        }
        if r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return true; // ECHILD: nothing left to reap
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A close-on-exec pipe `(read end, write end)` that wakes the reader thread when the write end closes.
fn wake_pipe() -> io::Result<(File, File)> {
    let mut fds: [RawFd; 2] = [-1; 2];
    // SAFETY: `fds` is a valid two-element out-array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both fds are fresh and owned here; each File closes its own.
    let (r, w) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
    for f in [&r, &w] {
        // SAFETY: a live fd we own; F_SETFD only sets its descriptor flags.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((r, w))
}

/// Whether `wake` fires (the write end closed) within `limit`: the reader's sleep while the channel is full.
fn woken_within(wake: RawFd, limit: Duration) -> bool {
    let mut fd = libc::pollfd {
        fd: wake,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = libc::c_int::try_from(limit.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: `fd` is one initialised pollfd. An EINTR just ends this sleep early; the caller retries.
    let r = unsafe { libc::poll(&mut fd, 1, ms) };
    r > 0 && fd.revents != 0
}

/// Queue `chunk` on the bounded channel, blocking while it is full (the backpressure, #770) but never past
/// a wake from `Drop`. `false` when the reader should stop: woken, or the receiver is gone.
fn send_chunk(tx: &SyncSender<Vec<u8>>, mut chunk: Vec<u8>, wake: RawFd) -> bool {
    loop {
        match tx.try_send(chunk) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(c)) => {
                if woken_within(wake, FULL_RETRY) {
                    return false;
                }
                chunk = c;
            }
        }
    }
}

/// Block until `fd` is readable (or hung up) or `wake` fires. `Ok(true)` when `fd` is ready, `Ok(false)` when
/// woken to stop.
fn wait_readable(fd: RawFd, wake: RawFd) -> io::Result<bool> {
    let mut fds = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: `fds` is a valid array of two initialised pollfds.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if fds[1].revents != 0 {
            return Ok(false);
        }
        if fds[0].revents != 0 {
            return Ok(true);
        }
    }
}

/// Read the PTY master in a dedicated thread, forwarding output chunks to the bounded `tx`; while the channel
/// is full it stops reading, so the child blocks on a full PTY (#770). Ends on EOF (child exit), on a wake
/// from `Drop` (the wake pipe's write end closed), or once the receiver is dropped; dropping `tx` on exit
/// disconnects the channel, signalling the session.
/// A failed thread spawn is an error (never a panic, unlike `thread::spawn`), so the caller can reap the
/// child (#1754).
fn spawn_reader(mut fd: File, wake: File, tx: SyncSender<Vec<u8>>) -> io::Result<JoinHandle<()>> {
    fault(FaultPoint::SpawnReader)?;
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; READ_CHUNK];
            // Stops when woken by Drop (`Ok(false)`) or when poll itself fails.
            while let Ok(true) = wait_readable(fd.as_raw_fd(), wake.as_raw_fd()) {
                match fd.read(&mut buf) {
                    Ok(0) => break, // EOF: the child closed the tty
                    Ok(n) => {
                        if !send_chunk(&tx, buf[..n].to_vec(), wake.as_raw_fd()) {
                            break; // woken by Drop, or the session dropped the receiver
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break, // master closed / I/O error (Linux: EIO once the slave is gone)
                }
            }
        })
}

/// The steps after `forkpty` that can fail, so a test can make each fail on demand (#1754): the fd limit
/// cannot reach them, because `forkpty` itself needs at least as many fds as the clone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultPoint {
    /// `try_clone` of the master for the reader.
    CloneMaster,
    /// Spawning the reader thread.
    SpawnReader,
}

#[cfg(test)]
thread_local! {
    /// The step that fails on this thread's next spawn, consumed when it fires.
    static FAULT: std::cell::Cell<Option<FaultPoint>> = const { std::cell::Cell::new(None) };
}

/// Make `point` fail on this thread's next spawn (test-only fault injection, #1754).
#[cfg(test)]
fn inject_fault(point: FaultPoint) {
    FAULT.with(|f| f.set(Some(point)));
}

/// `Err` when a test injected a fault at `point`; always `Ok` outside tests.
#[cfg(test)]
fn fault(point: FaultPoint) -> io::Result<()> {
    if FAULT.with(|f| f.get()) == Some(point) {
        FAULT.with(|f| f.set(None));
        return Err(io::Error::other(format!("injected fault at {point:?}")));
    }
    Ok(())
}

/// `Err` when a test injected a fault at `point`; always `Ok` outside tests.
#[cfg(not(test))]
#[inline]
fn fault(_point: FaultPoint) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// #603: a C0 punctuation chord reaches the child as its control byte, also in the `CTRL-4`..`CTRL-7`
    /// form crossterm decodes from a legacy terminal, never as the bare digit.
    #[test]
    fn ctrl_chords_forward_as_control_bytes() {
        let ctrl = |c| encode_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        assert_eq!(ctrl('a'), Some(vec![0x01]));
        assert_eq!(ctrl(' '), Some(vec![0x00]));
        assert_eq!(ctrl('4'), Some(vec![0x1c]));
        assert_eq!(ctrl('\\'), Some(vec![0x1c]));
        assert_eq!(ctrl('5'), Some(vec![0x1d]));
        assert_eq!(ctrl(']'), Some(vec![0x1d]));
        assert_eq!(ctrl('7'), Some(vec![0x1f]));
    }

    /// #784: each chord reaches the child as xterm (normal cursor-key mode) sends it — Alt as an ESC
    /// prefix, modified cursor keys in the `CSI 1;<mod>` form, and the editing/function keys that used to
    /// be dropped.
    #[test]
    fn key_chords_forward_as_xterm_sequences() {
        let (n, s, a, c) = (
            KeyModifiers::NONE,
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        );
        let cases: &[(KeyCode, KeyModifiers, &[u8])] = &[
            (KeyCode::Char('b'), n, b"b"),
            (KeyCode::Char('B'), s, b"B"),
            (KeyCode::Char('b'), a, b"\x1bb"),
            (KeyCode::Char('B'), a | s, b"\x1bB"),
            (KeyCode::Char('.'), a, b"\x1b."),
            (KeyCode::Char('b'), a | c, b"\x1b\x02"),
            (KeyCode::Char('1'), c, b"1"), // no control byte: never the macro register's 0x80 tag
            (KeyCode::Char('/'), c, b"\x1f"),
            (KeyCode::Enter, a, b"\x1b\r"),
            (KeyCode::Backspace, a, b"\x1b\x7f"),
            (KeyCode::Up, n, b"\x1b[A"),
            (KeyCode::Up, s, b"\x1b[1;2A"),
            (KeyCode::Left, c, b"\x1b[1;5D"),
            (KeyCode::Right, a, b"\x1b[1;3C"),
            (KeyCode::Down, c | s, b"\x1b[1;6B"),
            (KeyCode::Home, n, b"\x1b[H"),
            (KeyCode::End, s, b"\x1b[1;2F"),
            (KeyCode::BackTab, s, b"\x1b[Z"),
            (KeyCode::Tab, s, b"\x1b[Z"),
            (KeyCode::Insert, n, b"\x1b[2~"),
            (KeyCode::Delete, n, b"\x1b[3~"),
            (KeyCode::Delete, c, b"\x1b[3;5~"),
            (KeyCode::PageUp, n, b"\x1b[5~"),
            (KeyCode::PageDown, s, b"\x1b[6;2~"),
            (KeyCode::F(1), n, b"\x1bOP"),
            (KeyCode::F(4), n, b"\x1bOS"),
            (KeyCode::F(1), s, b"\x1b[1;2P"),
            (KeyCode::F(5), n, b"\x1b[15~"),
            (KeyCode::F(10), n, b"\x1b[21~"),
            (KeyCode::F(11), n, b"\x1b[23~"),
            (KeyCode::F(12), c, b"\x1b[24;5~"),
        ];
        for &(code, mods, want) in cases {
            let got = encode_key(KeyEvent::new(code, mods));
            assert_eq!(got.as_deref(), Some(want), "{code:?} + {mods:?}");
        }
        assert_eq!(encode_key(KeyEvent::new(KeyCode::F(13), n)), None);
    }

    /// Read `rx` until the output contains `needle` or `limit` passes; whether it was seen.
    fn wait_for(rx: &Receiver<Vec<u8>>, needle: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
                out.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&out).contains(needle) {
                    return true;
                }
            }
        }
        false
    }

    /// #770: a child that floods output while nobody drains fills the bounded channel and then blocks;
    /// the channel never holds more than `CHANNEL_CHUNKS`, and `Drop` still returns while the reader is
    /// parked on the full channel.
    #[test]
    fn a_flood_fills_the_bounded_channel_and_then_blocks() {
        let Ok((pty, rx)) = UnixPty::spawn_argv(&["/bin/sh", "-c", "yes"], 24, 80) else {
            return; // no PTY available (sandbox)
        };
        // Let the flood run with no consumer: the reader must park once the channel is full.
        std::thread::sleep(Duration::from_millis(500));
        let mut queued = 0;
        while rx.try_recv().is_ok() {
            queued += 1;
        }
        assert!(queued > 0, "the flood produced nothing");
        // The reader may slip in one more chunk while we drained, so allow CHANNEL_CHUNKS + 1.
        assert!(
            queued <= CHANNEL_CHUNKS + 1,
            "{queued} chunks queued past the bound {CHANNEL_CHUNKS}"
        );
        // Park the reader on a full channel again, then drop with the receiver still alive.
        std::thread::sleep(Duration::from_millis(200));
        let start = Instant::now();
        drop(pty);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "Drop hung on a full channel"
        );
        drop(rx);
    }

    /// How the drop of one PTY went: how long `Drop` itself took, and how long after `Drop` began the child
    /// was gone (reaped, not even a zombie), if it went within the patience limit.
    struct Teardown {
        took: Duration,
        gone_after: Option<Duration>,
    }

    /// Spawn `script` under `sh -c`, wait for it to print `READY`, drop the PTY, and wait for the child to be
    /// reaped. `None` when the environment denies a PTY.
    fn drop_after_ready(script: &str) -> Option<Teardown> {
        let (pty, rx) = UnixPty::spawn_argv(&["/bin/sh", "-c", script], 24, 80).ok()?;
        assert!(
            wait_for(&rx, "READY", Duration::from_secs(5)),
            "child never got ready"
        );
        let pid = pty.child_pid;
        let start = Instant::now();
        drop(pty);
        let took = start.elapsed();
        let deadline = start + Duration::from_secs(10);
        let gone_after = loop {
            // SAFETY: signal 0 only probes whether `pid` exists; a zombie still does.
            let r = unsafe { libc::kill(pid, 0) };
            if r == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break Some(start.elapsed());
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        Some(Teardown { took, gone_after })
    }

    /// #771: a child that ignores SIGHUP no longer hangs `Drop` in a blocking `waitpid`: it is SIGKILLed after
    /// the grace period and reaped.
    #[test]
    fn drop_kills_a_child_that_ignores_sighup() {
        let Some(t) = drop_after_ready("trap '' HUP; echo READY; sleep 30") else {
            return; // no PTY available (sandbox)
        };
        assert!(t.took < Duration::from_secs(5), "Drop took {:?}", t.took);
        assert!(t.gone_after.is_some(), "the child survived Drop");
    }

    /// #1775: closing a terminal whose child ignores SIGHUP returns at once; the hangup grace and the
    /// SIGKILL run on the reaper thread, which still kills and reaps the child once the grace runs out.
    #[test]
    fn drop_returns_at_once_and_the_reaper_kills_a_child_that_ignores_sighup() {
        let Some(t) = drop_after_ready("trap '' HUP; echo READY; sleep 30") else {
            return; // no PTY available (sandbox)
        };
        assert!(
            t.took < Duration::from_millis(50),
            "Drop blocked for {:?}",
            t.took
        );
        let gone = t.gone_after.expect("the reaper never reaped the child");
        // It ignored SIGHUP, so only the SIGKILL after the grace ended it: the escalation is unchanged.
        assert!(
            gone >= HANGUP_GRACE - Duration::from_millis(50),
            "the child went after {gone:?}, before the hangup grace"
        );
    }

    /// #1775: at exit `finish_reaping` waits for the reaper, so a child that ignores SIGHUP is SIGKILLed and
    /// reaped before the editor exits, never orphaned by it.
    #[test]
    fn finish_reaping_waits_out_the_escalation() {
        let Ok((pty, rx)) = UnixPty::spawn_argv(
            &["/bin/sh", "-c", "trap '' HUP; echo READY; sleep 30"],
            24,
            80,
        ) else {
            return; // no PTY available (sandbox)
        };
        assert!(
            wait_for(&rx, "READY", Duration::from_secs(5)),
            "child never got ready"
        );
        let pid = pty.child_pid;
        drop(pty);
        finish_reaping();
        // SAFETY: signal 0 only probes whether `pid` exists; a zombie still does.
        let r = unsafe { libc::kill(pid, 0) };
        let err = io::Error::last_os_error().raw_os_error();
        assert!(
            r == -1 && err == Some(libc::ESRCH),
            "the child outlived finish_reaping"
        );
    }

    /// #771: a background job in its own process group that ignores SIGHUP keeps the slave open after the
    /// shell dies, so the master never reaches EOF (on Linux a read only fails once every slave fd is
    /// closed). The reader is woken by `Drop` instead, so the join cannot hang.
    #[test]
    fn drop_returns_while_a_background_job_holds_the_slave() {
        let script = "set -m; (trap '' HUP; sleep 10) & trap '' HUP; echo READY; wait";
        let Some(t) = drop_after_ready(script) else {
            return; // no PTY available (sandbox)
        };
        assert!(t.took < Duration::from_secs(5), "Drop took {:?}", t.took);
        assert!(t.gone_after.is_some(), "the shell survived Drop");
    }
}

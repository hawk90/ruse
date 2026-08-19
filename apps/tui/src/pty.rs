//! Unix pseudo-terminal (F-011 slice 1). `UnixPty::spawn` forks a shell attached to a PTY and streams its
//! output over an `mpsc` channel from a dedicated reader thread — the async seam that lets the editor render
//! shell output without blocking on a keypress. Unix-only (via the `libc` we already depend on, like the
//! F-010 capability probe); Windows ConPTY is a later slice behind the same [`Pty`] boundary (`DEP-PTY`).
//!
//! `unsafe` is confined to this module. `forkpty` sets the child's controlling terminal up itself, so the
//! child only needs an `execvp`; the shell `CString` is built in the PARENT before the fork so that nothing
//! between fork and exec allocates (async-signal-safety: the editor may already be multi-threaded).

#![cfg(unix)]

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode a key event as the bytes a terminal expects on the child's stdin (F-011 slice 1, line mode):
/// printable chars (UTF-8), Enter/Tab/Backspace/Esc, `Ctrl-<letter>` control codes, and the common
/// arrow/Home/End/Delete escape sequences. `None` for keys with no terminal encoding.
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let utf8 = |c: char| {
        let mut b = [0u8; 4];
        c.encode_utf8(&mut b).as_bytes().to_vec()
    };
    let bytes = match key.code {
        KeyCode::Char(c) if ctrl => {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphabetic() {
                vec![lc as u8 - b'a' + 1] // C-a = 0x01 … C-z = 0x1a
            } else if c == ' ' {
                vec![0] // C-space = NUL
            } else {
                utf8(c)
            }
        }
        KeyCode::Char(c) => utf8(c),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

/// The platform PTY boundary (`DEP-PTY` exit strategy). Slice 1 ships only [`UnixPty`]; a future ConPTY impl
/// slots in behind this trait without touching the session.
pub trait Pty {
    /// Forward raw bytes to the child's stdin (key forwarding).
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
}

/// A shell running on a Unix PTY. Owns the master fd (as a [`File`]) and the reader thread; `Drop` hangs up
/// the child, reaps it, and joins the reader so no zombie or thread leaks.
pub struct UnixPty {
    master: File,
    child_pid: libc::pid_t,
    reader: Option<JoinHandle<()>>,
}

impl UnixPty {
    /// Fork `$SHELL` (or `/bin/sh`) on a fresh PTY sized `rows × cols`, returning the handle and the receiver
    /// its reader thread streams output chunks into. The channel disconnects (reader thread ends) on EOF —
    /// i.e. when the child exits.
    pub fn spawn(rows: u16, cols: u16) -> io::Result<(UnixPty, Receiver<Vec<u8>>)> {
        // Build argv in the PARENT: no allocation may happen between fork and exec.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let c_shell = CString::new(shell).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "SHELL has an interior NUL")
        })?;
        let argv = [c_shell.as_ptr(), ptr::null()];

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
            // async-signal-safe calls here. `c_shell`/`argv` live at the same COW addresses as the parent.
            // SAFETY: argv is NUL-terminated and points at a valid CString; on success execvp never returns.
            unsafe {
                libc::execvp(c_shell.as_ptr(), argv.as_ptr());
                libc::_exit(127); // exec failed (e.g. no such shell)
            }
        }

        // PARENT: keep the master; the child inherited it but it is close-on-exec so it is already gone there.
        // SAFETY: `master` is a fresh, owned fd returned by forkpty.
        let master = unsafe { File::from_raw_fd(master) };
        let (tx, rx) = mpsc::channel();
        let reader_fd = master.try_clone()?;
        let reader = spawn_reader(reader_fd, tx);
        Ok((
            UnixPty {
                master,
                child_pid: pid,
                reader: Some(reader),
            },
            rx,
        ))
    }
}

impl Pty for UnixPty {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.master.write_all(bytes)
    }
}

impl Drop for UnixPty {
    fn drop(&mut self) {
        // Hang up the child; then reap it so no zombie survives. Closing the slave (child death) EOFs the
        // master, so the reader thread ends and can be joined.
        // SAFETY: `child_pid` is our own child; SIGHUP/waitpid on it are always valid.
        unsafe {
            libc::kill(self.child_pid, libc::SIGHUP);
            let mut status = 0;
            libc::waitpid(self.child_pid, &mut status, 0);
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

/// Read the PTY master in a dedicated thread, forwarding output chunks to `tx`. Ends on EOF (child exit) or
/// once the receiver is dropped; dropping `tx` on exit disconnects the channel, signalling the session.
fn spawn_reader(mut fd: File, tx: Sender<Vec<u8>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match fd.read(&mut buf) {
                Ok(0) => break, // EOF: the child closed the tty
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break; // session dropped the receiver
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break, // master closed / I/O error
            }
        }
    })
}

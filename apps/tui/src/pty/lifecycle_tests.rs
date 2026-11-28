//! PTY child-lifecycle and leak checks for the `:terminal` PTY (#915): after `UnixPty` is dropped, its
//! file descriptors, reader thread and child process are all gone, whether the child was still running,
//! exited on its own, or failed to exec (the reaper thread reaps the child shortly after `Drop` returns,
//! #1775); spawning under fd exhaustion fails cleanly, and so does a failure after the fork (#1754); resize
//! reaches the child. The `Drop`-timing and backpressure cases live in `pty.rs`'s own `tests` module (#770,
//! #771, #1775). The one reaper thread is started by each probe's warm-up spawn, so the thread baseline
//! already counts it.
//!
//! Counting the process's fds and threads is only exact with nothing else running, but the unit-test
//! binary runs many tests in parallel threads. So each counting probe re-runs this test binary with only
//! that probe selected (`--exact`, one test thread) and the [`PROBE_ENV`] variable set; without the
//! variable the probe returns at once.

use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use super::{inject_fault, FaultPoint, Pty, UnixPty};

/// Set (to the probe's name) in the isolated child test process that runs one counting probe.
const PROBE_ENV: &str = "RUSE_PTY_LEAK_PROBE";
/// Generous upper bound for anything that should happen promptly, so a loaded CI runner cannot flake.
const PATIENCE: Duration = Duration::from_secs(10);

/// Run `probe` (a test fn in this module) alone in a fresh copy of this test binary and require it passed.
fn run_isolated(probe: &str) {
    let name = format!("pty::lifecycle_tests::{probe}");
    let exe = std::env::current_exe().expect("test binary path");
    let out = Command::new(exe)
        .args([name.as_str(), "--exact", "--test-threads=1", "--nocapture"])
        .env(PROBE_ENV, probe)
        .output()
        .expect("re-run the test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "isolated probe {name} failed\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "isolated probe {name} did not run\n--- stdout\n{stdout}"
    );
}

/// Whether this process is the isolated copy that should run `probe`.
fn probing(probe: &str) -> bool {
    std::env::var(PROBE_ENV).is_ok_and(|v| v == probe)
}

/// The fds open in this process, by number (`/dev/fd` lists them on both macOS and Linux). The directory
/// handle that lists them is closed before the list is returned, and filtered out by re-checking each fd.
fn open_fds() -> Vec<i32> {
    let names: Vec<i32> = std::fs::read_dir("/dev/fd")
        .expect("list /dev/fd")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    names
        .into_iter()
        // SAFETY: F_GETFD only reads the descriptor flags of `fd`; it fails with EBADF once it is closed.
        .filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1)
        .collect()
}

/// This process's thread count, where the platform exposes it cheaply.
#[cfg(target_os = "linux")]
fn thread_count() -> Option<usize> {
    Some(std::fs::read_dir("/proc/self/task").ok()?.count())
}

/// This process's thread count, where the platform exposes it cheaply.
#[cfg(target_os = "macos")]
fn thread_count() -> Option<usize> {
    // SAFETY: an all-zero proc_taskinfo is a valid value of this plain C struct.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_taskinfo>()).ok()?;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes for the PROC_PIDTASKINFO flavor.
    let n = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    usize::try_from(info.pti_threadnum).ok()
}

/// This process's thread count, where the platform exposes it cheaply.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn thread_count() -> Option<usize> {
    None
}

/// Poll `read` until it returns `want` or [`PATIENCE`] passes; the last value read.
fn settles_to<T: PartialEq + Copy>(want: T, read: impl Fn() -> T) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let got = read();
        if got == want || Instant::now() >= deadline {
            return got;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `true` once this process has no child at all, zombie or running: `waitpid(-1, WNOHANG)` fails with
/// ECHILD. Only meaningful in an isolated probe, where the PTY children are the only children.
fn no_children_left() -> bool {
    let mut status = 0;
    // SAFETY: `status` is a valid out-pointer; WNOHANG never blocks.
    let r = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
    r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
}

/// `true` once `pid` no longer exists, not even as a zombie: `kill(pid, 0)` fails with ESRCH.
fn gone(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only probes whether `pid` exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Read `rx` until the output contains `needle` or [`PATIENCE`] passes; whether it was seen.
fn saw(rx: &Receiver<Vec<u8>>, needle: &str) -> bool {
    let deadline = Instant::now() + PATIENCE;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                out.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&out).contains(needle) {
                    return true;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
    false
}

/// Drain `rx` until the reader thread hangs up (it drops its sender as it ends) or [`PATIENCE`] passes;
/// whether it hung up.
fn disconnects(rx: &Receiver<Vec<u8>>) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return true,
        }
    }
    false
}

/// After `Drop` the reader thread has ended (it was joined, so its sender is gone): whatever it queued
/// drains, then the channel reports a hangup at once, never "empty, still connected".
fn reader_ended(rx: &Receiver<Vec<u8>>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(_) => {}
            Err(TryRecvError::Disconnected) => return true,
            Err(TryRecvError::Empty) => return false,
        }
    }
}

/// How the child stands when its `UnixPty` is dropped, one per teardown path.
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// Still running (`sleep`): `Drop` hangs it up and reaps it.
    Running,
    /// Exited on its own; the reader saw EOF first and the child waits as a zombie until `Drop` reaps it.
    Exited,
    /// `execvp` failed (no such program), so the forked child `_exit`ed with 127: the spawn-failure path.
    ExecFailed,
    /// The user's `$SHELL` via the real `UnixPty::spawn`, dropped at once (maybe before it even exec'd).
    ShellAtOnce,
}

/// Spawn a PTY child that ends as `how`, drop the PTY, and check that the child and the reader are gone.
fn spawn_and_drop(how: Ending) {
    let spawned = match how {
        Ending::Running => UnixPty::spawn_argv(&["/bin/sh", "-c", "echo READY; sleep 30"], 24, 80),
        Ending::Exited => UnixPty::spawn_argv(&["/bin/sh", "-c", "echo BYE"], 24, 80),
        Ending::ExecFailed => UnixPty::spawn_argv(&["/nonexistent/ruse-no-such-program"], 24, 80),
        Ending::ShellAtOnce => UnixPty::spawn(24, 80),
    };
    let (pty, rx) = spawned.expect("spawn a PTY");
    let pid = pty.child_pid;
    match how {
        Ending::Running => assert!(saw(&rx, "READY"), "{how:?}: child never got ready"),
        Ending::Exited | Ending::ExecFailed => {
            assert!(
                disconnects(&rx),
                "{how:?}: reader never saw the child's EOF"
            );
        }
        Ending::ShellAtOnce => {}
    }
    drop(pty);
    assert!(
        reader_ended(&rx),
        "{how:?}: reader thread still connected after Drop"
    );
    // The reaper thread reaps it shortly after `Drop` returns (#1775).
    assert!(
        settles_to(true, || gone(pid)),
        "{how:?}: child {pid} left behind (running or zombie)"
    );
}

/// Outer half of [`fds_threads_and_children_return_to_baseline_probe`].
#[test]
fn fds_threads_and_children_return_to_baseline() {
    if std::env::var_os(PROBE_ENV).is_some() {
        return; // this copy runs only its probe
    }
    run_isolated("fds_threads_and_children_return_to_baseline_probe");
}

/// Open and close many PTYs through every teardown path; afterwards the process holds exactly the fds and
/// threads it had before and has no child left, so neither the master, its reader dup, the wake pipe, the
/// reader thread nor the child (as a zombie) leaks.
#[test]
fn fds_threads_and_children_return_to_baseline_probe() {
    if !probing("fds_threads_and_children_return_to_baseline_probe") {
        return;
    }
    if UnixPty::spawn_argv(&["/bin/sh", "-c", "exit 0"], 24, 80).is_err() {
        // No PTY available (sandbox): nothing to probe.
        return;
    }
    // The probe above warmed up whatever the first spawn initialises lazily; let its teardown settle.
    assert!(
        settles_to(true, no_children_left),
        "warm-up child not reaped"
    );
    let fds = open_fds().len();
    let threads = thread_count();

    let endings = [
        Ending::Running,
        Ending::Exited,
        Ending::ExecFailed,
        Ending::ShellAtOnce,
    ];
    for round in 0..5 {
        for how in endings {
            spawn_and_drop(how);
            assert!(
                settles_to(true, no_children_left),
                "round {round}, {how:?}: a child is left unreaped"
            );
        }
    }

    let after = settles_to(fds, || open_fds().len());
    assert_eq!(after, fds, "fds leaked: {fds} open before, {after} after");
    if let Some(before) = threads {
        let after = settles_to(threads, thread_count);
        assert_eq!(
            after, threads,
            "threads leaked: {before} before, {after:?} after"
        );
    }
}

/// Outer half of [`spawn_under_fd_exhaustion_fails_cleanly_probe`].
#[test]
fn spawn_under_fd_exhaustion_fails_cleanly() {
    if std::env::var_os(PROBE_ENV).is_some() {
        return; // this copy runs only its probe
    }
    run_isolated("spawn_under_fd_exhaustion_fails_cleanly_probe");
}

/// The lowest fd limit that leaves exactly `free` fd numbers unused below it, given the `open` fds.
fn limit_leaving(open: &[i32], free: usize) -> libc::rlim_t {
    let mut limit: i32 = 0;
    loop {
        let used = open.iter().filter(|&&fd| fd < limit).count();
        if usize::try_from(limit).is_ok_and(|l| l - used == free) {
            return libc::rlim_t::try_from(limit).expect("limit fits rlim_t");
        }
        limit += 1;
    }
}

/// With only 0..=6 fds left to open, spawning either fails with an error (never a panic) or yields a
/// working PTY; either way nothing leaks: no fd, no child. Too few fds for even the wake pipe must fail.
#[test]
fn spawn_under_fd_exhaustion_fails_cleanly_probe() {
    if !probing("spawn_under_fd_exhaustion_fails_cleanly_probe") {
        return;
    }
    if UnixPty::spawn_argv(&["/bin/sh", "-c", "exit 0"], 24, 80).is_err() {
        // No PTY available (sandbox): nothing to probe.
        return;
    }
    assert!(
        settles_to(true, no_children_left),
        "warm-up child not reaped"
    );
    let mut saved = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `saved` is a valid out-pointer for getrlimit.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut saved) },
        0
    );
    let open = open_fds();
    let fds = open.len();

    let mut outcomes = Vec::new();
    for free in 0..=6 {
        let tight = libc::rlimit {
            rlim_cur: limit_leaving(&open, free).min(saved.rlim_cur),
            rlim_max: saved.rlim_max,
        };
        // SAFETY: lowering only the soft limit, below the unchanged hard limit, is always permitted.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &tight) }, 0);
        let spawned = UnixPty::spawn_argv(&["/bin/sh", "-c", "exit 0"], 24, 80);
        // Restore before anything else can need an fd (or panic while the limit is tight).
        // SAFETY: restoring the limits read above.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &saved) }, 0);
        outcomes.push((free, spawned.is_ok()));
        if let Ok((pty, rx)) = spawned {
            let pid = pty.child_pid;
            drop(pty);
            assert!(
                reader_ended(&rx),
                "free={free}: reader still connected after Drop"
            );
            assert!(
                settles_to(true, || gone(pid)),
                "free={free}: child {pid} left behind"
            );
        }
        assert!(
            settles_to(true, no_children_left),
            "free={free}: a child is left unreaped after a failed spawn"
        );
        let after = settles_to(fds, || open_fds().len());
        assert_eq!(
            after, fds,
            "free={free}: fds leaked ({fds} before, {after} after)"
        );
    }
    assert!(
        outcomes.iter().take(2).all(|&(_, ok)| !ok),
        "spawn succeeded with fewer than 2 free fds: {outcomes:?}"
    );
}

/// Outer half of [`a_failure_after_fork_reaps_the_child_probe`].
#[test]
fn a_failure_after_fork_reaps_the_child() {
    if std::env::var_os(PROBE_ENV).is_some() {
        return; // this copy runs only its probe
    }
    run_isolated("a_failure_after_fork_reaps_the_child_probe");
}

/// #1754: when a step after `forkpty` fails (the master clone, or the reader thread spawn), `spawn_argv`
/// returns the error and the forked child is gone, not left running or as a zombie, and no fd leaks. The
/// child ignores SIGHUP and SIGTERM, so this also covers the SIGKILL escalation on the error path (#1788).
#[test]
fn a_failure_after_fork_reaps_the_child_probe() {
    if !probing("a_failure_after_fork_reaps_the_child_probe") {
        return;
    }
    if UnixPty::spawn_argv(&["/bin/sh", "-c", "exit 0"], 24, 80).is_err() {
        // No PTY available (sandbox): nothing to probe.
        return;
    }
    assert!(
        settles_to(true, no_children_left),
        "warm-up child not reaped"
    );
    let fds = open_fds().len();
    for point in [FaultPoint::CloneMaster, FaultPoint::SpawnReader] {
        inject_fault(point);
        let spawned = UnixPty::spawn_argv(&["/bin/sh", "-c", "trap '' HUP TERM; sleep 30"], 24, 80);
        let err = spawned.err().expect("the injected fault fails the spawn");
        assert!(
            err.to_string().contains("injected fault"),
            "{point:?}: unexpected error {err}"
        );
        assert!(
            settles_to(true, no_children_left),
            "{point:?}: the forked child is left behind"
        );
        let after = settles_to(fds, || open_fds().len());
        assert_eq!(
            after, fds,
            "{point:?}: fds leaked ({fds} before, {after} after)"
        );
    }
}

/// `resize` reaches the child: `stty size` reports the new window after `TIOCSWINSZ`.
#[test]
fn resize_reaches_the_child() {
    let script = "stty size; echo READY; read x; stty size; sleep 30";
    let Ok((mut pty, rx)) = UnixPty::spawn_argv(&["/bin/sh", "-c", script], 24, 80) else {
        return; // no PTY available (sandbox)
    };
    assert!(saw(&rx, "24 80\r\nREADY"), "child did not start at 24x80");
    pty.resize(30, 100).expect("resize");
    pty.write(b"\n").expect("write");
    assert!(saw(&rx, "30 100"), "child did not see the 30x100 resize");
}

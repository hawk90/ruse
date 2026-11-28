//! Ending a child process (#1774): the one place that waits for a child, then signals, kills and reaps it.
//! The terminal's PTY child, the `:!` shell and the clipboard tools all go through it, each with its own
//! [`Escalation`].
//!
//! An escalation is a list of steps: send a signal, then give the child a grace period to exit. When the
//! last grace runs out the child is given up on (a process stuck in an uninterruptible sleep cannot be
//! reaped; leaking that zombie beats freezing the editor). Every signal goes to the child's process group
//! and to the child itself, so the child must lead its own group (`forkpty`, `process_group(0)`).
//!
//! [`end`] runs an escalation on the calling thread; [`end_in_background`] sends the first signal and hands
//! the rest to one process-wide reaper thread, so closing a terminal never waits on its child (#1775).
//! [`finish_background`] waits for that thread at exit.
//!
//! The waits poll `waitpid(pid, WNOHANG)` on the raw pid, so they also serve a [`std::process::Child`]:
//! once a helper here has reaped it, the caller must not wait on that `Child` again.

#![cfg(unix)]

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

/// One escalation step: send `signal`, then wait up to `grace` for the child to exit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Step {
    pub(crate) signal: libc::c_int,
    pub(crate) grace: Duration,
}

/// How a child is ended: the steps in order. The last step should send `SIGKILL`.
pub(crate) type Escalation = [Step];

/// How long a child may take to exit after `SIGKILL` before it is given up on.
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(2);

/// A cancelled shell command (`:!`, `:r !`, a filter): `SIGTERM`, then `SIGKILL` after 2 s, as nvim's
/// `proc_stop` does on CTRL-C (`KILL_TIMEOUT_MS`), so the command can clean up its temp files and locks
/// (#1748).
pub(crate) const SHELL: &Escalation = &[
    Step {
        signal: libc::SIGTERM,
        grace: Duration::from_secs(2),
    },
    Step {
        signal: libc::SIGKILL,
        grace: KILL_GRACE,
    },
];

/// How often a blocking wait polls `waitpid`.
const POLL: Duration = Duration::from_millis(2);

/// Send `sig` to `pid`'s process group and to `pid` itself (in case it left its group). `pid` must be an
/// unreaped child that leads its own group, so neither id can name anyone else's process.
pub(crate) fn signal(pid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: `pid` is our own unreaped child and leads its own process group; kill touches no memory.
    unsafe {
        libc::kill(-pid, sig);
        libc::kill(pid, sig);
    }
}

/// Reap `pid` if it has exited, without blocking. `Ok(None)` while it runs; `Err` when it is not (or no
/// longer) our child.
fn try_reap(pid: libc::pid_t) -> io::Result<Option<ExitStatus>> {
    loop {
        let mut status = 0;
        // SAFETY: `status` is a valid out-pointer; WNOHANG never blocks.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return Ok(Some(ExitStatus::from_raw(status)));
        }
        if r == 0 {
            return Ok(None);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Wait for `pid` to exit and reap it, giving up at `deadline` (`None`: no deadline) or as soon as `stop`
/// says so. `Ok(Some(status))` once reaped; `Ok(None)` when the wait ended first and the child still runs.
///
/// # Errors
/// When `pid` is not (or no longer) our child.
pub(crate) fn wait(
    pid: libc::pid_t,
    deadline: Option<Instant>,
    mut stop: impl FnMut() -> bool,
) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = try_reap(pid)? {
            return Ok(Some(status));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) || stop() {
            return Ok(None);
        }
        std::thread::sleep(POLL);
    }
}

/// Run `escalation` on `pid` on this thread: each step signals it and waits out its grace. Returns the
/// exit status once it is reaped, or `None` when it outlived every step (or was not our child) and was
/// given up on.
pub(crate) fn end(pid: libc::pid_t, escalation: &Escalation) -> Option<ExitStatus> {
    for step in escalation {
        signal(pid, step.signal);
        match wait(pid, Some(Instant::now() + step.grace), || false) {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
    }
    None
}

/// Wait up to `timeout` for `pid` to exit; if it has not, run `escalation` on it. Either way it is reaped
/// (or given up on) before this returns. The exit status when it exited by itself in time; `None` when it
/// had to be ended (or was not our child).
pub(crate) fn wait_or_end(
    pid: libc::pid_t,
    timeout: Duration,
    escalation: &Escalation,
) -> Option<ExitStatus> {
    match wait(pid, Some(Instant::now() + timeout), || false) {
        Ok(Some(status)) => Some(status),
        Ok(None) => {
            let _ = end(pid, escalation);
            None
        }
        Err(_) => None,
    }
}

/// Send `escalation`'s first signal to `pid` now and leave the rest to the reaper thread, so the caller
/// never waits on the child (#1775). Falls back to [`end`] on this thread when no reaper thread can be
/// started.
pub(crate) fn end_in_background(pid: libc::pid_t, escalation: &'static Escalation) {
    let Some(first) = escalation.first() else {
        return;
    };
    signal(pid, first.signal);
    if !reaper::hand_off(pid, escalation) {
        // No reaper thread: run the rest here, bounded.
        if let Ok(None) = wait(pid, Some(Instant::now() + first.grace), || false) {
            let _ = end(pid, &escalation[1..]);
        }
    }
}

/// Wait (at most `limit`) for every child handed to the reaper to be reaped or given up on. Called once
/// at exit, so no child outlives the editor (#1775).
pub(crate) fn finish_background(limit: Duration) {
    reaper::wait_idle(limit);
}

/// The one reaper thread (#1775): [`end_in_background`] hands it each signalled child, and it runs the rest
/// of each escalation for all of them at once. Started on the first hand-off and kept for the life of the
/// process, so ending any number of children costs one thread.
mod reaper {
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
    use std::time::{Duration, Instant};

    use super::{signal, try_reap, Escalation};

    /// How often the reaper polls `waitpid` while it has children in hand.
    const POLL: Duration = Duration::from_millis(5);

    /// A child handed off, already sent its escalation's first signal.
    type Job = (libc::pid_t, &'static Escalation);

    /// The reaper's inbox, or `None` when its thread could not be started.
    static INBOX: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    /// Children handed off and not yet reaped or given up on.
    static PENDING: Mutex<usize> = Mutex::new(0);
    /// Signalled whenever `PENDING` drops.
    static IDLE: Condvar = Condvar::new();

    /// Give `pid` to the reaper. `false` when there is no reaper thread, so the caller must end it itself.
    pub(super) fn hand_off(pid: libc::pid_t, escalation: &'static Escalation) -> bool {
        let inbox = INBOX.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("reaper".into())
                .spawn(move || run(&rx))
                .ok()
                .map(|_| tx)
        });
        let Some(tx) = inbox else {
            return false;
        };
        *PENDING.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        if tx.send((pid, escalation)).is_ok() {
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

    /// Where one child stands in its escalation.
    struct Child {
        pid: libc::pid_t,
        escalation: &'static Escalation,
        /// The step whose grace is running (its signal was sent).
        step: usize,
        /// When that grace ends.
        deadline: Instant,
    }

    /// The reaper loop: take new children, reap the ones that exited, move the ones whose grace ran out to
    /// their next step, and give up on the ones that outlast the last step.
    fn run(inbox: &Receiver<Job>) {
        let mut children: Vec<Child> = Vec::new();
        loop {
            let next = if children.is_empty() {
                inbox.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                inbox.recv_timeout(POLL)
            };
            match next {
                Ok((pid, escalation)) => children.push(Child {
                    pid,
                    escalation,
                    step: 0,
                    deadline: Instant::now()
                        + escalation.first().map_or(Duration::ZERO, |s| s.grace),
                }),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) if children.is_empty() => return,
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(POLL),
            }
            let now = Instant::now();
            children.retain_mut(|c| {
                // `Err` is ECHILD: nothing left to reap.
                if !matches!(try_reap(c.pid), Ok(None)) {
                    done();
                    return false;
                }
                if now < c.deadline {
                    return true;
                }
                c.step += 1;
                let Some(step) = c.escalation.get(c.step) else {
                    done(); // give up: the zombie leaks
                    return false;
                };
                signal(c.pid, step.signal);
                c.deadline = now + step.grace;
                true
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    /// A short escalation for the tests: `SIGTERM`, then `SIGKILL` after 300 ms.
    const QUICK: &Escalation = &[
        Step {
            signal: libc::SIGTERM,
            grace: Duration::from_millis(300),
        },
        Step {
            signal: libc::SIGKILL,
            grace: KILL_GRACE,
        },
    ];

    /// Spawn `sh -c script` leading its own group, and wait until it prints `READY` (its traps are set).
    fn spawn_ready(script: &str) -> (Child, libc::pid_t) {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("sh spawns");
        let mut line = String::new();
        let stdout = child.stdout.take().expect("piped stdout");
        BufReader::new(stdout).read_line(&mut line).expect("READY");
        assert_eq!(line.trim(), "READY");
        let pid = libc::pid_t::try_from(child.id()).expect("pid fits");
        (child, pid)
    }

    /// Whether `pid` no longer exists (reaped; a zombie would still answer signal 0).
    fn gone(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only probes whether `pid` exists.
        let r = unsafe { libc::kill(pid, 0) };
        r == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// #1748: a child that traps SIGTERM gets to clean up and exit within the grace; it is not SIGKILLed.
    #[test]
    fn a_child_that_traps_sigterm_cleans_up_and_exits_in_the_grace() {
        let (_child, pid) =
            spawn_ready("trap 'exit 7' TERM; echo READY; while :; do sleep 0.05; done");
        let status = end(pid, QUICK).expect("reaped");
        assert_eq!(status.code(), Some(7), "{status:?}");
        assert!(gone(pid));
    }

    /// #1774: a child that ignores SIGTERM is SIGKILLed once the grace runs out, and reaped.
    #[test]
    fn a_child_that_ignores_sigterm_is_killed_after_the_grace() {
        let (_child, pid) = spawn_ready("trap '' TERM; echo READY; sleep 30");
        let t0 = Instant::now();
        let status = end(pid, QUICK).expect("reaped");
        let took = t0.elapsed();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
        assert!(
            took >= Duration::from_millis(250),
            "killed before the grace: {took:?}"
        );
        assert!(took < Duration::from_secs(3), "took {took:?}");
        assert!(gone(pid));
    }

    /// #1774: a child that never exits is ended when the wait times out, within the timeout and the grace.
    #[test]
    fn a_child_that_never_exits_is_ended_at_the_timeout() {
        let (_child, pid) = spawn_ready("echo READY; sleep 30");
        let t0 = Instant::now();
        let got = wait_or_end(pid, Duration::from_millis(100), QUICK);
        assert!(got.is_none(), "{got:?}");
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "took {:?}",
            t0.elapsed()
        );
        assert!(gone(pid));
    }

    /// A child that exits in time hands back its own status.
    #[test]
    fn a_child_that_exits_in_time_keeps_its_status() {
        let (_child, pid) = spawn_ready("echo READY; exit 3");
        let got = wait_or_end(pid, Duration::from_secs(5), QUICK);
        assert_eq!(got.and_then(|s| s.code()), Some(3));
    }

    /// #1775: a background end returns at once and the reaper still runs the escalation to the end.
    #[test]
    fn a_background_end_returns_at_once_and_the_reaper_kills() {
        let (_child, pid) = spawn_ready("trap '' TERM; echo READY; sleep 30");
        let t0 = Instant::now();
        end_in_background(pid, QUICK);
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "blocked {:?}",
            t0.elapsed()
        );
        let until = Instant::now() + Duration::from_secs(5);
        while !gone(pid) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(gone(pid), "the reaper never ended the child");
        assert!(
            t0.elapsed() >= Duration::from_millis(250),
            "killed before the grace"
        );
    }
}

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
//! [`finish_background`] waits for that thread at exit. [`end_group`] (a cancelled `:!`, #1787) returns
//! once the group leader exits and leaves the SIGKILL of what is left of its group, at the end of the grace,
//! to that thread.
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

/// Whether `pid` has exited, WITHOUT reaping it (`waitid` with `WNOWAIT`, on Linux and macOS alike). The
/// unreaped zombie keeps both its pid and its process group id from being reused, so the group can still be
/// signalled safely afterwards. `Ok(None)` while it runs; `Err` when it is not (or no longer) our child.
fn peek(pid: libc::pid_t) -> io::Result<Option<ExitStatus>> {
    let id = libc::id_t::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ECHILD))?;
    loop {
        // SAFETY: a zeroed siginfo_t is a valid out-value; waitid only writes it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid out-pointer; WNOHANG never blocks and WNOWAIT leaves the child waitable.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r == 0 {
            // SAFETY: waitid filled a SIGCHLD siginfo (or left it zeroed); these read its child fields.
            let (got, status) = unsafe { (info.si_pid(), info.si_status()) };
            if got != pid {
                return Ok(None); // still running (POSIX leaves si_pid zero)
            }
            // Rebuild the wait(2) status word `ExitStatus` wraps from the siginfo's code.
            let raw = match info.si_code {
                libc::CLD_EXITED => (status & 0xff) << 8,
                libc::CLD_DUMPED => status | 0x80,
                _ => status, // CLD_KILLED: the signal number
            };
            return Ok(Some(ExitStatus::from_raw(raw)));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// How a command's whole process group is ended (#1787): `escalation` as in [`end`], but each grace watches
/// the leader without reaping it ([`peek`]). Once the leader exits, its status is returned at once (the
/// caller does not wait out the grace), and the reaper thread SIGKILLs whatever is left of its group when
/// that step's grace runs out, then reaps the leader. So a grandchild that ignores `SIGTERM` (`trap ''
/// TERM`, a daemon-like helper) does not outlive the cancel, while one that handles it still gets the whole
/// grace to clean up. nvim stops at the leader (`children_kill_cb` skips a job that has exited); this is a
/// deliberate improvement over it. The leader's unreaped zombie holds the group id until the SIGKILL, so
/// that signal cannot reach a stranger's group. `None` when the leader outlived every step (given up on) or
/// was not our child.
pub(crate) fn end_group(pid: libc::pid_t, escalation: &Escalation) -> Option<ExitStatus> {
    for step in escalation {
        signal(pid, step.signal);
        let deadline = Instant::now() + step.grace;
        loop {
            match peek(pid) {
                Ok(Some(status)) => {
                    if step.signal == libc::SIGKILL || !reaper::hand_off_exited(pid, deadline) {
                        // The group was already SIGKILLed, or there is no reaper thread: end what is left
                        // of the group now and reap the leader here.
                        signal(pid, libc::SIGKILL);
                        let _ = try_reap(pid);
                    }
                    return Some(status);
                }
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(POLL),
                Err(_) => return None,
            }
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

    /// A child handed off: already sent its escalation's first signal, or (`hold` set) an exited group
    /// leader whose zombie is kept until `hold`, when what is left of its group is SIGKILLed (#1787).
    struct Job {
        pid: libc::pid_t,
        escalation: &'static Escalation,
        hold: Option<Instant>,
    }

    /// What a held leader's job runs once its group was SIGKILLed: only the reap.
    const KILLED: &Escalation = &[super::Step {
        signal: libc::SIGKILL,
        grace: super::KILL_GRACE,
    }];

    /// The reaper's inbox, or `None` when its thread could not be started.
    static INBOX: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    /// Children handed off and not yet reaped or given up on.
    static PENDING: Mutex<usize> = Mutex::new(0);
    /// Signalled whenever `PENDING` drops.
    static IDLE: Condvar = Condvar::new();

    /// Give `pid` to the reaper. `false` when there is no reaper thread, so the caller must end it itself.
    pub(super) fn hand_off(pid: libc::pid_t, escalation: &'static Escalation) -> bool {
        send(Job {
            pid,
            escalation,
            hold: None,
        })
    }

    /// Give the reaper `pid`, an exited but unreaped group leader: at `until` it SIGKILLs what is left of
    /// the group, then reaps the leader (#1787). `false` when there is no reaper thread.
    pub(super) fn hand_off_exited(pid: libc::pid_t, until: Instant) -> bool {
        send(Job {
            pid,
            escalation: KILLED,
            hold: Some(until),
        })
    }

    /// Queue `job` for the reaper thread, starting it on first use.
    fn send(job: Job) -> bool {
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
        if tx.send(job).is_ok() {
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
        /// An exited leader kept unreaped until `deadline`, when its group is SIGKILLed (#1787).
        held: bool,
    }

    /// The reaper loop: take new children, SIGKILL the groups of held leaders whose hold ran out, reap the
    /// ones that exited, move the ones whose grace ran out to their next step, and give up on the ones that
    /// outlast the last step.
    fn run(inbox: &Receiver<Job>) {
        let mut children: Vec<Child> = Vec::new();
        loop {
            let next = if children.is_empty() {
                inbox.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                inbox.recv_timeout(POLL)
            };
            match next {
                Ok(job) => children.push(Child {
                    pid: job.pid,
                    escalation: job.escalation,
                    step: 0,
                    deadline: job.hold.unwrap_or_else(|| {
                        Instant::now() + job.escalation.first().map_or(Duration::ZERO, |s| s.grace)
                    }),
                    held: job.hold.is_some(),
                }),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) if children.is_empty() => return,
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(POLL),
            }
            let now = Instant::now();
            children.retain_mut(|c| {
                if c.held {
                    if now < c.deadline {
                        return true; // the zombie holds the group id until then
                    }
                    signal(c.pid, libc::SIGKILL); // what is left of the group
                    c.held = false;
                    c.deadline = now + c.escalation.first().map_or(Duration::ZERO, |s| s.grace);
                }
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
        let (child, pid, line) = spawn_line(script);
        assert_eq!(line.trim(), "READY");
        (child, pid)
    }

    /// Spawn `sh -c script` leading its own group, and return the first line it prints.
    fn spawn_line(script: &str) -> (Child, libc::pid_t, String) {
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
        BufReader::new(stdout).read_line(&mut line).expect("a line");
        let pid = libc::pid_t::try_from(child.id()).expect("pid fits");
        (child, pid, line)
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

    /// #1787: ending a group returns once its leader exits, with the leader's own status; a member that
    /// ignores SIGTERM keeps running through the grace and is SIGKILLed when it runs out; the leader is
    /// reaped after that.
    #[test]
    fn ending_a_group_kills_a_member_that_ignores_sigterm_after_the_grace() {
        let (_child, pid, line) = spawn_line(
            "trap 'exit 7' TERM; (trap '' TERM; exec sleep 30) & echo $!; while :; do sleep 0.05; done",
        );
        let member: libc::pid_t = line.trim().parse().expect("the member's pid");
        std::thread::sleep(Duration::from_millis(100)); // let the subshell set its trap
        let t0 = Instant::now();
        let status = end_group(pid, QUICK).expect("the leader exited");
        assert_eq!(status.code(), Some(7), "{status:?}");
        assert!(
            t0.elapsed() < Duration::from_millis(250),
            "waited out the grace"
        );
        assert!(
            !gone(member),
            "the member was killed before the grace ran out"
        );
        let until = Instant::now() + Duration::from_secs(5);
        while !(gone(member) && gone(pid)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(gone(member), "the member that ignores SIGTERM survived");
        assert!(gone(pid), "the leader was never reaped");
        assert!(
            t0.elapsed() >= Duration::from_millis(250),
            "killed before the grace"
        );
    }

    /// #1787: a leader that ignores SIGTERM is SIGKILLed with its group, and its status says so.
    #[test]
    fn ending_a_group_whose_leader_ignores_sigterm_reports_the_kill() {
        let (_child, pid) = spawn_ready("trap '' TERM; echo READY; sleep 30");
        let status = end_group(pid, QUICK).expect("reaped");
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
        assert!(gone(pid));
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

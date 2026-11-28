//! Child processes that jobs run, so that Quit can stop them.

use std::collections::HashSet;
use std::io;
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tracing::warn;

/// The running child processes of all jobs; cheap to clone.
///
/// A process is in the set from its start until it is reaped. Reaping and
/// removing happen under one lock, and so does [`kill_all`](Self::kill_all),
/// so a signal never reaches a process that took over the id of a reaped
/// child.
#[derive(Debug, Clone, Default)]
pub struct Children(Arc<Mutex<HashSet<u32>>>);

impl Children {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends `SIGKILL` to every child that runs; returns how many were sent.
    ///
    /// The children are reaped by the jobs that started them.
    pub fn kill_all(&self) -> usize {
        let pids = self.lock();
        let mut killed = 0;
        for &pid in pids.iter() {
            let Ok(pid) = libc::pid_t::try_from(pid) else {
                continue;
            };
            // SAFETY: plain system call; `pid` is a child that is not
            // reaped yet, so the id is still its own.
            if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
                killed += 1;
            } else {
                warn!(pid, error = %io::Error::last_os_error(), "couldn't kill a child process");
            }
        }
        killed
    }

    /// Whether no child runs.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Keeps `child` in the set until the returned handle reaps it.
    pub(crate) fn track(&self, child: Child) -> Tracked<'_> {
        self.lock().insert(child.id());
        Tracked {
            child,
            children: self,
            reaped: false,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<u32>> {
        // The set stays consistent: every change is one insert or remove.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A child process in [`Children`]. Dropped unreaped, it is killed and
/// reaped.
#[derive(Debug)]
pub(crate) struct Tracked<'a> {
    child: Child,
    children: &'a Children,
    reaped: bool,
}

impl Tracked<'_> {
    /// The exit status if the child has ended; then it is reaped and leaves
    /// the set.
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut pids = self.children.lock();
        let status = self.child.try_wait()?;
        if status.is_some() {
            pids.remove(&self.child.id());
            self.reaped = true;
        }
        Ok(status)
    }

    /// Kills the child if it still runs and reaps it.
    pub(crate) fn kill(&mut self) -> io::Result<ExitStatus> {
        let mut pids = self.children.lock();
        // An ended but unreaped child takes the signal without harm.
        if let Err(error) = self.child.kill() {
            warn!(%error, pid = self.child.id(), "couldn't kill a child process");
        }
        let status = self.child.wait();
        pids.remove(&self.child.id());
        self.reaped = true;
        status
    }
}

impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    fn sleeper() -> Child {
        Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap_or_else(|error| panic!("sleep: {error}"))
    }

    fn wait_for_end(tracked: &mut Tracked<'_>) -> ExitStatus {
        let start = Instant::now();
        loop {
            match tracked.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) => {}
                Err(error) => panic!("try_wait: {error}"),
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the child runs on"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_child_is_in_the_set_until_it_is_reaped() {
        let children = Children::new();
        let mut tracked = children.track(sleeper());
        assert!(!children.is_empty());
        assert_eq!(children.kill_all(), 1);
        let status = wait_for_end(&mut tracked);
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(children.is_empty());
        // Nothing is left to kill.
        assert_eq!(children.kill_all(), 0);
    }

    #[test]
    fn a_dropped_child_is_killed_and_reaped() {
        let children = Children::new();
        let tracked = children.track(sleeper());
        let pid = libc::pid_t::try_from(tracked.child.id()).unwrap_or(libc::pid_t::MAX);
        drop(tracked);
        assert!(children.is_empty());
        // SAFETY: signal 0 only checks that the process exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "the child still exists");
    }
}

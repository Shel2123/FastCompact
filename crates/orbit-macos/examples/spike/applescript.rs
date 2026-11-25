//! Diagnostic cross-check: asks Finder for its selection through JXA and
//! compares it with the AX read. Never a source of the target: Finder's
//! `selection` describes the front window, while the target is defined by
//! the focused file area.
//!
//! Runs on its own thread, one check at a time. A check that arrives while
//! the previous one is still running is skipped, so neither the AX result
//! nor later gestures wait for it.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use objc2_foundation::{NSArray, NSData, NSJSONReadingOptions, NSJSONSerialization, NSString};
use tracing::{error, info, warn};

/// Prints the selection as a JSON array of POSIX paths, so names with line
/// breaks or quotes stay unambiguous.
const SCRIPT: &str = "ObjC.import('Foundation'); \
    JSON.stringify(Application('Finder').selection().map(function (item) { \
        return $.NSURL.URLWithString(item.url()).path.js; \
    }))";
const TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);

pub struct Check {
    pub session: u64,
    /// Paths from the AX read, or `None` when the read gave no complete
    /// selection; the result is then only logged.
    pub ax: Option<Vec<PathBuf>>,
}

pub struct Checker {
    sender: mpsc::SyncSender<Check>,
}

impl Checker {
    pub fn spawn() -> Option<Self> {
        // Zero capacity: a check is accepted only while the thread is idle.
        let (sender, receiver) = mpsc::sync_channel::<Check>(0);
        let spawned = thread::Builder::new()
            .name("orbit-spike-applescript".into())
            .spawn(move || {
                for check in receiver {
                    run_check(&check);
                }
            });
        match spawned {
            Ok(_) => Some(Self { sender }),
            Err(err) => {
                error!(%err, "applescript thread not started");
                None
            }
        }
    }

    pub fn submit(&self, check: Check) {
        match self.sender.try_send(check) {
            Ok(()) => {}
            Err(TrySendError::Full(check)) => {
                info!(session = check.session, "applescript busy; check skipped");
            }
            Err(TrySendError::Disconnected(check)) => {
                warn!(session = check.session, "applescript thread is gone");
            }
        }
    }
}

enum Outcome {
    Ok(Vec<PathBuf>),
    /// Automation access to Finder was refused (error -1743).
    Denied(String),
    Timeout,
    Error(String),
}

fn run_check(check: &Check) {
    let session = check.session;
    let started = Instant::now();
    let outcome = run_jxa();
    let elapsed = started.elapsed();
    match outcome {
        Outcome::Ok(paths) => {
            let matches = check.ax.as_deref().map(|ax| same_set(ax, &paths));
            info!(session, ?elapsed, ?paths, ax = ?check.ax, ?matches, "applescript match");
        }
        Outcome::Denied(stderr) => {
            warn!(session, ?elapsed, %stderr, "applescript denied (Automation)");
        }
        Outcome::Timeout => warn!(session, ?elapsed, "applescript timeout; osascript killed"),
        Outcome::Error(detail) => warn!(session, ?elapsed, %detail, "applescript error"),
    }
}

/// Same number of paths and the same set, in any order.
fn same_set(ax: &[PathBuf], script: &[PathBuf]) -> bool {
    ax.len() == script.len()
        && ax.iter().collect::<BTreeSet<_>>() == script.iter().collect::<BTreeSet<_>>()
}

fn run_jxa() -> Outcome {
    let spawned = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", SCRIPT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => return Outcome::Error(format!("osascript not started: {err}")),
    };
    // Drain both pipes on their own threads so a large output cannot block
    // the child while we wait for it.
    let stdout = child.stdout.take().and_then(drain);
    let stderr = child.stderr.take().and_then(drain);

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                if let Err(err) = child.kill() {
                    warn!(%err, "osascript not killed");
                }
                if let Err(err) = child.wait() {
                    warn!(%err, "killed osascript not reaped");
                }
                return Outcome::Timeout;
            }
            Ok(None) => thread::sleep(POLL),
            Err(err) => return Outcome::Error(format!("osascript wait failed: {err}")),
        }
    };
    let stdout = collect(stdout);
    let stderr = String::from_utf8_lossy(&collect(stderr)).trim().to_owned();
    if status.success() {
        parse(&stdout)
    } else if stderr.contains("-1743") {
        Outcome::Denied(stderr)
    } else {
        Outcome::Error(format!("{status}: {stderr}"))
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> Option<JoinHandle<Vec<u8>>> {
    let spawned = thread::Builder::new()
        .name("orbit-spike-osascript-pipe".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            if let Err(err) = pipe.read_to_end(&mut bytes) {
                warn!(%err, "osascript pipe read failed");
            }
            bytes
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(err) => {
            warn!(%err, "osascript pipe thread not started");
            None
        }
    }
}

fn collect(handle: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

fn parse(stdout: &[u8]) -> Outcome {
    let data = NSData::with_bytes(stdout.trim_ascii());
    let value = match NSJSONSerialization::JSONObjectWithData_options_error(
        &data,
        NSJSONReadingOptions::empty(),
    ) {
        Ok(value) => value,
        Err(err) => return Outcome::Error(format!("bad JSON: {}", err.localizedDescription())),
    };
    let Ok(array) = value.downcast::<NSArray>() else {
        return Outcome::Error("JSON is not an array".to_owned());
    };
    let mut paths = Vec::with_capacity(array.count());
    for item in array.iter() {
        match item.downcast_ref::<NSString>() {
            Some(path) => paths.push(PathBuf::from(path.to_string())),
            None => return Outcome::Error("JSON array holds a non-string".to_owned()),
        }
    }
    Outcome::Ok(paths)
}

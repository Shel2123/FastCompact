//! Application delegate: Quit while jobs run.
//!
//! With jobs, Quit cancels all of them and waits, in
//! `NSModalPanelRunLoopMode`, for the last result, but no longer than
//! [`QUIT_GRACE`]. At the deadline the child processes of the jobs are
//! killed and the application quits anyway: the temporary file of a stuck
//! job may stay behind.

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationTerminateReply};
use tracing::{info, warn};

use super::jobs::{Jobs, QUIT_GRACE, QuitEnd, QuitReply};
use crate::operations::children::Children;

/// What the delegate needs.
#[derive(Debug)]
pub struct Ivars {
    jobs: Jobs,
    children: Children,
}

define_class!(
    /// The delegate of `NSApp`. `NSApp` keeps its delegate weakly: keep the
    /// returned value alive while the application runs.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OrbitAppDelegate"]
    #[ivars = Ivars]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            self.terminate_reply()
        }
    }
);

impl AppDelegate {
    /// A delegate that waits for `jobs` on Quit and kills `children` at the
    /// deadline.
    pub fn new(mtm: MainThreadMarker, jobs: Jobs, children: Children) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { jobs, children });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn terminate_reply(&self) -> NSApplicationTerminateReply {
        let mtm = self.mtm();
        let children = self.ivars().children.clone();
        let reply = self.ivars().jobs.quit(QUIT_GRACE, move |end| {
            if end == QuitEnd::Deadline {
                let killed = children.kill_all();
                warn!(killed, "quitting with jobs still running");
            } else {
                info!("the jobs ended; quitting");
            }
            NSApplication::sharedApplication(mtm).replyToApplicationShouldTerminate(true);
        });
        match reply {
            QuitReply::Now => NSApplicationTerminateReply::TerminateNow,
            QuitReply::Later => NSApplicationTerminateReply::TerminateLater,
        }
    }
}

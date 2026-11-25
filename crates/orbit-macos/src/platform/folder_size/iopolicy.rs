//! Keeping the walk from downloading iCloud content.
//!
//! An iCloud Drive folder can be dataless: its list of entries is not on the
//! disk, and reading it makes the system fetch the list from the network.
//! With this policy on, the reads of the calling thread fail on such an
//! object instead, and the walk counts it as unreadable.

use std::io;

use libc::c_int;
use tracing::warn;

// From `sys/resource.h`; libc does not have them.
const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
const IOPOL_SCOPE_THREAD: c_int = 1;
const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;

unsafe extern "C" {
    fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
}

/// Turns off fetching dataless content for the calling thread. If that
/// fails, only the dataless flag checked by the walk protects it.
pub(super) fn leave_dataless_alone() {
    // SAFETY: the call takes three integers and changes only the policy of
    // the calling thread.
    let status = unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_THREAD,
            IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
        )
    };
    if status != 0 {
        warn!(
            err = %io::Error::last_os_error(),
            "the folder walk may download iCloud content"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    unsafe extern "C" {
        fn getiopolicy_np(iotype: c_int, scope: c_int) -> c_int;
    }

    fn policy() -> c_int {
        // SAFETY: the call takes two integers and only reads the policy.
        unsafe {
            getiopolicy_np(
                IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                IOPOL_SCOPE_THREAD,
            )
        }
    }

    #[test]
    fn the_policy_holds_for_the_calling_thread_only() {
        let (inside, outside) = thread::scope(|scope| {
            let walker = scope.spawn(|| {
                leave_dataless_alone();
                policy()
            });
            let other = scope.spawn(policy);
            (walker.join(), other.join())
        });
        assert_eq!(inside.ok(), Some(IOPOL_MATERIALIZE_DATALESS_FILES_OFF));
        assert_ne!(outside.ok(), Some(IOPOL_MATERIALIZE_DATALESS_FILES_OFF));
    }
}

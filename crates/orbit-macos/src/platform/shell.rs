//! The shell of the macOS platform: the clipboard and Reveal in Finder.
//!
//! Each AppKit call runs inside [`objc2::exception::catch`]: the pasteboard
//! reports some failures by raising `NSPasteboardCommunicationException`
//! instead of returning `false`, and an Objective-C exception that reached
//! Rust code would end the process. A caught exception becomes a
//! [`ShellError`] like any other failure.
//!
//! Nothing here touches the filesystem: whether the file still exists is
//! for Finder to tell.

use std::panic::AssertUnwindSafe;
use std::path::Path;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString, NSWorkspace};
use objc2_foundation::NSString;
use orbit_app::ports::{Shell, ShellError};
use tracing::debug;

/// The [`Shell`] of the macOS platform. Lives on the main thread.
#[derive(Debug)]
pub struct MacShell {
    _main_thread: MainThreadMarker,
}

impl MacShell {
    /// The shell for the main thread.
    pub const fn new(mtm: MainThreadMarker) -> Self {
        Self { _main_thread: mtm }
    }
}

impl Shell for MacShell {
    /// Puts the path on the general pasteboard as plain text, replacing what
    /// was there. Only text: a file URL would make Finder paste the file
    /// itself.
    fn write_clipboard(&mut self, path: &Path) -> Result<(), ShellError> {
        let text = NSString::from_str(path_text(path)?);
        let written = guarded(|| {
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            // SAFETY: an AppKit constant, valid for the program's lifetime.
            let string_type = unsafe { NSPasteboardTypeString };
            pasteboard.setString_forType(&text, string_type)
        })?;
        if written {
            debug!(path = %path.display(), "path copied");
            Ok(())
        } else {
            Err(ShellError("the clipboard did not take the path".into()))
        }
    }

    /// Asks Finder to select the object in a window; Finder becomes active.
    /// A symbolic link is selected itself, not what it points to.
    fn reveal(&mut self, path: &Path) -> Result<(), ShellError> {
        let text = NSString::from_str(path_text(path)?);
        let shown = guarded(|| {
            // An empty root: Finder picks the window.
            NSWorkspace::sharedWorkspace()
                .selectFile_inFileViewerRootedAtPath(Some(&text), &NSString::new())
        })?;
        if shown {
            debug!(path = %path.display(), "revealed");
            Ok(())
        } else {
            Err(ShellError("Finder could not find the item".into()))
        }
    }
}

/// The path as text for AppKit.
///
/// # Errors
///
/// [`ShellError`] when the path is not valid UTF-8: a lossy copy would name
/// another file.
fn path_text(path: &Path) -> Result<&str, ShellError> {
    path.to_str()
        .ok_or_else(|| ShellError("the path is not valid UTF-8".into()))
}

/// Runs an AppKit call and turns an Objective-C exception into an error.
fn guarded<R>(call: impl FnOnce() -> R) -> Result<R, ShellError> {
    objc2::exception::catch(AssertUnwindSafe(call)).map_err(|exception| {
        ShellError(match exception {
            Some(exception) => format!("the system raised an exception: {exception}"),
            None => "the system raised an exception".into(),
        })
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    use objc2_foundation::NSArray;

    use super::*;

    #[test]
    fn utf8_paths_are_used_as_they_are() {
        let path = Path::new("/Users/me/Мои документы/a b.txt");
        assert_eq!(path_text(path), Ok("/Users/me/Мои документы/a b.txt"));
    }

    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        let path = Path::new(OsStr::from_bytes(b"/tmp/\xff.txt"));
        assert_eq!(
            path_text(path),
            Err(ShellError("the path is not valid UTF-8".into()))
        );
    }

    #[test]
    fn an_objective_c_exception_becomes_an_error() {
        let empty = NSArray::<NSString>::new();
        // Out of range: NSArray raises NSRangeException.
        let result = guarded(|| empty.objectAtIndex(3).to_string());
        let Err(ShellError(message)) = result else {
            panic!("no error: {result:?}");
        };
        assert!(
            message.starts_with("the system raised an exception"),
            "{message}"
        );
    }

    #[test]
    fn a_call_without_an_exception_returns_its_value() {
        assert_eq!(guarded(|| 7), Ok(7));
    }
}

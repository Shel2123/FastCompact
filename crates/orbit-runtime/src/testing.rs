//! Fakes shared by the runtime tests.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crossbeam_channel::{Receiver, Sender};
use orbit_app::catalog::MenuModel;
use orbit_app::messages::{
    AppEvent, GestureId, JobId, Notice, ParameterError, ParametersModel, Progress,
};
use orbit_app::ports::{EventSink, Feedback, MenuView, ParametersView, Shell, ShellError};
use orbit_app::state::Highlight;
use orbit_domain::target::{FileFacts, FileIdentity, FileKind, FileTarget, FileVersion};

use crate::platform::Views;

/// Longest wait for an event before a test fails.
const TIMEOUT: Duration = Duration::from_secs(5);

/// An [`EventSink`] that the test reads in order.
pub struct ChannelSink {
    sender: Sender<AppEvent>,
    receiver: Receiver<AppEvent>,
}

impl ChannelSink {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self { sender, receiver }
    }

    pub fn shared(&self) -> Arc<dyn EventSink> {
        Arc::new(SenderSink(self.sender.clone()))
    }

    /// The next event; fails the test after [`TIMEOUT`].
    pub fn next(&self) -> AppEvent {
        match self.receiver.recv_timeout(TIMEOUT) {
            Ok(event) => event,
            Err(err) => panic!("no event: {err}"),
        }
    }

    pub fn assert_empty(&self) {
        if let Ok(event) = self.receiver.try_recv() {
            panic!("unexpected event: {event:?}");
        }
    }
}

struct SenderSink(Sender<AppEvent>);

impl EventSink for SenderSink {
    fn send(&self, event: AppEvent) {
        // The test may have stopped reading; nothing to do then.
        let _ = self.0.send(event);
    }
}

/// Holds a fake inside its call until the test opens it.
#[derive(Clone)]
pub struct Gate {
    entered: (Sender<()>, Receiver<()>),
    open: (Sender<()>, Receiver<()>),
}

impl Gate {
    pub fn new() -> Self {
        Self {
            entered: crossbeam_channel::unbounded(),
            open: crossbeam_channel::unbounded(),
        }
    }

    /// Called by the fake: tells the test it entered, then waits.
    pub fn pass(&self) {
        let _ = self.entered.0.send(());
        if self.open.1.recv_timeout(TIMEOUT).is_err() {
            panic!("gate was not opened");
        }
    }

    /// Waits until the fake is inside its call.
    pub fn wait_entered(&self) {
        if self.entered.1.recv_timeout(TIMEOUT).is_err() {
            panic!("fake did not enter the gate");
        }
    }

    /// Lets one waiting call go on.
    pub fn open(&self) {
        let _ = self.open.0.send(());
    }
}

/// A plain file target at `path`.
pub fn plain(path: &str) -> FileTarget {
    FileTarget {
        path: PathBuf::from(path),
        identity: FileIdentity {
            device: 1,
            inode: 42,
        },
        version: FileVersion {
            size: 1024,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
        },
        facts: FileFacts {
            kind: FileKind::File,
            image: None,
        },
    }
}

/// One call of a view, feedback or shell port.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    MenuPresent(MenuModel),
    MenuHighlight(Highlight),
    MenuHide,
    FormPresent(ParametersModel),
    FormInvalid(GestureId, ParameterError),
    FormHide,
    JobProgress(JobId, Progress),
    Notify(Notice),
    ShowInfo(FileTarget),
    WriteClipboard(PathBuf),
    Reveal(PathBuf),
}

/// The calls of all fake ports in the order they happened.
#[derive(Clone, Default)]
pub struct Journal(Rc<RefCell<Vec<Call>>>);

impl Journal {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&self, call: Call) {
        self.0.borrow_mut().push(call);
    }

    /// Everything recorded so far; the journal starts again empty.
    pub fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

/// Fake ports that record into a [`Journal`]. The shell fails with the
/// given message when `shell_error` is set.
pub fn views(journal: &Journal, shell_error: Option<&str>) -> Views {
    Views {
        menu: Box::new(Fake(journal.clone(), None)),
        parameters: Box::new(Fake(journal.clone(), None)),
        feedback: Box::new(Fake(journal.clone(), None)),
        shell: Box::new(Fake(journal.clone(), shell_error.map(str::to_owned))),
    }
}

/// Implements every port by recording the call.
struct Fake(Journal, Option<String>);

impl MenuView for Fake {
    fn present(&mut self, model: &MenuModel) {
        self.0.push(Call::MenuPresent(model.clone()));
    }

    fn highlight(&mut self, highlight: Highlight) {
        self.0.push(Call::MenuHighlight(highlight));
    }

    fn hide(&mut self) {
        self.0.push(Call::MenuHide);
    }
}

impl ParametersView for Fake {
    fn present(&mut self, model: &ParametersModel) {
        self.0.push(Call::FormPresent(model.clone()));
    }

    fn show_invalid(&mut self, gesture: GestureId, error: ParameterError) {
        self.0.push(Call::FormInvalid(gesture, error));
    }

    fn hide(&mut self) {
        self.0.push(Call::FormHide);
    }
}

impl Feedback for Fake {
    fn job_progress(&mut self, job: JobId, progress: Progress) {
        self.0.push(Call::JobProgress(job, progress));
    }

    fn notify(&mut self, notice: &Notice) {
        self.0.push(Call::Notify(notice.clone()));
    }

    fn show_info(&mut self, target: &FileTarget) {
        self.0.push(Call::ShowInfo(target.clone()));
    }
}

impl Shell for Fake {
    fn write_clipboard(&mut self, path: &Path) -> Result<(), ShellError> {
        self.0.push(Call::WriteClipboard(path.to_owned()));
        self.1
            .clone()
            .map_or(Ok(()), |message| Err(ShellError(message)))
    }

    fn reveal(&mut self, path: &Path) -> Result<(), ShellError> {
        self.0.push(Call::Reveal(path.to_owned()));
        self.1
            .clone()
            .map_or(Ok(()), |message| Err(ShellError(message)))
    }
}

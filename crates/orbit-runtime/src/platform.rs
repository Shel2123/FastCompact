//! What a platform supplies to the runtime.

use std::error::Error;
use std::sync::Arc;

use orbit_app::messages::AppEvent;
use orbit_app::ports::{
    EventSink, Feedback, MenuView, OperationExecutor, ParametersView, Shell, TargetResolver,
};
use orbit_domain::operation::FileNamePolicy;

/// Takes the events of the controller on the main thread, one at a time.
/// Not `Send`: it lives where the event loop runs.
pub type Handler = Box<dyn FnMut(AppEvent)>;

/// A failure of the platform that the runtime cannot recover from.
pub type PlatformError = Box<dyn Error + Send + Sync>;

/// Makes a [`TargetResolver`] on the thread that will use it, so the
/// resolver itself does not have to be `Send`.
pub struct ResolverFactory(Box<dyn FnOnce() -> Box<dyn TargetResolver> + Send>);

impl ResolverFactory {
    /// A factory that calls `make` on the resolver thread.
    pub fn new<R>(make: impl FnOnce() -> R + Send + 'static) -> Self
    where
        R: TargetResolver + 'static,
    {
        Self(Box::new(move || Box::new(make())))
    }

    /// Makes the resolver. Must run on the thread that will call it.
    pub(crate) fn make(self) -> Box<dyn TargetResolver> {
        (self.0)()
    }
}

/// The ports that show things and act on the system. Owned by the runtime
/// and called on the main thread; see the contract in
/// [`orbit_app::ports`].
pub struct Views {
    /// The radial menu.
    pub menu: Box<dyn MenuView>,
    /// The parameters form.
    pub parameters: Box<dyn ParametersView>,
    /// Progress, notices and information.
    pub feedback: Box<dyn Feedback>,
    /// Clipboard and file manager.
    pub shell: Box<dyn Shell>,
}

/// Everything specific to an operating system.
///
/// The runtime asks for the parts first and then hands over the event loop
/// with [`run`](Self::run). A platform is created before [`run`](crate::run)
/// is called, so anything the parts share, such as an input tap that the
/// resolver reads, is set up in its constructor.
pub trait Platform {
    /// Delivers events to the controller on the main thread. See the
    /// contract of [`EventSink`].
    fn sink(&self) -> Arc<dyn EventSink>;

    /// Makes the target resolver on its own thread.
    fn resolver(&self) -> ResolverFactory;

    /// Runs jobs on worker threads.
    fn executor(&self) -> Arc<dyn OperationExecutor>;

    /// Which names the platform refuses in the Rename form.
    fn file_name_policy(&self) -> FileNamePolicy;

    /// Hands over the views. Called once.
    fn views(&mut self) -> Views;

    /// Runs the application until it quits.
    ///
    /// In this order, and the platform is responsible for it:
    ///
    /// 1. Make `handler` receive what the sink delivers. Events sent before
    ///    this point, from any thread, are kept and delivered first, in
    ///    order.
    /// 2. Start whatever produces the gesture events that must not reach
    ///    the controller before the handler exists, such as the global
    ///    hotkey.
    /// 3. Run the event loop on the calling thread.
    ///
    /// If a step fails, the later steps do not run and the error is
    /// returned.
    ///
    /// # Errors
    ///
    /// [`PlatformError`] when a step cannot be done.
    fn run(self, handler: Handler) -> Result<(), PlatformError>;
}

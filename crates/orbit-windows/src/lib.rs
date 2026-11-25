//! Native Win32 interface and Explorer integration for Orbit.
//!
//! [`WindowsPlatform`] is the [`Platform`] for `orbit_runtime::run`;
//! [`show_error`] and [`show_notice`] report startup problems, since a
//! release build has no console.

#![cfg(windows)]

mod explorer;
mod tray;
mod window;

use std::mem::size_of;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use orbit_app::messages::{
    AppEvent, GestureId, JobError, JobOutput, JobSpec, ParameterError, ParametersModel,
};
use orbit_app::ports::{EventSink, JobContext, OperationExecutor, ParametersView};
use orbit_domain::action::ActionId;
use orbit_domain::geometry::{ScreenPoint, ScreenRect};
use orbit_domain::operation::FileNamePolicy;
use orbit_runtime::{Handler, Platform, PlatformError, ResolverFactory, Views};
use tracing::{error, warn};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
    VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_MENU, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Error, HSTRING, Result, w};

use explorer::{Apartment, ExplorerFocus, ExplorerResolver, ExplorerShell, Selection};
use tray::Tray;
use window::{Popup, RadialMenu, Window};

const WAKE: u32 = WM_APP;
const HOTKEY: i32 = 1;

#[derive(Clone)]
struct Sink {
    events: Sender<AppEvent>,
    thread: u32,
}

impl EventSink for Sink {
    fn send(&self, event: AppEvent) {
        if self.events.send(event).is_ok() {
            // SAFETY: The UI thread creates its Win32 message queue before
            // this sink is shared. A full queue already has messages to wake it.
            unsafe {
                let _ = PostThreadMessageW(self.thread, WAKE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Orbit on Windows: Explorer as the file manager, a thread hotkey for the
/// gesture, Win32 windows for the menu and feedback, and a notification
/// area icon to exit.
///
/// Must be made, run and dropped on one thread, which becomes the UI
/// thread.
pub struct WindowsPlatform {
    menu: Option<RadialMenu>,
    popup: Popup,
    selection: Selection,
    sink: Sink,
    events: Receiver<AppEvent>,
    scale: f64,
    // Fields drop in order: COM stays initialized until the windows and the
    // shell objects above are gone.
    _com: Apartment,
}

impl WindowsPlatform {
    /// Initializes COM for this thread and creates the hidden windows.
    ///
    /// # Errors
    ///
    /// When COM cannot be initialized or a window cannot be created.
    pub fn new() -> std::result::Result<Self, PlatformError> {
        let com = Apartment::initialize()?;
        Ok(Self::with_apartment(com)?)
    }

    fn with_apartment(com: Apartment) -> Result<Self> {
        // SAFETY: Startup runs on the thread that will own both windows.
        let (instance, thread, scale) = unsafe {
            let _ = SetProcessDPIAware();
            let _ = PeekMessageW(&mut MSG::default(), None, 0, 0, PM_NOREMOVE);
            (
                HINSTANCE(GetModuleHandleW(None)?.0),
                GetCurrentThreadId(),
                f64::from(GetDpiForSystem().max(96)) / 96.0,
            )
        };
        let (sender, events) = mpsc::channel();
        let sink = Sink {
            events: sender,
            thread,
        };
        Window::register(instance)?;
        let menu = RadialMenu(Window::new(instance, scale)?, sink.clone());
        let popup = Popup(Rc::new(Window::new(instance, scale)?));
        Ok(Self {
            menu: Some(menu),
            popup,
            selection: Selection::default(),
            sink,
            events,
            scale,
            _com: com,
        })
    }

    fn loop_messages(&self, mut handler: Handler, tray: &Tray) -> Result<()> {
        let mut active: Option<Gesture> = None;
        let mut sequence = 0_u64;
        let mut message = MSG::default();
        loop {
            // SAFETY: The message buffer is valid and processed on its owning thread.
            match unsafe { GetMessageW(&mut message, None, 0, 0) }.0 {
                0 => return Ok(()),
                -1 => return Err(Error::from_thread()),
                _ => {}
            }
            match message.message {
                WAKE if message.hwnd.is_invalid() => {}
                WM_HOTKEY if message.wParam.0 == HOTKEY as usize && active.is_none() => {
                    if held(VK_SPACE)
                        && held(VK_CONTROL)
                        && held(VK_MENU)
                        && let Some(focus) = ExplorerFocus::capture()
                        && let Ok(position) = cursor()
                        && let Some(area) = work_area(position)
                    {
                        sequence = sequence.wrapping_add(1);
                        let start = GestureStart {
                            focus,
                            position,
                            area,
                            scale: self.scale,
                        };
                        // A thread timer runs only while the hotkey is held.
                        // SAFETY: The UI thread owns the timer and receives its messages.
                        let set_timer = || unsafe { SetTimer(None, 0, POLL_MS, None) };
                        active = begin_gesture(
                            GestureId(sequence),
                            start,
                            &self.selection,
                            &self.sink,
                            set_timer,
                        );
                        if active.is_some() {
                            self.popup.hide();
                        }
                    }
                }
                WM_TIMER
                    if message.hwnd.is_invalid()
                        && active
                            .as_ref()
                            .is_some_and(|gesture| gesture.timer == message.wParam.0) =>
                {
                    if let Some(gesture) = active.as_mut()
                        && !gesture.poll(&self.sink, self.scale)
                    {
                        active = None;
                    }
                }
                _ => {
                    // SAFETY: The message came from this thread's Win32 queue.
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            }
            self.drain(&mut handler);
            if self.serve_tray(tray, &mut active, &mut handler) {
                return Ok(());
            }
        }
    }

    /// Handles what the tray window recorded, once and outside any nested
    /// message loop. Returns whether Exit was chosen.
    fn serve_tray(&self, tray: &Tray, active: &mut Option<Gesture>, handler: &mut Handler) -> bool {
        let (click, restore) = self.popup.take_tray_requests();
        if restore && let Err(error) = tray.add() {
            warn!(%error, "could not restore the Orbit tray icon");
        }
        if click.is_none() {
            return false;
        }
        if let Some(gesture) = active.take() {
            gesture.cancel(&self.sink);
            self.drain(handler);
        }
        let exit = tray.wants_exit().unwrap_or_else(|error| {
            warn!(%error, "could not show the Orbit tray menu");
            false
        });
        // A click on the icon while the menu was open belonged to the menu.
        self.popup.discard_tray_click();
        exit
    }

    fn drain(&self, handler: &mut Handler) {
        while let Ok(event) = self.events.try_recv() {
            handler(event);
        }
    }
}

impl Platform for WindowsPlatform {
    fn sink(&self) -> Arc<dyn EventSink> {
        Arc::new(self.sink.clone())
    }

    fn resolver(&self) -> ResolverFactory {
        let selection = Arc::clone(&self.selection);
        ResolverFactory::new(move || ExplorerResolver(selection))
    }

    fn executor(&self) -> Arc<dyn OperationExecutor> {
        Arc::new(InstantActions)
    }

    fn file_name_policy(&self) -> FileNamePolicy {
        FileNamePolicy::UNIVERSAL
    }

    fn supported_actions(&self) -> &'static [ActionId] {
        &[ActionId::CopyPath, ActionId::ShowInfo, ActionId::Reveal]
    }

    fn views(&mut self) -> Views {
        Views {
            // Platform::views is called once by orbit_runtime::run.
            menu: Box::new(
                self.menu
                    .take()
                    .unwrap_or_else(|| unreachable!("views are taken once")),
            ),
            parameters: Box::new(InstantActions),
            feedback: Box::new(self.popup.clone()),
            shell: Box::new(ExplorerShell(self.popup.0.handle)),
        }
    }

    fn run(self, handler: Handler) -> std::result::Result<(), PlatformError> {
        // SAFETY: A thread hotkey belongs to this force-created message queue.
        unsafe {
            RegisterHotKey(
                None,
                HOTKEY,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                u32::from(VK_SPACE.0),
            )
        }
        .map_err(|error| format!("Could not register Ctrl+Alt+Space. {error}"))?;
        let result =
            Tray::install(self.popup.0.handle).and_then(|tray| self.loop_messages(handler, &tray));
        // SAFETY: This thread registered the hotkey above.
        unsafe {
            let _ = UnregisterHotKey(None, HOTKEY);
        }
        result.map_err(Into::into)
    }
}

struct InstantActions;

impl ParametersView for InstantActions {
    fn present(&mut self, _: &ParametersModel) {}

    fn show_invalid(&mut self, _: GestureId, _: ParameterError) {}

    fn hide(&mut self) {}
}

impl OperationExecutor for InstantActions {
    fn execute(&self, _: &JobSpec, _: &JobContext) -> std::result::Result<JobOutput, JobError> {
        Err(JobError::Internal(
            "This action is unavailable on Windows.".into(),
        ))
    }
}

/// How often a held gesture reads the pointer and the keys.
const POLL_MS: u32 = 12;

/// What the hotkey press found.
#[derive(Clone, Copy)]
struct GestureStart {
    focus: ExplorerFocus,
    position: POINT,
    area: RECT,
    scale: f64,
}

/// Starts gesture `id` with the timer that `set_timer` makes (0 when it
/// fails). The timer is made before anything is published, so a failure
/// drops only this press: it is logged, the selection is cleared, nothing
/// is sent, and the next press starts afresh.
fn begin_gesture(
    id: GestureId,
    start: GestureStart,
    selection: &Selection,
    sink: &Sink,
    set_timer: impl FnOnce() -> usize,
) -> Option<Gesture> {
    let timer = set_timer();
    if let Ok(mut selection) = selection.lock() {
        *selection = (timer != 0).then_some((id, start.focus));
    }
    if timer == 0 {
        error!(
            error = %Error::from_thread(),
            ?id,
            "could not start the gesture timer; the press is dropped"
        );
        return None;
    }
    let point = screen_point(start.position, start.scale);
    sink.send(AppEvent::Invoked {
        gesture: id,
        point,
        screens: vec![screen_rect(start.area, start.scale)],
    });
    Some(Gesture {
        id,
        point,
        focus: start.focus,
        timer,
    })
}

struct Gesture {
    id: GestureId,
    point: ScreenPoint,
    focus: ExplorerFocus,
    timer: usize,
}

impl Gesture {
    fn cancel(&self, sink: &Sink) {
        sink.send(AppEvent::GestureCancelled { gesture: self.id });
        // A cancelled gesture remains held in the controller until Released.
        sink.send(AppEvent::Released {
            gesture: self.id,
            point: self.point,
        });
    }

    fn poll(&mut self, sink: &Sink, scale: f64) -> bool {
        let Ok(point) = cursor() else {
            self.cancel(sink);
            return false;
        };
        let point = screen_point(point, scale);
        if held(VK_ESCAPE) || ExplorerFocus::capture() != Some(self.focus) {
            self.cancel(sink);
            return false;
        }
        if !(held(VK_SPACE) && held(VK_CONTROL) && held(VK_MENU)) {
            sink.send(AppEvent::Released {
                gesture: self.id,
                point,
            });
            return false;
        }
        if point != self.point {
            self.point = point;
            sink.send(AppEvent::PointerMoved {
                gesture: self.id,
                point,
            });
        }
        true
    }
}

impl Drop for Gesture {
    fn drop(&mut self) {
        // SAFETY: A gesture is dropped on the UI thread that set the timer.
        unsafe {
            let _ = KillTimer(None, self.timer);
        }
    }
}

fn held(key: VIRTUAL_KEY) -> bool {
    // SAFETY: GetAsyncKeyState accepts a virtual key code without pointers.
    unsafe { GetAsyncKeyState(i32::from(key.0)) < 0 }
}

fn cursor() -> Result<POINT> {
    let mut point = POINT::default();
    // SAFETY: point is a writable POINT.
    unsafe { GetCursorPos(&mut point)? };
    Ok(point)
}

fn work_area(point: POINT) -> Option<RECT> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: The info buffer is initialized to the required size.
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(monitor, &mut info)
            .as_bool()
            .then_some(info.rcWork)
    }
}

fn screen_point(point: POINT, scale: f64) -> ScreenPoint {
    ScreenPoint::new(f64::from(point.x) / scale, -f64::from(point.y) / scale)
}

fn screen_rect(area: RECT, scale: f64) -> ScreenRect {
    ScreenRect::new(
        f64::from(area.left) / scale,
        -f64::from(area.bottom) / scale,
        f64::from(area.right - area.left) / scale,
        f64::from(area.bottom - area.top) / scale,
    )
}

/// Shows `message` in a modal error box.
pub fn show_error(message: &str) {
    message_box(message, MB_ICONERROR);
}

/// Shows `message` in a modal information box.
pub fn show_notice(message: &str) {
    message_box(message, MB_ICONINFORMATION);
}

fn message_box(message: &str, icon: MESSAGEBOX_STYLE) {
    let message = HSTRING::from(message);
    // SAFETY: The text and title stay valid for the modal call.
    unsafe { MessageBoxW(None, &message, w!("Orbit"), MB_OK | icon) };
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: GestureStart = GestureStart {
        focus: ExplorerFocus::fake(10, 11),
        position: POINT { x: 500, y: 300 },
        area: RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        },
        scale: 1.0,
    };

    fn sink() -> (Sink, Receiver<AppEvent>) {
        let (events, received) = mpsc::channel();
        // SAFETY: Reads the id of the calling thread.
        let thread = unsafe { GetCurrentThreadId() };
        (Sink { events, thread }, received)
    }

    fn selected(selection: &Selection) -> Option<(GestureId, ExplorerFocus)> {
        match selection.lock() {
            Ok(selection) => *selection,
            Err(_) => panic!("selection lock poisoned"),
        }
    }

    #[test]
    fn a_failed_timer_drops_only_that_press() {
        let (sink, events) = sink();
        let selection = Selection::default();
        if let Ok(mut stale) = selection.lock() {
            *stale = Some((GestureId(1), START.focus));
        }

        let failed = begin_gesture(GestureId(2), START, &selection, &sink, || 0);
        assert!(failed.is_none());
        assert!(events.try_recv().is_err(), "nothing is published");
        assert!(selected(&selection).is_none(), "no stale selection");

        // A timer id that was never set: dropping the gesture kills nothing.
        let next = begin_gesture(GestureId(3), START, &selection, &sink, || 0x7fff);
        assert!(
            next.as_ref()
                .is_some_and(|gesture| gesture.id == GestureId(3))
        );
        assert!(matches!(
            events.try_recv(),
            Ok(AppEvent::Invoked {
                gesture: GestureId(3),
                ..
            })
        ));
        assert!(selected(&selection) == Some((GestureId(3), START.focus)));
    }
}

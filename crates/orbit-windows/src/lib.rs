//! Native Win32 interface and Explorer integration for Orbit.

#![cfg(windows)]

mod explorer;
mod tray;
mod window;

use std::mem::size_of;
use std::process::ExitCode;
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
use orbit_runtime::{Handler, Platform, PlatformError, ResolverFactory, RuntimeConfig, Views};
use tracing::warn;
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
use tray::{TRAY_MESSAGE, TRAY_RESTORE, Tray};
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

struct WindowsPlatform {
    menu: Option<RadialMenu>,
    popup: Popup,
    selection: Selection,
    sink: Sink,
    events: Receiver<AppEvent>,
    scale: f64,
}

impl WindowsPlatform {
    fn new() -> Result<Self> {
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
                        && let Some(area) = work_area(message.pt)
                    {
                        sequence = sequence.wrapping_add(1);
                        let id = GestureId(sequence);
                        // A thread timer runs only while the hotkey is held.
                        // SAFETY: The UI thread owns the timer and receives its messages.
                        let timer = unsafe { SetTimer(None, 0, 12, None) };
                        if timer == 0 {
                            return Err(Error::from_thread());
                        }
                        self.popup.hide();
                        if let Ok(mut selection) = self.selection.lock() {
                            *selection = Some((id, focus));
                        }
                        let point = screen_point(message.pt, self.scale);
                        active = Some(Gesture {
                            id,
                            point,
                            focus,
                            timer,
                        });
                        self.sink.send(AppEvent::Invoked {
                            gesture: id,
                            point,
                            screens: vec![screen_rect(area, self.scale)],
                        });
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
                TRAY_MESSAGE if message.hwnd == self.popup.0.handle => {
                    if let Some(gesture) = active.take() {
                        gesture.cancel(&self.sink);
                        self.drain(&mut handler);
                    }
                    if tray.wants_exit(message.lParam)? {
                        return Ok(());
                    }
                }
                TRAY_RESTORE if message.hwnd == self.popup.0.handle => {
                    if let Err(error) = tray.add() {
                        warn!(%error, "could not restore the Orbit tray icon");
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
        }
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

/// Starts Orbit and runs its Win32 message loop on the calling thread.
pub fn run_app() -> ExitCode {
    let result = (|| -> std::result::Result<(), PlatformError> {
        let _com = Apartment::initialize()?;
        orbit_runtime::run(WindowsPlatform::new()?, RuntimeConfig::default())?;
        Ok(())
    })();
    if let Err(error) = result {
        let message = HSTRING::from(error.to_string());
        // SAFETY: The error text and title are valid for the modal Win32 call.
        unsafe { MessageBoxW(None, &message, w!("Orbit"), MB_OK | MB_ICONERROR) };
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

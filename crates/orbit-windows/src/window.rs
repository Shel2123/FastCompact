//! Non-activating windows for the radial menu and feedback.

use std::cell::{Cell, RefCell};
use std::f64::consts::TAU;
use std::rc::Rc;

use orbit_app::catalog::MenuModel;
use orbit_app::messages::{AppEvent, JobId, Notice, Progress, ResolveError, ShellAction};
use orbit_app::ports::{EventSink, Feedback, MenuView};
use orbit_app::state::Highlight;
use orbit_domain::action::{ActionId, MenuItem};
use orbit_domain::target::{FileKind, FileTarget};
use tracing::warn;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Error, Result, w};

use crate::tray::{TRAY_MESSAGE, is_click};
use crate::{Sink, cursor, monitor_scale, work_area};

const BACKGROUND: COLORREF = rgb(20, 25, 35);
const SECTOR: COLORREF = rgb(34, 43, 57);
const SELECTED: COLORREF = rgb(25, 79, 80);
const BORDER: COLORREF = rgb(59, 71, 91);
const TEXT: COLORREF = rgb(236, 242, 252);
const MUTED: COLORREF = rgb(152, 166, 187);
const CANCEL: COLORREF = rgb(254, 169, 174);
const DISMISS: usize = 1;

const fn rgb(red: u32, green: u32, blue: u32) -> COLORREF {
    COLORREF(red | (green << 8) | (blue << 16))
}

enum Content {
    None,
    Menu(MenuModel, Highlight),
    Feedback { title: String, body: String },
}

struct WindowState {
    /// Pixels per Orbit unit for the current content, set on every show.
    scale: f64,
    /// True while our own SetWindowPos runs: the WM_DPICHANGED it causes
    /// is the expected move to the target monitor.
    positioning: Cell<bool>,
    /// Set when the scale of the shown window changed under it. Shared
    /// with the message loop for the menu.
    dpi_lost: Rc<Cell<bool>>,
    taskbar_created: u32,
    content: Content,
    /// Tray requests seen by the window procedure, taken by the main loop.
    /// The procedure only records them: it also runs inside nested message
    /// loops such as the tray menu itself.
    tray: TrayRequests,
}

#[derive(Default)]
struct TrayRequests {
    click: Cell<Option<LPARAM>>,
    restore: Cell<bool>,
}

pub(super) struct Window {
    pub(super) handle: HWND,
    state: Box<RefCell<WindowState>>,
}

impl Window {
    pub(super) fn register(instance: HINSTANCE) -> Result<()> {
        let class = WNDCLASSW {
            hInstance: instance,
            lpszClassName: w!("Orbit.Surface"),
            lpfnWndProc: Some(window_proc),
            ..Default::default()
        };
        // SAFETY: Class data is valid during registration, and the callback is static.
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(Error::from_thread());
        }
        Ok(())
    }

    pub(super) fn new(instance: HINSTANCE) -> Result<Self> {
        // SAFETY: TaskbarCreated is a stable shell broadcast message name.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        let state = Box::new(RefCell::new(WindowState {
            scale: 1.0,
            positioning: Cell::new(false),
            dpi_lost: Rc::default(),
            taskbar_created,
            content: Content::None,
            tray: TrayRequests::default(),
        }));
        // SAFETY: The boxed state remains at a stable address until after the
        // window is destroyed. This window and its state stay on this thread.
        let handle = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                w!("Orbit.Surface"),
                w!("Orbit"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                Some((&raw const *state).cast()),
            )?
        };
        Ok(Self { handle, state })
    }

    fn hide(&self) {
        // SAFETY: The window belongs to this thread.
        unsafe {
            let _ = ShowWindow(self.handle, SW_HIDE);
        }
    }

    /// The flag the window sets when its scale changes while it is shown.
    pub(super) fn dpi_lost(&self) -> Rc<Cell<bool>> {
        Rc::clone(&self.state.borrow().dpi_lost)
    }

    /// Shows the window at `bounds`, in physical pixels, drawn at `scale`.
    fn show(&self, bounds: RECT, scale: f64) -> Result<()> {
        self.state.borrow_mut().scale = scale;
        self.state.borrow().positioning.set(true);
        // SAFETY: All coordinates are in the same physical desktop space.
        let placed = unsafe {
            SetWindowPos(
                self.handle,
                Some(HWND_TOPMOST),
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
        self.state.borrow().positioning.set(false);
        placed?;
        // SAFETY: The update is requested for our own window.
        unsafe {
            let _ = InvalidateRect(Some(self.handle), None, false);
        }
        Ok(())
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: Destruction is synchronous on the window's owning thread,
        // while self.state is still alive.
        unsafe {
            let _ = DestroyWindow(self.handle);
        }
    }
}

/// The menu window, the sink to cancel a gesture it cannot show, and the
/// scale of the current gesture, fixed by the message loop at the press.
pub(super) struct RadialMenu(pub Window, pub Sink, pub Rc<Cell<f64>>);

impl MenuView for RadialMenu {
    fn present(&mut self, model: &MenuModel) {
        if let Err(error) = self.show(model) {
            warn!(%error, "could not show the radial menu");
            self.hide();
            self.1.send(AppEvent::GestureCancelled {
                gesture: model.gesture,
            });
        }
    }

    fn highlight(&mut self, highlight: Highlight) {
        if let Content::Menu(_, active) = &mut self.0.state.borrow_mut().content {
            *active = highlight;
        }
        // SAFETY: The update is requested for our own window.
        unsafe {
            let _ = InvalidateRect(Some(self.0.handle), None, false);
        }
    }

    fn hide(&mut self) {
        self.0.hide();
        self.0.state.borrow_mut().content = Content::None;
    }
}

impl RadialMenu {
    fn show(&self, model: &MenuModel) -> Result<()> {
        let scale = self.2.get();
        let radius = pixels(model.geometry.outer_radius(), scale);
        let diameter = 2 * radius;
        // SAFETY: The region is ours until SetWindowRgn succeeds.
        unsafe {
            let region = CreateEllipticRgn(0, 0, diameter, diameter);
            if region.is_invalid() {
                return Err(Error::from_thread());
            }
            if SetWindowRgn(self.0.handle, Some(region), false) == 0 {
                let error = Error::from_thread();
                let _ = DeleteObject(region.into());
                return Err(error);
            }
        }
        self.0.state.borrow_mut().content = Content::Menu(model.clone(), Highlight::None);
        let x = pixels(model.center.x, scale);
        let y = pixels(-model.center.y, scale);
        self.0.show(
            RECT {
                left: x - radius,
                top: y - radius,
                right: x + radius,
                bottom: y + radius,
            },
            scale,
        )
    }
}

#[derive(Clone)]
pub(super) struct Popup(pub Rc<Window>);

impl Popup {
    pub(super) fn hide(&self) {
        self.0.hide();
    }

    /// The tray click and the icon restore request recorded since the last
    /// call. The popup window owns the tray icon.
    pub(super) fn take_tray_requests(&self) -> (Option<LPARAM>, bool) {
        let state = self.0.state.borrow();
        (state.tray.click.take(), state.tray.restore.take())
    }

    /// Forgets a tray click recorded while the tray menu was open: it
    /// belonged to that menu.
    pub(super) fn discard_tray_click(&self) {
        self.0.state.borrow().tray.click.set(None);
    }

    pub(super) fn display(&self, title: impl Into<String>, body: impl Into<String>, duration: u32) {
        let Ok(point) = cursor() else { return };
        let Some(area) = work_area(point) else { return };
        let scale = monitor_scale(point);
        let width = pixels(360.0, scale).min(area.right - area.left);
        let height = pixels(118.0, scale).min(area.bottom - area.top);
        let offset = pixels(16.0, scale);
        let left = (point.x + offset).clamp(area.left, area.right - width);
        let top = (point.y + offset).clamp(area.top, area.bottom - height);
        self.0.state.borrow_mut().content = Content::Feedback {
            title: title.into(),
            body: body.into(),
        };
        // SAFETY: The timer is scoped to the popup's window handle.
        unsafe {
            if SetTimer(Some(self.0.handle), DISMISS, duration, None) == 0 {
                self.hide();
                return;
            }
        }
        let bounds = RECT {
            left,
            top,
            right: left + width,
            bottom: top + height,
        };
        if let Err(error) = self.0.show(bounds, scale) {
            warn!(%error, "could not show feedback");
        }
    }
}

impl Feedback for Popup {
    fn job_progress(&mut self, _: JobId, _: Progress) {}

    fn notify(&mut self, notice: &Notice) {
        let (title, body) = match notice {
            Notice::PathCopied => ("Path copied", "The path is on the clipboard.".into()),
            Notice::Refused(reason) => {
                let message = match reason {
                    ResolveError::FileManagerNotActive => "Select an item in File Explorer.".into(),
                    ResolveError::OutsideFileArea => "Focus the file list in File Explorer.".into(),
                    ResolveError::NothingSelected => {
                        "Select a file or folder in File Explorer.".into()
                    }
                    ResolveError::MultipleSelected => "Select just one file or folder.".into(),
                    ResolveError::SelectionUnstable => "The selection changed. Try again.".into(),
                    _ => reason.to_string(),
                };
                ("Can't open the menu", message)
            }
            Notice::NoFileName => (
                "Can't open the menu",
                "Select a named file or folder.".into(),
            ),
            Notice::ActionFailed { action, message } => {
                let title = match action {
                    ShellAction::CopyPath => "Couldn't copy the path",
                    ShellAction::Reveal => "Couldn't show the item",
                };
                (title, message.clone())
            }
            Notice::JobSucceeded { output, .. } => ("Done", output.path.display().to_string()),
            Notice::JobFailed { error, .. } => ("Action failed", error.to_string()),
        };
        self.display(title, body, 3_500);
    }

    fn show_info(&mut self, target: &FileTarget) {
        let size = match target.facts.kind {
            FileKind::File => format!("File  ·  {} bytes", target.version.size),
            FileKind::Directory | FileKind::Package => "Folder".into(),
            FileKind::Symlink => "Symbolic link".into(),
        };
        self.display(size, target.path.display().to_string(), 12_000);
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: Win32 supplies the creation structure for WM_NCCREATE and
    // Window keeps the state alive until DestroyWindow completes.
    unsafe {
        let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *const RefCell<WindowState>;
        match message {
            WM_NCCREATE => {
                let create = &*(lparam.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(window, &mut paint);
                let mut bounds = RECT::default();
                if let Some(state) = state.as_ref()
                    && let Ok(state) = state.try_borrow()
                    && GetClientRect(window, &mut bounds).is_ok()
                {
                    render(dc, bounds, &state);
                }
                let _ = EndPaint(window, &paint);
                return LRESULT(0);
            }
            WM_NCHITTEST => return LRESULT(HTTRANSPARENT as isize),
            WM_ERASEBKGND => return LRESULT(1),
            WM_TIMER if wparam.0 == DISMISS => {
                let _ = KillTimer(Some(window), DISMISS);
                let _ = ShowWindow(window, SW_HIDE);
                return LRESULT(0);
            }
            TRAY_MESSAGE => {
                if is_click(lparam)
                    && let Some(state) = state.as_ref()
                    && let Ok(state) = state.try_borrow()
                {
                    state.tray.click.set(Some(lparam));
                }
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                // A shown window whose scale changed under it (display
                // settings, a monitor added or removed) has the layout of the
                // old scale, so it closes; for the menu the loop then cancels
                // the gesture. The suggested rectangle is not used.
                if let Some(state) = state.as_ref()
                    && let Ok(state) = state.try_borrow()
                    && !state.positioning.get()
                    && IsWindowVisible(window).as_bool()
                {
                    let _ = KillTimer(Some(window), DISMISS);
                    let _ = ShowWindow(window, SW_HIDE);
                    state.dpi_lost.set(true);
                }
                return LRESULT(0);
            }
            WM_CLOSE => return LRESULT(0),
            WM_NCDESTROY => {
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            }
            other => {
                // The broadcast is sent to the procedure, not the thread queue.
                if let Some(state) = state.as_ref()
                    && let Ok(state) = state.try_borrow()
                    && other != 0
                    && other == state.taskbar_created
                {
                    state.tray.restore.set(true);
                }
            }
        }
        DefWindowProcW(window, message, wparam, lparam)
    }
}

fn render(dc: HDC, bounds: RECT, state: &WindowState) {
    // SAFETY: The buffer and bitmap are compatible with the paint DC.
    // Each selected GDI object is restored before deletion.
    unsafe {
        let buffer = CreateCompatibleDC(Some(dc));
        if buffer.is_invalid() {
            draw(dc, bounds, state);
            return;
        }
        let bitmap = CreateCompatibleBitmap(dc, bounds.right, bounds.bottom);
        if bitmap.is_invalid() {
            let _ = DeleteDC(buffer);
            draw(dc, bounds, state);
            return;
        }
        let previous = SelectObject(buffer, bitmap.into());
        draw(buffer, bounds, state);
        let _ = BitBlt(
            dc,
            0,
            0,
            bounds.right,
            bounds.bottom,
            Some(buffer),
            0,
            0,
            SRCCOPY,
        );
        SelectObject(buffer, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(buffer);
    }
}

fn draw(dc: HDC, bounds: RECT, state: &WindowState) {
    let canvas = Canvas {
        dc,
        scale: state.scale,
    };
    // SAFETY: The stock objects are borrowed, and the previous objects are restored.
    unsafe {
        let brush = SelectObject(dc, GetStockObject(DC_BRUSH));
        let pen = SelectObject(dc, GetStockObject(DC_PEN));
        SetDCBrushColor(dc, BACKGROUND);
        FillRect(dc, &bounds, HBRUSH(GetStockObject(DC_BRUSH).0));
        SetBkMode(dc, TRANSPARENT);
        match &state.content {
            Content::Menu(model, highlight) => {
                draw_menu(&canvas, bounds.right / 2, model, *highlight)
            }
            Content::Feedback { title, body } => {
                let padding = pixels(16.0, canvas.scale);
                let mut text = bounds;
                text.left += padding;
                text.right -= padding;
                text.top += padding;
                text.bottom -= padding;
                canvas.text(
                    title,
                    text,
                    17.0,
                    FW_SEMIBOLD,
                    TEXT,
                    DT_SINGLELINE | DT_END_ELLIPSIS,
                );
                text.top += pixels(29.0, canvas.scale);
                canvas.text(
                    body,
                    text,
                    14.0,
                    FW_NORMAL,
                    MUTED,
                    DT_WORDBREAK | DT_END_ELLIPSIS,
                );
            }
            Content::None => {}
        }
        SelectObject(dc, pen);
        SelectObject(dc, brush);
    }
}

fn draw_menu(canvas: &Canvas, center: i32, model: &MenuModel, highlight: Highlight) {
    const ARC_STEPS: usize = 12;
    let outer = model.geometry.outer_radius();
    let inner = model.geometry.neutral_radius();
    let polar = |angle: f64, radius: f64| POINT {
        x: center + pixels(angle.sin() * radius, canvas.scale),
        y: center - pixels(angle.cos() * radius, canvas.scale),
    };
    for (index, slot) in model.slots.iter().enumerate() {
        let (start, end) = model.geometry.slot_bounds(index);
        let angle = |step| start + (end - start) * step as f64 / ARC_STEPS as f64;
        let mut path = Vec::with_capacity(2 * (ARC_STEPS + 1));
        path.extend((0..=ARC_STEPS).map(|step| polar(angle(step), outer - 1.0)));
        path.extend((0..=ARC_STEPS).rev().map(|step| polar(angle(step), inner)));
        let fill = match highlight {
            Highlight::Active(slot) if slot == index => SELECTED,
            Highlight::Inactive(slot) if slot == index => SECTOR,
            _ => BACKGROUND,
        };
        // SAFETY: The points and selected stock objects are valid for this paint call.
        unsafe {
            SetDCBrushColor(canvas.dc, fill);
            SetDCPenColor(canvas.dc, BORDER);
            let _ = Polygon(canvas.dc, &path);
        }
        // Slots within one slot width of straight up sit side by side, so
        // their horizontal labels need the wider, outer part of the ring.
        let step = TAU / model.geometry.slots() as f64;
        let text_radius = if model.geometry.slot_center(index).cos() > step.cos() {
            82.0
        } else {
            70.0
        };
        let text_center = polar(model.geometry.slot_center(index), text_radius);
        let color = match slot.item {
            MenuItem::Cancel => CANCEL,
            _ if !slot.availability.is_available() => MUTED,
            _ => TEXT,
        };
        let label = match slot.item {
            MenuItem::Cancel => "Cancel",
            MenuItem::Action(action) => match action {
                ActionId::CopyPath => "Copy path",
                ActionId::ShowInfo => "Info",
                ActionId::Rename => "Rename",
                ActionId::ArchiveZip => "ZIP",
                ActionId::ResizeImage => "Resize",
                ActionId::ConvertToPng => "PNG",
                ActionId::ConvertToJpeg => "JPEG",
                ActionId::Reveal => "Reveal",
            },
        };
        canvas.centered(label, text_center, 39.0, color);
    }
    canvas.centered(
        &model.title,
        POINT {
            x: center,
            y: center,
        },
        23.0,
        MUTED,
    );
}

struct Canvas {
    dc: HDC,
    scale: f64,
}

impl Canvas {
    fn centered(&self, text: &str, point: POINT, half_width: f64, color: COLORREF) {
        let width = pixels(half_width, self.scale);
        let height = pixels(10.0, self.scale);
        self.text(
            text,
            RECT {
                left: point.x - width,
                top: point.y - height,
                right: point.x + width,
                bottom: point.y + height,
            },
            12.0,
            FW_SEMIBOLD,
            color,
            DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }

    fn text(
        &self,
        text: &str,
        mut bounds: RECT,
        size: f64,
        weight: FONT_WEIGHT,
        color: COLORREF,
        format: DRAW_TEXT_FORMAT,
    ) {
        let mut text: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: The font is selected only for this draw and deleted after the old one is restored.
        unsafe {
            let font = CreateFontW(
                -pixels(size, self.scale),
                0,
                0,
                0,
                weight.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Segoe UI"),
            );
            let previous = (!font.is_invalid()).then(|| SelectObject(self.dc, font.into()));
            SetTextColor(self.dc, color);
            DrawTextW(self.dc, &mut text, &mut bounds, format | DT_NOPREFIX);
            if let Some(previous) = previous {
                SelectObject(self.dc, previous);
                let _ = DeleteObject(font.into());
            }
        }
    }
}

fn pixels(unit: f64, scale: f64) -> i32 {
    (unit * scale).round() as i32
}

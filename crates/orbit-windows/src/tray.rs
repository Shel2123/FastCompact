//! The Orbit notification area icon and its exit menu.

use std::mem::size_of;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Error, Result, w};

use crate::cursor;

pub(super) const TRAY_MESSAGE: u32 = WM_APP + 1;
const EXIT: usize = 1;

/// Whether the mouse event of a [`TRAY_MESSAGE`] opens the tray menu.
pub(super) fn is_click(event: LPARAM) -> bool {
    matches!(event.0 as u32, WM_RBUTTONUP | WM_LBUTTONUP)
}

pub(super) struct Tray(NOTIFYICONDATAW);

impl Tray {
    pub(super) fn install(window: HWND) -> Result<Self> {
        // SAFETY: IDI_APPLICATION refers to a shared system icon.
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION)? };
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: TRAY_MESSAGE,
            hIcon: icon,
            ..Default::default()
        };
        for (output, unit) in data
            .szTip
            .iter_mut()
            .zip("Orbit · Ctrl+Alt+Space".encode_utf16())
        {
            *output = unit;
        }
        let tray = Self(data);
        tray.add()?;
        Ok(tray)
    }

    pub(super) fn add(&self) -> Result<()> {
        // SAFETY: The icon data refers to a live window on this thread.
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &self.0) }.as_bool() {
            return Err(Error::from_thread());
        }
        Ok(())
    }

    /// Shows the tray menu at the pointer and tells whether Exit was
    /// chosen. Runs a nested message loop until the menu closes.
    pub(super) fn wants_exit(&self) -> Result<bool> {
        let point = cursor()?;
        // SAFETY: The menu is built and used on the window thread and
        // destroyed after TrackPopupMenu returns.
        unsafe {
            let menu = CreatePopupMenu()?;
            let result = (|| {
                AppendMenuW(
                    menu,
                    MF_STRING | MF_GRAYED,
                    0,
                    w!("Hold Ctrl+Alt+Space in File Explorer"),
                )?;
                AppendMenuW(menu, MF_SEPARATOR, 0, None)?;
                AppendMenuW(menu, MF_STRING, EXIT, w!("Exit Orbit"))?;
                let _ = SetForegroundWindow(self.0.hWnd);
                let choice = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                    point.x,
                    point.y,
                    None,
                    self.0.hWnd,
                    None,
                );
                // Let the shell dismiss the tray menu on the next invocation.
                let _ = PostMessageW(Some(self.0.hWnd), WM_NULL, WPARAM(0), LPARAM(0));
                Ok(choice.0 as usize == EXIT)
            })();
            let _ = DestroyMenu(menu);
            result
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: The notification area icon belongs to this window.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.0);
        }
    }
}

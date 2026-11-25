//! Resolve one filesystem selection in the active Explorer view.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orbit_app::messages::{GestureId, ResolveError};
use orbit_app::ports::{ResolveRequest, Shell, ShellError, TargetResolver};
use orbit_domain::target::{FileFacts, FileIdentity, FileKind, FileTarget, FileVersion};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    GetFileInformationByHandle,
};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, IServiceProvider,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    IFolderView2, IShellBrowser, IShellWindows, IWebBrowser2, SHOpenFolderAndSelectItems,
    SHParseDisplayName, SID_STopLevelBrowser, SIGDN_FILESYSPATH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, IsChild, IsWindowVisible,
};
use windows::core::{Error, Interface, PCWSTR, Result};

/// Captured window handles are plain identifiers, verified on both threads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ExplorerFocus {
    window: isize,
    pane: isize,
}

impl ExplorerFocus {
    pub(super) fn capture() -> Option<Self> {
        // SAFETY: The buffer is initialized to the documented size.
        unsafe {
            let window = GetForegroundWindow();
            let mut info = GUITHREADINFO {
                cbSize: size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            GetGUIThreadInfo(0, &mut info).ok()?;
            (window == info.hwndActive && !window.is_invalid() && !info.hwndFocus.is_invalid())
                .then_some(Self {
                    window: window.0 as isize,
                    pane: info.hwndFocus.0 as isize,
                })
        }
    }
}

pub(super) type Selection = Arc<Mutex<Option<(GestureId, ExplorerFocus)>>>;

pub(super) struct Apartment;

impl Apartment {
    pub(super) fn initialize() -> Result<Self> {
        // SAFETY: Each success is paired with CoUninitialize on this thread.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: The apartment is local to the thread that initialized COM.
        unsafe { CoUninitialize() };
    }
}

pub(super) struct ExplorerResolver(pub Selection);

impl TargetResolver for ExplorerResolver {
    fn resolve(
        &mut self,
        request: &ResolveRequest,
    ) -> std::result::Result<FileTarget, ResolveError> {
        let focus = self
            .0
            .lock()
            .map_err(resolve_failed)?
            .filter(|(id, _)| *id == request.gesture)
            .map(|(_, focus)| focus)
            .ok_or(ResolveError::SelectionUnstable)?;
        let _com = Apartment::initialize().map_err(resolve_failed)?;
        let view = explorer_view(focus)?;
        let path = selected_path(&view)?;
        let target = read_target(path).map_err(resolve_failed)?;
        if ExplorerFocus::capture() != Some(focus)
            || selected_path(&view).as_ref() != Ok(&target.path)
            || ExplorerFocus::capture() != Some(focus)
        {
            return Err(ResolveError::SelectionUnstable);
        }
        Ok(target)
    }
}

fn resolve_failed(error: impl std::fmt::Display) -> ResolveError {
    ResolveError::Failed(error.to_string())
}

fn explorer_view(focus: ExplorerFocus) -> std::result::Result<IFolderView2, ResolveError> {
    if ExplorerFocus::capture() != Some(focus) {
        return Err(ResolveError::SelectionUnstable);
    }
    let wanted = HWND(focus.window as _);
    let pane = HWND(focus.pane as _);
    let mut matching_window = false;
    // SAFETY: COM is initialized on this thread. The interfaces are owned by
    // this call and the captured HWNDs are used only for Win32 comparisons.
    unsafe {
        let windows: IShellWindows =
            CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER).map_err(resolve_failed)?;
        for index in 0..windows.Count().map_err(resolve_failed)? {
            let Ok(browser) = windows
                .Item(&VARIANT::from(index))
                .and_then(|item| item.cast::<IWebBrowser2>())
            else {
                continue;
            };
            if browser.HWND().ok().map(|handle| HWND(handle.0 as _)) != Some(wanted) {
                continue;
            }
            matching_window = true;
            let Ok(view) = browser
                .cast::<IServiceProvider>()
                .and_then(|provider| provider.QueryService::<IShellBrowser>(&SID_STopLevelBrowser))
                .and_then(|browser| browser.QueryActiveShellView())
            else {
                continue;
            };
            let handle = view.GetWindow().map_err(resolve_failed)?;
            if IsWindowVisible(handle).as_bool()
                && (handle == pane || IsChild(handle, pane).as_bool())
            {
                return view.cast().map_err(resolve_failed);
            }
        }
    }
    Err(if matching_window {
        ResolveError::OutsideFileArea
    } else {
        ResolveError::FileManagerNotActive
    })
}

fn selected_path(view: &IFolderView2) -> std::result::Result<PathBuf, ResolveError> {
    // SAFETY: View belongs to this COM apartment and the returned path is
    // copied before its COM buffer is freed.
    unsafe {
        let items = view.GetSelection(false).map_err(resolve_failed)?;
        match items.GetCount().map_err(resolve_failed)? {
            0 => Err(ResolveError::NothingSelected),
            1 => {
                let path = items
                    .GetItemAt(0)
                    .and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH))
                    .map_err(|_| ResolveError::Failed("Select a file or folder on disk.".into()))?;
                let owned = OsString::from_wide(path.as_wide());
                CoTaskMemFree(Some(path.0.cast()));
                Ok(PathBuf::from(owned))
            }
            _ => Err(ResolveError::MultipleSelected),
        }
    }
}

fn read_target(path: PathBuf) -> std::io::Result<FileTarget> {
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(&path)?;
    let metadata = file.metadata()?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: The file handle stays open, and info is writable for the call.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(std::io::Error::from)?;
    let kind = if metadata.file_type().is_symlink() {
        FileKind::Symlink
    } else if metadata.is_dir() {
        FileKind::Directory
    } else {
        FileKind::File
    };
    Ok(FileTarget {
        path,
        identity: FileIdentity {
            device: u64::from(info.dwVolumeSerialNumber),
            inode: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        },
        version: FileVersion {
            size: metadata.len(),
            modified: metadata.modified()?,
        },
        facts: FileFacts { kind, image: None },
    })
}

pub(super) struct ExplorerShell(pub HWND);

impl Shell for ExplorerShell {
    fn write_clipboard(&mut self, path: &Path) -> std::result::Result<(), ShellError> {
        copy_path(self.0, path).map_err(|error| ShellError(error.to_string()))
    }

    fn reveal(&mut self, path: &Path) -> std::result::Result<(), ShellError> {
        reveal_path(path).map_err(|error| ShellError(error.to_string()))
    }
}

fn copy_path(owner: HWND, path: &Path) -> Result<()> {
    let text = wide_path(path);
    // SAFETY: The UTF-16 buffer fits the allocation. The clipboard takes
    // ownership only on success. Every successful open is closed below.
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, size_of::<u16>() * text.len())?;
        let data = GlobalLock(memory).cast::<u16>();
        if data.is_null() {
            let error = Error::from_thread();
            let _ = GlobalFree(Some(memory));
            return Err(error);
        }
        data.copy_from_nonoverlapping(text.as_ptr(), text.len());
        let _ = GlobalUnlock(memory);
        let result = OpenClipboard(Some(owner)).and_then(|()| {
            let result = EmptyClipboard().and_then(|()| {
                SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(memory.0)))
            });
            let _ = CloseClipboard();
            result
        });
        if result.is_err() {
            let _ = GlobalFree(Some(memory));
        }
        result.map(|_| ())
    }
}

fn reveal_path(path: &Path) -> Result<()> {
    let text = wide_path(path);
    let mut item = std::ptr::null_mut();
    // SAFETY: The application thread has initialized COM. Shell owns the
    // PIDL allocation until it hands it to us.
    unsafe {
        SHParseDisplayName(PCWSTR(text.as_ptr()), None, &mut item, 0, None)?;
        let result = SHOpenFolderAndSelectItems(item, None, 0);
        CoTaskMemFree(Some(item.cast()));
        result
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

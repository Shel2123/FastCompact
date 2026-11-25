# Orbit for Windows

Build on Windows with Rust 1.97 or newer and the MSVC C++ build tools.

```powershell
cargo build --locked --release -p orbit-desktop
.\target\release\orbit.exe
```

1. Select one file or folder in File Explorer.
2. Hold Ctrl+Alt+Space to open the radial menu at the pointer.
3. Move over Copy path, Info, or Reveal and release the shortcut. Info shows
   the path, item type and file size. Reveal opens the parent folder and
   selects the item.
4. Press Esc, release in the center, or release outside the ring to dismiss
   the menu. Use the Orbit tray icon to exit the app.

Rename, ZIP, resize, and image conversion are dimmed on Windows. The menu
works in Explorer's file list. A shortcut already registered by another
program is reported at startup.

To check the Windows UI, try a file with a Unicode name, a folder, an empty
or multiple selection, and a selection near a screen edge. Check that the
menu leaves Explorer focused, dimmed actions do nothing, Esc cancels the
gesture, and Copy path pastes the complete path.

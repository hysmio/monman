# MonMan

MonMan is a small Windows monitor-layout manager written in Rust with `eframe`/`egui`.
It uses Windows CCD (`QueryDisplayConfig` / `SetDisplayConfig`) rather than the legacy
`ChangeDisplaySettingsEx` route.

The important behavior is that a layout is an **exclusive set of active display paths**.
When a saved monitor is marked **Off**, MonMan does not move it outside the desktop or
hide windows on it: that monitor's path is omitted from the topology submitted to
Windows, so Windows deactivates the display path.

## Current features

- Capture the current Windows display topology as a named layout.
- Create a custom layout seeded from all currently connected monitors.
- Keep disabled monitors in a saved layout so they can be enabled again later.
- Drag monitors in an egui preview to edit desktop X/Y coordinates.
- Snap dragged enabled monitors to adjacent edges and matching top, middle, or
  bottom axes, with highlighted alignment guides.
- Edit X/Y, source width/height, and refresh rate numerically.
- Make any enabled desktop source primary by shifting it to `(0, 0)`.
- Preserve captured clone/mirror groups when recapturing an existing topology.
- Detect and display portrait/landscape orientation, allow 0°/90°/180°/270°
  editing, and preserve DisplayConfig source-to-target scaling values.
- Sync newly connected monitors into an existing layout without enabling them
  automatically.
- Duplicate, rename, delete, explicitly save, and automatically save layouts.
- Assign optional global hotkeys (at least one of `Ctrl` / `Alt` / `Shift` / `Win` + F1-F12 or 0-9).
- Keep running in the Windows system tray when the GUI is closed, with Open and
  Quit actions so global hotkeys remain available.
- Report global-hotkey registration failures in the GUI instead of silently ignoring
  reserved or conflicting shortcuts.
- Capture the current topology before every apply, automatically attempt rollback on a failed two-stage apply, and offer **Undo last apply** after success.
- Keep a backup of the last parseable `layouts.json` and recover from it if the primary file becomes unreadable/corrupt.
- Refuse to apply an all-off layout.
- Refuse to silently substitute a different target when a required saved monitor is
  disconnected.

## How applying a layout works

MonMan uses a two-stage CCD apply:

1. Query all currently possible display paths with `QDC_ALL_PATHS`.
2. Match each saved monitor by its DisplayConfig monitor device path.
3. Construct only the paths for monitors marked **On**. Source/target mode indexes are
   initially invalid so Windows can use its best-mode logic when a previously disabled
   monitor is re-enabled.
4. Validate and apply that exclusive topology using `SetDisplayConfig`.
5. Query the now-active topology again and patch concrete source modes with the saved
   coordinates/resolution plus the saved target refresh rate.
6. Validate and apply the final mode data and save it to the Windows display database.

Clone layouts are represented explicitly. Targets in a clone group share one Windows
source; independent monitors are allocated separate available sources. This avoids
mistaking inactive `QDC_ALL_PATHS` routing candidates for an intentional clone setup.

## Build on Windows

From the project directory:

```powershell
cd F:\Projects\hysmio\monman
cargo check
cargo run --release
```

To build only the executable:

```powershell
cargo build --release
```

The binary will be:

```text
target\release\monman.exe
```

The project currently targets Rust edition 2024 and uses `windows` 0.62.2 and
`eframe` 0.34.1.

## Using it

1. Start MonMan.
2. Choose **Capture current layout** to save what Windows is using now, or **New custom
   layout** to start with all connected monitors disabled and select the ones you want.
3. Tick **On** only for displays that should participate in that layout.
4. Drag the monitor rectangles or edit coordinates, source resolution, orientation,
   and refresh rate in the grid.
5. Use **Make primary** if needed.
6. Optionally enable and choose a global hotkey.
7. Click **Apply**.

Changes are saved to:

```text
%APPDATA%\MonMan\layouts.json
```

Autosave is throttled while controls are being dragged so the file is not rewritten on
every GUI frame. **Save now** is available for an immediate explicit write. The previous
parseable config is retained as `layouts.json.bak` before the primary file is replaced.

Before applying a layout, MonMan captures the currently active topology in memory. If the
two-stage CCD apply fails after changing active paths, it attempts to restore that snapshot.
After a successful apply, **Undo last apply** restores the snapshot manually.

## Important distinction: Windows-disable vs hardware power-off

"Off" in MonMan means the Windows display path is deactivated. The monitor is no longer
part of the Windows desktop and Windows stops driving that target as an active desktop
path. This is stronger than simply moving a monitor off-screen.

It is not a DDC/CI command that cuts hardware power to the panel. A physical monitor may
show "No signal" and enter its own standby behavior. Hardware power control would be a
separate optional feature.

## Hotkey behavior

Global hotkeys use Win32 `RegisterHotKey` with `MOD_NOREPEAT` on a dedicated message-loop
thread. Closing the GUI hides MonMan to the system tray, so those shortcuts continue to
work. Double-click the tray icon to reopen the GUI, or right-click it and choose **Open
MonMan** or **Quit MonMan**. If Windows or another application already owns a shortcut,
the failure is shown in MonMan's status bar.

## Safety / limitations

- At least one monitor must remain enabled; an all-off layout is rejected before any CCD
  call.
- Applying display configuration requires access to the interactive console desktop;
  Windows can reject CCD changes from unsupported/remote contexts.
- A layout intentionally fails if a required target is disconnected instead of choosing
  a different monitor with a similar friendly name.
- DisplayConfig path scaling is captured/restored, but its GUI editing is not exposed yet.
- Windows per-monitor DPI percentage is not the same field as DisplayConfig path scaling
  and is not currently edited by MonMan.
- Use **Quit MonMan** from the tray menu or GUI when you want to stop global hotkeys.

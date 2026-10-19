# No helper consoles (v0.1.11)

Airstream must not flash a black console or Windows Terminal during normal use. This includes startup, item selection, probing, thumbnails, drag/drop import, playback/preparation, subtitle extraction and native file/folder pickers.

## Cause and fix

The GUI executable was already a Windows-subsystem build, but child console executables were spawned with default process flags. On the tested Windows 11 machine, four metadata requests produced four visible Windows Terminal windows.

`src/process.rs` now centralizes both synchronous and Tokio process constructors with Windows `CREATE_NO_WINDOW` (0x08000000). All FFmpeg/ffprobe calls, dialog PowerShell, and the cmd browser fallback use the appropriate constructor. GUI Edge and Explorer remain normal GUI launches. Output capture, diagnostics, exit status, child lifecycle and file-dialog visibility are preserved; no unrelated terminals are closed or hidden.

## Verified on the installed executable

Win32 `SetWinEventHook(EVENT_OBJECT_SHOW)` observed `ConsoleWindowClass` and `CASCADIA_HOSTING_WINDOW_CLASS`:

- Before: v0.1.10, four metadata reads, four visible Terminal events.
- After: v0.1.11, the same four reads, zero visible Terminal events.
- Edge E2E: external file/folder drops, probing, internal reordering, deletion, multi-select and context menu all passed with zero visible console events.
- Real receiver E2E: audio/subtitle memory, original audio stream hash, x1.5 playback and consecutive episode changes passed with zero visible console events.
- Both native file and folder dialogs were visibly detected, cancelled successfully and returned JSON, with zero console events.

Tests ran against `%LOCALAPPDATA%\Programs\Airstream\airstream.exe`. Test fixtures were restored/removed, with the user's playlist/preferences preserved. The watcher hides its own fixture-generation subprocesses separately so app-created consoles would still be detected.

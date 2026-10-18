# Windows per-user installation

Run the supplied PowerShell installer with the tested portable executable staged in `dist/airstream.exe`:

```powershell
powershell -NoProfile -File .\scripts\install.ps1 -Launch
```

An explicit executable can be supplied with `-SourceExe`. FFmpeg and ffprobe must be available initially; optionally pass their folder with `-MediaToolsDirectory`.

## What is installed

- Application and private media runtimes: `%LOCALAPPDATA%\Programs\Airstream`.
- Start Menu: Airstream group and application shortcut.
- Desktop: Airstream shortcut, with the embedded executable icon.
- Current-user Installed Apps entry: Airstream, including an uninstall action.

No administrator rights, service, scheduled task or global PATH change is required. The application prioritizes the private FFmpeg/ffprobe in its own folder. Same-volume media-tool files are hard-linked where possible instead of duplicating hundreds of MB; a normal copy is used if hard links are unavailable. Removing the original tool package does not remove the installed app's remaining hard links.

The existing Desktop portable executable is archived under `%LOCALAPPDATA%\Airstream\backups` rather than left as a second visible entry point. Playlist/preferences/media caches remain in `%LOCALAPPDATA%\Airstream`.

## Uninstall

Use Windows Settings > Apps > Installed apps > Airstream > Uninstall. The uninstaller asks before removing the application. It removes app files, the Installed Apps registration, and the two shortcut locations while preserving user data. Automated verification can call `uninstall.ps1 -Quiet` inside the installed application folder.

## Verification performed for v0.1.7

- Installed executable tested through real Edge UI and a Cast receiver, with system FFmpeg removed from the test process PATH; private media runtimes supplied probing/preparation.
- Five speed choices only: x1, x1.25, x1.5, x1.75, x2. Offline selection, restart persistence, initial playback speed and clock advancement verified.
- Real uninstall/reinstall performed; program files, registration and shortcuts removed/restored, while the preferences SHA-256 stayed unchanged.
- Start Menu shortcut launched the installed path, and the server read back version 0.1.7.

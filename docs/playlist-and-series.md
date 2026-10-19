# Playlist editing and series viewing (v0.1.9)

## Reorder, remove, drop

- Drag a row (grip on the left) to reorder. Alt+Up/Down moves the selected row; Delete removes it.
- The ✕ button removes an item from the playlist only. The media file is never deleted.
- Drop video files or folders from Explorer onto the window. A drop at a row inserts before that row; elsewhere it appends. A dropped folder is imported recursively in natural order (E2 before E10).

Browsers do not expose the absolute path of dropped files. The window therefore sends name, size and modified time to `/api/add_dropped`, and the local app finds the exact file with all three facts. Media folders are searched first, then the profile and fixed drives, excluding system and AppData folders. The search is local-only (non-loopback callers receive 403), serialized and limited to 25 seconds. If a file cannot be found the toast says so; `＋ ไฟล์` remains the fallback.

## Remembered audio, subtitles and speed

Audio and subtitle choices are saved by what the track is: language, title, codec, channel count and position among identically-tagged tracks. Position numbers are not used across files because episodes often reorder tracks. A positional index is only trusted for the file it was saved for.

- Pick audio/subtitle once; the next episode resolves the same choice automatically.
- If an episode lacks the remembered subtitle or audio, the UI shows a note, the episode uses automatic audio / no subtitle, and the remembered choice is kept for later episodes.
- Choosing "ปิด" for subtitles is remembered and is not overridden by automatic language rules.
- Playback speed (`x1` to `x2`) is global and is sent in each new Cast LOAD.
- Subtitle bitmap tracks (PGS/DVD) are not selectable for the native text-subtitle path.

## Verification

Release-executable E2E, using Edge and a real Cast TV:

- Internal drag reorder agrees across DOM, API and prefs file; keyboard reorder and Delete work.
- Chromium drag-drop with real file paths: duplicate skipped, same-name decoy rejected, folder imported, non-video and unresolvable drops reported, LAN origin rejected.
- Series fixture with three different track layouts: audio/subtitle choice follows by identity, missing subtitle handled, subtitle-off persists, TV playback uses x1.5 and the expected audio stream hash without reselecting.
- Choices survive a full app restart.
- A real 4K HEVC 10-bit HDR10 60fps excerpt from the supplied series played on the tested receiver in Original mode.

If Airflow (`Airflow.server.exe`) already owns port 42015, Airstream moves to the next free port, normally 42016.

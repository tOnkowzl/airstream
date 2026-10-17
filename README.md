# Airstream

**Airflow-style local media caster — rewritten in Rust.**

Airstream is a small, fast Windows media player + LAN caster that reproduces the workflow of
[Airflow](http://www.airflowapp.com) (playlist → probe → topology → HTTP/HLS → TV), built from a
reverse-engineering study of Airflow 3.3.7 and rewritten from scratch in Rust.

> Clean-room note: this repository contains **no** code, binaries, or assets from Airflow.
> Behaviour was studied with REA/PDB symbol recovery and public protocol specs; the
> implementation, UI markup and every line of code here are original.

---

## Why

On an i5-13400F + RTX 4060 machine, Airflow can only hardware-accelerate through **Intel
QuickSync**, which does not exist on F-series CPUs. It therefore falls back to software `libx264`
and logs:

```
codec_util:75 … Intel QuickSync hardware is not available
Transcoding Rate: 91% realtime            <-- slower than realtime
```

Airstream keeps the exact same topology decision but adds the GPU encoders the original never
ships — **NVENC / AMF / QSV** — so the same 1080p source encodes ~20x faster:

| task (60 s of 1080p) | Airflow today | Airstream |
|---|---|---|
| remux / passthrough | 0.12 s (500x) | same path (copy) |
| H.264 8-bit source | passthrough | passthrough |
| HEVC / 10-bit source | `libx264` — 69 s (**0.86x**) | `h264_nvenc` — 3.3 s (**18x**) |

## Features

- **Cast-only remote console** — no in-app video. Shows the playing title, elapsed / total time,
  a seekable progress bar, play/pause/stop, ±10/30 s, plus the Airflow-style playlist panel.
  Position is read **from the TV itself** (Cast `MEDIA_STATUS`).
- **Real Google Cast v2 sender** (TLS :8009, protobuf, Default Media Receiver) — verified against an
  Android-TV Chromecast: load, play, pause, stop and exact seek.
- **Pick audio / subtitle tracks from dropdowns** — read from the file with ffprobe; text
  subtitles can be burned in. Settings mirror Airflow's: quality ladder (`passthrough`,
  `variable-720p/1080p/1080p-hq`), encoder (auto/NVENC/AMF/QSV/x264), subtitle size, resume, loop.
- **MKV/AC3/DTS/HEVC just work** — non-MP4 sources are remuxed/encoded once into a faststart MP4
  (progress shown) because many Cast receivers reject live HLS but accept MP4 + Range.
- **Range requests** — `/media/<id>` supports `Range:` so clients can seek in the original file.
- **Airflow prefs import** — on first run it reads
  `%LOCALAPPDATA%\Airflow\PlaylistManager.prefs` (the brace-less JSON fragment Airflow writes)
  and imports the existing playlist.
- **Single .exe** — static Rust binary; the UI is embedded in the executable.

## How it works

```
Airstream.exe
 ├── axum HTTP server  :42800   (LAN IP, e.g. http://192.168.1.40:42800)
 │    ├── /                embedded UI (HTML/CSS/JS compiled into the binary)
 │    ├── /api/*           state, add files/folders, probe, devices, cast, play, stats
 │    ├── /stream/:id?t=   fragmented-MP4 pipe for the built-in player (seek = restart at t)
 │    ├── /media/:id       original file with Range support
 │    └── /hls/:sid/*      HLS sessions (fMP4 segments) for TVs / other clients
 ├── ffprobe             media analyzer  (stream inventory, passthrough decision)
 ├── ffmpeg              transcoder     (copy | h264_nvenc | h264_amf | h264_qsv | libx264)
 └── WebView2 app window  loads http://127.0.0.1:42800 in Edge --app mode
```

Topology rule (same idea as Airflow's `transcoding_session`):

```
if video == h264 && bitdepth == 8 && pix_fmt in {yuv420p, …}  -> passthrough (copy)
else if NVENC smoke-test passes                                -> h264_nvenc
else if AMF / QSV available                                    -> h264_amf / h264_qsv
else                                                            -> libx264 (veryfast)
```

## Requirements

- Windows 10/11 x64, WebView2 runtime (pre-installed on Windows 11)
- `ffmpeg` + `ffprobe` on `PATH` (winget: `Gyan.FFmpeg`)

## Build

```powershell
# GNU toolchain + WinLibs (no Visual Studio needed)
rustup toolchain install stable-x86_64-pc-windows-gnu
winget install BrechtSanders.WinLibs.POSIX.UCRT
.\build.ps1            # -> dist\airstream.exe
```

or manually:

```bash
cargo +stable-x86_64-pc-windows-gnu build --release
```

## Usage

1. Run `airstream.exe` → the window opens, playlist loads (imported from Airflow on first run).
2. `+ เพิ่มไฟล์` / `+ เพิ่มโฟลเดอร์` to add media.
3. Click a row to play in-app (seek with the bottom scrub bar, `←/→`, `Space`).
4. Click the **cast** button → pick a TV → the file is pushed to it over DLNA.

## API

| method | path | purpose |
|---|---|---|
| GET | `/api/state` | version, hardware support, LAN URL, playlist prefs |
| POST | `/api/add` | `{"paths": ["C:\\dir", "C:\\a.mkv"]}` |
| POST | `/api/pick_files`, `/api/pick_folder` | native Windows pickers |
| POST | `/api/item/:id/info` | probe one item (duration/codec/passthrough) |
| GET | `/api/thumb/:id` | JPEG thumbnail |
| GET | `/api/devices` | mDNS + SSDP discovery |
| POST | `/api/cast` | `{"device": "<id>", "id": "<item>"}` |
| POST | `/api/play` | starts an HLS session, returns playlist URL + mode |
| GET | `/api/stats` | encoder support + active HLS sessions |
| GET | `/stream/:id?t=sec` | fragmented MP4 (built-in player) |
| GET | `/media/:id` | original file, HTTP Range |
| GET | `/hls/:sid/index.m3u8` | HLS playlist |

## Roadmap

- Google Cast v2 (TLS 8009 + `cast_channel`) and AirPlay 2 sender (`/pair-setup`, `/pair-verify`)
- Adaptive ladder (`variable-720p`, rank manager) instead of a single rendition
- Multiple playlists / drag-and-drop reordering
- Subtitle selection and burning (ASS/DVD/PGS)
- Native `wry` window instead of Edge `--app`

## Licence

MIT — see [LICENSE](LICENSE).

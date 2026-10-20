# Fast start (v0.1.16)

Goal: press ▶ and the TV starts playing, including after the TV was switched off.

## Prepared before Play
- Selecting an item (or opening the app, which re-selects the last item) starts a
  **speculative preparation** with the exact audio, subtitle, quality and
  subtitle size the Play button would use. The readiness line under the title
  shows `กำลังเตรียมไฟล์ล่วงหน้า N%` and then `พร้อมเล่นทันที`.
- Only one speculative job runs. Selecting another file cancels it (ffmpeg is
  killed, the partial file removed). Pressing Play on the file being warmed
  promotes the running job instead of restarting it.
- Speculative preparation never writes preferences.
- The next episode is prepared as soon as the current one has played 20 s.
- `ffprobe` results are cached per file (path + size + mtime), single-flight.
- Prepared files are a cache keyed by source path, size, mtime, quality,
  encoder, audio, subtitle and subtitle size, so a different choice never
  reuses a wrong file.

## Cache size
`%LOCALAPPDATA%\Airstream\prepared` is trimmed to 30 GB (least recently played
first) at startup and after each preparation. The playing file and running
jobs are never removed. Only that app-owned folder is touched, never source media.

## TV switched off
A switched-off TV leaves a half-open socket. The Cast reader now treats 15 s of
silence (no reply to PING) as a lost connection, the UI shows `NO DEVICE`, and
▶ starts a fresh Cast that resumes from the last saved position.

## LAN safety
The HTTP server must be reachable by the TV, but only `/media`, `/prepared`,
`/subtitles`, `/stream` and `/hls` answer LAN peers. The control API answers
loopback only. `/prepared/<key>` accepts 16-hex keys only.

## Verification (E2E)
- `warm_e2e.py` (isolated headless backend, synthetic media): 18/18.
- `tv_latency_e2e.py` (real TV SKWAMX5M): warm Play → PLAYING 1.07 s; resume
  after backend restart from 24.1 s.

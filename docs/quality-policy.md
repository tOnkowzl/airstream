# Original-quality Cast policy (v0.1.4)

Default: **Original video + original audio + separate subtitles** (`passthrough`). Source quality is the ceiling; no artificial upscaling, sharpening, gain boosts, or audio enhancement is implied.

## Transport

- H.264 and HEVC are copied, not decoded/re-encoded, in Original mode. MKV is remuxed into a faststart MP4 for the tested Cast receiver, with HEVC tagged `hvc1`.
- AAC, AC-3, E-AC-3, ALAC and MP3 are transported without audio encoding or channel downmix. Receiver support varies; acceptance in the container is not a claim about every TV decoder or speaker system.
- Unsupported audio codecs fail explicitly rather than being silently converted to AAC. This can require selecting a compatible original audio track.
- Text subtitles are converted to UTF-8 WebVTT, served with CORS, declared in Cast `media.tracks`, and activated through `activeTrackIds`. Native subtitle scale and white outlined text are sent to the receiver.
- Native subtitle selection/size does not invalidate the copied-video cache. The cache includes source metadata and selected audio; subtitle content has its own cache.
- Bitmap PGS/DVD subtitles are not supported by the text path. ASS/SSA advanced typography, positioning, or effects may not survive WebVTT conversion. The app does not silently omit a selected unsupported subtitle.

## Explicit conversion mode

The separately labeled conversion modes are lossy, not Original quality. SDR burn-in uses NVENC p7 CQ10 (or software CRF12), with the existing bitrate cap. 10/12-bit and detected HDR sources are rejected in burn-in mode rather than silently rendered as 8-bit SDR. Preserve these sources through Original mode instead.

## Verification on the release executable

- Actual H.264/AC-3 Thai excerpt: elementary video and audio SHA-256 matched source after remux.
- Actual HEVC Main10/E-AC-3 8-channel excerpt: video and audio SHA-256 matched source; output remained `yuv420p10le`.
- On receiver SKWAMX5M, both excerpts reported PLAYING, the receiver requested the WebVTT endpoint, and active subtitle track ID 1 was confirmed.
- Real Edge UI E2E: immediately changing audio/subtitle and pressing Cast persisted those selections before LOAD; no JavaScript page errors.
- Generated black PQ/BT.2020 10-bit fixture: decoded-frame color metadata survived remux. This is a synthetic metadata transport test, not visual HDR/Dolby Vision certification on the TV.
- Unsupported eight-channel PCM was rejected with HTTP 415 instead of downmix/re-encode. Invalid subtitle indices returned 404.
- Explicit SDR burn-in still rendered Thai glyphs; media Range delivery matched the prepared file. Returning to Original/no-subtitle mode recovered source-identical decoded pixels.

These checks verify the sender and receiver protocol behavior for the tested inputs. They do not certify Atmos reproduction at speakers, all Dolby Vision/HDR profiles, every receiver, or universally fastest wall-clock startup. Startup can still require full-file remuxing; Original mode avoids full-film video encoding solely to display text subtitles.

## Playback speed (v0.1.6)

The remote includes x1/x1.25/x1.5/x1.75/x2/x3 buttons, matching the final requested choices (no x2.5). Speed can be selected without an active Cast session and is saved in settings. A new LOAD carries the preselected rate. Active playback uses native Cast `SET_PLAYBACK_RATE`, and the selected playing-rate indicator changes only after receiver confirmation. No media conversion is used to emulate speed. Validation, per-session serialization, error correlation and status readback prevent reporting an unsupported rate as success. Cast waits for a pending speed save, avoiding an immediate-click race.

Real receiver SKWAMX5M confirmed x1, x1.25, x1.5, x1.75 and x2. Tests verified offline selection, persistence through a full executable restart, initial LOAD at preselected x1.5, and clock advancement during playback. The receiver rejected x3 with `INVALID_REQUEST / INVALID_PARAMS`; the app reports the rejection and retains the receiver-confirmed playing speed. Other receivers may have different limits. Media bytes stayed unchanged.

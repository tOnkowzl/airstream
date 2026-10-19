# Backend lifetime and diagnostics (v0.1.15)

The backend follows the actual application windows, not JavaScript heartbeat timing:

- Opening the app starts the backend (or attaches to the existing instance).
- Minimizing/backgrounding the app keeps the backend and Cast control alive.
- Closing the final Airstream app window saves progress and exits the backend.
- Reopening starts a new backend. There is no permanently running service or new Windows autostart entry.

`src/window_lifetime.rs` enumerates actual Chromium app windows titled `Airstream`. It allows startup grace while the first window loads. Missing browser pings never cause shutdown. More than one app window keeps the same backend until the final one closes.

Cast PING messages are answered with PONG, and backend resume tracking requests media status independently of browser polling. A successful LOAD requires an actual media session in PLAYING; an optimistic nonzero resume position is not proof of playback readiness.

## Logs

`%LOCALAPPDATA%\Airstream\logs\backend.log` stores startup, window-close shutdown, Cast/transport requests, server errors and panic diagnostics. ANSI escape codes are disabled. On startup, logs larger than 8 MiB rotate to `backend.previous.log`.

## Focused verification

On the installed executable with the actual TV:

1. Preserve the receiver's actual current episode and position before updating.
2. Minimize the actual app for 18 seconds: the same backend PID remains alive and Cast stays connected.
3. Close the final app window: the backend process exits.
4. Reopen via the installed shortcut: a new backend starts and the original episode is restored at the current playback position.

No unrelated regression suite was rerun for this change.

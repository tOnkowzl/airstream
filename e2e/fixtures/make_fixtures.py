"""Generate small synthetic media into e2e/.work/media (idempotent). Needs ffmpeg on PATH."""
import os, subprocess, sys

NOWIN = 0x08000000
E2E = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MEDIA = os.path.join(E2E, ".work", "media")
AAC = os.path.join(MEDIA, "aac_thai.mkv")
EAC3 = os.path.join(MEDIA, "eac3_51.mkv")
SRT = "1\n00:00:01,000 --> 00:00:04,000\nสวัสดี\n\n2\n00:00:05,000 --> 00:00:09,000\nทดสอบซับไทย\n"


def _ffmpeg(out, audio_args):
    cmd = ["ffmpeg", "-v", "error", "-y",
           "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=60",
           "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=60",
           "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "50",
           *audio_args, "-shortest", out]
    subprocess.run(cmd, check=True, creationflags=NOWIN)


def make():
    os.makedirs(MEDIA, exist_ok=True)
    if not os.path.exists(AAC):
        _ffmpeg(AAC, ["-c:a", "aac", "-b:a", "128k", "-ac", "2"])
    with open(os.path.splitext(AAC)[0] + ".srt", "w", encoding="utf-8") as f:
        f.write(SRT)
    if not os.path.exists(EAC3):
        _ffmpeg(EAC3, ["-c:a", "eac3", "-ac", "6", "-b:a", "384k"])
    return {"aac": AAC, "eac3": EAC3}


if __name__ == "__main__":
    print(make())
    sys.exit(0)

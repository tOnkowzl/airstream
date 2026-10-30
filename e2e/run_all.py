"""Run every e2e/test_*.py sequentially (they share mock port 8443). Exit code != 0 on any failure."""
import glob, os, shutil, subprocess, sys, time

NOWIN = 0x08000000
E2E = os.path.dirname(os.path.abspath(__file__))
WORK = os.path.join(E2E, ".work")


def main():
    # fresh app data dirs; keep generated media and the TLS cert
    if os.path.isdir(WORK):
        for d in glob.glob(os.path.join(WORK, "app_*")):
            shutil.rmtree(d, ignore_errors=True)
    uv = shutil.which("uv")
    cmd = [uv, "run", "--quiet", "--with", "playwright", "python"] if uv else [sys.executable]
    tests = sorted(glob.glob(os.path.join(E2E, "test_*.py")))
    t_all = time.time()
    summary, failed = [], 0
    for t in tests:
        name = os.path.basename(t)
        print(f"===== {name}", flush=True)
        t0 = time.time()
        p = subprocess.Popen(cmd + [t], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                             encoding="utf-8", errors="replace", creationflags=NOWIN)
        npass = nfail = 0
        for line in p.stdout:
            line = line.rstrip()
            print(line, flush=True)
            if line.startswith("PASS "): npass += 1
            elif line.startswith("FAIL "): nfail += 1
        rc = p.wait()
        if rc != 0 and nfail == 0:
            nfail = 1
            print(f"FAIL {name} exited with code {rc} before reporting", flush=True)
        failed += nfail
        summary.append((name, npass, nfail, time.time() - t0))
    print("\n===== SUMMARY")
    for name, ok, bad, dt in summary:
        print(f"{'PASS' if bad == 0 else 'FAIL'}  {name:28s} {ok} passed, {bad} failed  ({dt:.0f}s)")
    tp = sum(s[1] for s in summary)
    print(f"TOTAL {tp} passed, {failed} failed in {time.time() - t_all:.0f}s")
    return 1 if failed or not tests else 0


if __name__ == "__main__":
    sys.exit(main())

"""Tiny check reporter shared by the test files (run_all.py parses PASS/FAIL lines)."""
import sys, time


class Checker:
    def __init__(self, suite):
        self.suite, self.results = suite, []

    def check(self, name, ok, detail=""):
        ok = bool(ok)
        self.results.append(ok)
        print(("PASS " if ok else "FAIL ") + name + (f" — {detail}" if detail else ""), flush=True)
        return ok

    def info(self, msg):
        print("INFO " + msg, flush=True)

    def finish(self):
        print(f"{sum(self.results)}/{len(self.results)} passed ({self.suite})", flush=True)
        sys.exit(0 if self.results and all(self.results) else 1)


def wait_until(fn, timeout, step=0.1):
    """Poll fn() until truthy; returns elapsed seconds or None on timeout."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        if fn():
            return time.time() - t0
        time.sleep(step)
    return None

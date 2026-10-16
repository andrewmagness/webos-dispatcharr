#!/usr/bin/env python3
"""Tests for `tools/luna-sub.py`, which ends a `luna-send -i` subscription that never closes.

`make install` piped the appinstalld subscription into `head -20`. On the dev set (webOS 6.5.3)
appinstalld stops reporting at `"installing : service installing"` and never sends `"installed"`,
while the subscription stays open, so the pipe never closed and the `make deploy` after it never
ran (2026-10-16: killed by a 600 s timeout with the install already landed). Each case here runs a
fake subscriber that prints a real recorded sequence and then hangs, as luna-send does.
"""
import os
import subprocess
import sys
import tempfile
import time
import unittest

TOOL = os.path.join(os.path.dirname(os.path.abspath(__file__)), "luna-sub.py")


def line(status, state, extra=""):
    return ('{"id":"com.example.app","statusValue":%d,"details":{"simpleStatus":"install",'
            '"state":"%s"%s}}' % (status, state, extra))


# The tail of the dev C1's real stream (webOS 6.5.3): no "installed" ever arrives.
C1 = [line(13, "installing : start"), line(17, "installing : installing"),
      line(27, "installing : service installing"), line(29, "installing : service installing")]
# The B8 (webOS 4.4.3) finishes the same stream with "installed".
B8 = C1 + [line(30, "installed")]


def run(lines, *args, hang=True, timeout=30):
    """Run the tool over a fake subscriber that prints `lines`, then sleeps (or exits)."""
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
        f.write("\n".join(lines) + "\n")
        path = f.name
    fake = ("import sys,time\n"
            f"for l in open({path!r}): sys.stdout.write(l); sys.stdout.flush()\n"
            + ("time.sleep(3600)\n" if hang else ""))
    t0 = time.monotonic()
    p = subprocess.run([sys.executable, TOOL, *args, "--", sys.executable, "-c", fake],
                       capture_output=True, text=True, timeout=timeout)
    os.unlink(path)
    return p, time.monotonic() - t0


class LunaSub(unittest.TestCase):
    def test_a_stream_that_reaches_the_done_state_ends_at_once(self):
        p, took = run(B8, "--done", "installed", "--quiet", "20")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertLess(took, 10, "it must not wait out the quiet period")
        self.assertIn('"state":"installed"', p.stdout, "every line is passed through")

    def test_a_stream_that_goes_quiet_without_finishing_ends_unconfirmed(self):
        """The C1's case: the subscriber never closes. The tool must still return."""
        p, took = run(C1, "--done", "installed", "--quiet", "2")
        self.assertEqual(p.returncode, 3, p.stderr)
        self.assertLess(took, 10, "it ends after the quiet period, not the hang")
        self.assertIn("quiet", p.stderr)

    def test_a_failure_ends_nonzero(self):
        bad = C1[:2] + ['{"returnValue":false,"errorCode":-1,"errorText":"FAILED_IPKG_INSTALL"}']
        p, _ = run(bad, "--done", "installed", "--quiet", "20")
        self.assertEqual(p.returncode, 1, p.stderr)
        self.assertIn("FAILED_IPKG_INSTALL", p.stderr)

    def test_an_install_failed_state_ends_nonzero(self):
        p, _ = run(C1[:2] + [line(31, "install failed")], "--done", "installed", "--quiet", "20")
        self.assertEqual(p.returncode, 1, p.stderr)

    def test_a_subscriber_that_exits_without_finishing_is_unconfirmed(self):
        p, _ = run(C1, "--done", "installed", "--quiet", "20", hang=False)
        self.assertEqual(p.returncode, 3, p.stderr)

    def test_the_deadline_bounds_a_stream_that_never_goes_quiet(self):
        chatty = ("import sys,time\n"
                  "while True: print('{\"state\":\"installing : installing\"}', flush=True); time.sleep(0.2)\n")
        t0 = time.monotonic()
        p = subprocess.run([sys.executable, TOOL, "--done", "installed", "--quiet", "5", "--deadline", "2",
                            "--", sys.executable, "-c", chatty], capture_output=True, text=True, timeout=30)
        self.assertEqual(p.returncode, 4, p.stderr)
        self.assertLess(time.monotonic() - t0, 10)


if __name__ == "__main__":
    unittest.main()

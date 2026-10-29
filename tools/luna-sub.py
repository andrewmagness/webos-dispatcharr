#!/usr/bin/env python3
"""Run a `luna-send -i` subscription and END it, which the subscription never does by itself.

    tools/luna-sub.py --done installed [--quiet 20] [--deadline 600] -- <command...>

`<command>` (in practice `tools/tv-ssh ssh tv 'script -qc "luna-send -i …"'`) is run in its own
process group and every line it prints is passed through. The subscription stays open after the
service has said all it will, so this decides when it is over and kills the command:

  exit 0  a line's `"state"` equals a `--done` state;
  exit 1  the service reported a failure (`"returnValue":false`, or a state naming a failure);
  exit 3  UNCONFIRMED: nothing for `--quiet` seconds after the first line, or the command exited,
          before a done state. The caller checks the outcome itself;
  exit 4  `--deadline` seconds passed without any of the above.

Why exit 3 exists: on the dev set (webOS 6.5.3) appinstalld's install subscription stops at
`"installing : service installing"` and never sends `"installed"`, though the install has landed;
webOS 4.4.3 does send it. `make install` piped this into `head -20`, which never saw EOF, so the
`make deploy` after it never ran (2026-10-16). `make install` answers exit 3 by checking that the
installed `appinfo.json` was rewritten.
"""
import argparse
import json
import os
import re
import select
import signal
import subprocess
import sys
import time

FAILED_STATE = re.compile(r"fail|error|cancel", re.I)


def verdict(text, done):
    """`"done"`, `"failed: <why>"` or None for one line of luna-send output."""
    try:
        msg = json.loads(text)
    except ValueError:
        return None
    if not isinstance(msg, dict):
        return None
    if msg.get("returnValue") is False:
        return "failed: %s" % (msg.get("errorText") or msg.get("errorCode") or text)
    state = (msg.get("details") or {}).get("state") if isinstance(msg.get("details"), dict) else None
    state = state or msg.get("state")
    if not isinstance(state, str):
        return None
    if state in done:
        return "done"
    if FAILED_STATE.search(state):
        return "failed: state %r" % state
    return None


def signal_group(pgid, sig):
    """Signal the subscriber's process group; a group with nothing left to signal is not an error.

    Linux returns ESRCH for a group that is gone. macOS returns EPERM when the group's only member
    is the subscriber itself, exited but not yet reaped — the case where it closed on its own.
    """
    try:
        os.killpg(pgid, sig)
    except (ProcessLookupError, PermissionError):
        pass


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--done", action="append", required=True, help="a state that ends it successfully")
    ap.add_argument("--quiet", type=float, default=20.0, help="seconds of silence that end it, unconfirmed")
    ap.add_argument("--deadline", type=float, default=600.0, help="seconds before giving up")
    ap.add_argument("command", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.command[1:] if a.command[:1] == ["--"] else a.command
    if not cmd:
        ap.error("give the command after --")

    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, start_new_session=True)
    fd = proc.stdout.fileno()
    start = time.monotonic()
    last = None  # the time of the last line; None until the first
    buf = b""
    code, why = 4, "deadline of %gs passed" % a.deadline
    try:
        while True:
            now = time.monotonic()
            if now - start >= a.deadline:
                break
            if last is not None and now - last >= a.quiet:
                code, why = 3, "quiet for %gs without a done state" % a.quiet
                break
            wait = a.deadline - (now - start)
            if last is not None:
                wait = min(wait, a.quiet - (now - last))
            ready, _, _ = select.select([fd], [], [], max(wait, 0.0))
            if not ready:
                continue
            chunk = os.read(fd, 65536)
            if not chunk:
                code, why = 3, "the subscription ended without a done state"
                break
            last = time.monotonic()
            buf += chunk
            lines = buf.split(b"\n")
            buf = lines.pop()
            ended = None
            for raw in lines:
                text = raw.decode("utf-8", "replace").rstrip("\r")
                print(text, flush=True)
                v = verdict(text, a.done)
                if v:
                    ended = v
                    break
            if ended == "done":
                code, why = 0, None
                break
            if ended:
                code, why = 1, ended
                break
    finally:
        signal_group(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            signal_group(proc.pid, signal.SIGKILL)
            proc.wait()
    if why:
        print("luna-sub: %s" % why, file=sys.stderr)
    return code


if __name__ == "__main__":
    sys.exit(main())

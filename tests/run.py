#!/usr/bin/env python3
"""
On-device regression harness for the Dispatcharr TV player.

Each case in manifest.json names a GENERATED clip (`make fixtures-pipeline`, written by
tests/fixtures/make_fixtures.py) and the Load payload declaration to play it with. This driver
serves the pack off this machine (tests/serve_fixtures.py) and, for each case:
  1. closes the running app on the TV (luna-send closeByAppId + fuser -k, via `make kill`);
  2. clears every dispatcharrtv-* trigger in THIS install's runtime root, then writes only the
     ones this case needs -- `dispatcharrtv-playurl` names the clip and carries the declaration;
  3. runs `make run-stream TV=<tv> FLAVOR=<f>`, which relaunches the app and tails its event
     log live;
  4. filters the `smp_cb type=43 num=0 str=` flood and evaluates the assertions CONTINUOUSLY as
     lines arrive, stopping the case the moment it passes -- the manifest's run_secs is the cap,
     not the runtime (see stream_case for why that is sound, and --no-early to turn it off);
  5. records PASS/FAIL with the failing evidence line.
Before the first case the set's Dispatcharr account is moved aside, and it is put back after the
last (see account_aside_cmd): with an account the app resumes its last channel on launch, and a
tuned channel outranks the clip trigger.

Two builds can sit on one television -- com.magness.dispatcharrtv, the app users install, and
com.magness.dispatcharrtv.debug beside it -- each with its own app directory, its own SAM id and its own
runtime root (/tmp for stable, /tmp/<app id> for a flavoured install). Every run here drives
exactly ONE of them: the flavour comes from --flavor, else the Makefile's own default, and every
path that follows is ASKED FOR rather than restated (`make -s print-rundir` / `print-eventlog`).
See resolve_flavour.

The TV ssh creds already live in the committed tools/tv-ssh (the published dev-mode password),
so device I/O shells out to `make` / tools/tv-ssh.

Usage:
  ./tests/run.py --list                 # list cases and the clips they play
  ./tests/run.py --build                # cargo + make + make deploy, then run all cases
  ./tests/run.py --filter hevc          # run only cases whose name contains "hevc"
  ./tests/run.py --flavor stable        # drive the OTHER install (see check_install first)
  ./tests/run.py                        # run every case (assumes app already deployed)

Exit code is nonzero if any selected case fails.

No third-party deps -- Python 3 stdlib only.
"""

import argparse
import atexit
import functools
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time

# ---------------------------------------------------------------------------
# Paths / constants
# ---------------------------------------------------------------------------
TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(TESTS_DIR)
TOOLS_DIR = os.path.join(REPO_ROOT, "tools")
MANIFEST = os.path.join(TESTS_DIR, "manifest.json")
MANIFEST_LOCAL = os.path.join(TESTS_DIR, "manifest.local.json")
TV_SSH = os.path.join(REPO_ROOT, "tools", "tv-ssh")  # key-first ssh/scp front door; see its header

sys.path.insert(0, TESTS_DIR)
from serve_fixtures import serve, default_root as serve_fixtures_default_root  # noqa: E402  (needs TESTS_DIR on the path first)
sys.path.insert(0, TOOLS_DIR)
import tv_config  # noqa: E402  (the TV address: tools/tv-config.sh is the one lookup)

# Where `make fixtures-pipeline` puts the generated pack. NOT inside the repo, and not merely by
# convention: the generator REFUSES an --out under the repo root, because this repository is
# public and .gitignore as the only defence against committing media has already been got wrong
# here once. The env var is the same one the Makefile reads, so `FIXTURES_OUT=... make
# fixtures-pipeline` and `FIXTURES_OUT=... ./tests/run.py` agree without a flag.
FIXTURES_ROOT = serve_fixtures_default_root()

# ---------------------------------------------------------------------------
# WHICH INSTALL this run drives. All four are resolved once, by resolve_flavour(), before anything
# is touched — and every one of them is ANSWERED BY THE MAKEFILE rather than computed here.
#
# That is the whole point: the flavour this harness resolved and the flavour it kills, launches and
# greps for are then one value and cannot disagree. Closing install A while launching install B
# reproduces SAM's stale-"running" no-op, and the run afterwards grades the other app's log — the
# "plausible wrong data" failure, not a clean one.
#
# (The Makefile spells the variable FLAVOR; this repo's prose, the Rust half and the overlay key
# spell the word flavour. Both spellings are deliberate, so neither side is being quoted wrong.)
FLAVOUR = None          # "stable" | "debug"
APPID = None            # com.magness.dispatcharrtv[.<flavour>]
RUNDIR = None           # the runtime root: /tmp for stable, /tmp/<app id> for a flavoured install
APPDIR = None           # the install's app directory on the television
EVENTLOG = None         # <RUNDIR>/dispatcharrtv-events.log
RUN_STREAM_MARK = None  # the remote command text — see _run_stream_pids()


# the type=43 spam filter (mirrors: grep -vaE "smp_cb type=43 num=0 str=$")
TYPE43_SPAM = re.compile(r"smp_cb type=43 num=0 str=\s*$")


def load_manifest(tv_override=None, for_listing=False):
    """The tracked case list plus the TV address to drive.

    Nothing here is installation-dependent, so there is no overlay: the TV address comes from
    `--tv`, else the gitignored `.tv-host` files or the per-user `~/.config/dispatcharrtv/tv-host`
    (`tools/tv-config.sh`, the same lookup the Makefile and `tools/` use).

    `for_listing` drops even that, and it is the difference between `--list` being offline and
    only claiming to be: `--list` opens no socket and touches no television, so it must work on a
    machine with no TV configured at all — a fresh clone, a fleet worktree, the CI runner.
    """
    with open(MANIFEST) as f:
        manifest = json.load(f)
    tv = tv_override or tv_config.host() or None
    if not tv and not for_listing:
        sys.exit("tests/run.py needs a TV address: put one in ~/.config/dispatcharrtv/tv-host (one "
                 "line, the IP), or pass --tv <ip>")
    if tv:
        manifest["tv"] = tv
    return manifest


def partition_skips(entries):
    """Split `entries` into (runnable, [(name, reason), …]).

    The counterpart to `_resolve_fixtures`, which is what sets the `skip` key.
    It must happen before ANY consumer subscripts `path`, because a skipped entry carries
    neither — deliberately, so a partition that is ever wrong raises KeyError naming the entry
    instead of driving the television at some sentinel. Held as (name, reason) pairs so a summary
    can say which shape an installation is missing, not merely how many.
    """
    return ([e for e in entries if not e.get("skip")],
            [(e["name"], e["skip"]) for e in entries if e.get("skip")])


def sh_squote(s):
    """POSIX single-quote a string for the one-line ssh command (names can contain apostrophes)."""
    return "'" + s.replace("'", "'\\''") + "'"


def ssh_argv(tv, remote_cmd):
    """The one SSH command shape used by blocking calls and streaming profiler helpers: through
    tools/tv-ssh, which tries this machine's key first and runs `sshpass` only if the set refuses
    it (host-key options, timeouts and silencing live there, once)."""
    return [TV_SSH, "ssh", f"root@{tv}", remote_cmd]


def ssh(tv, remote_cmd, timeout=30):
    """Run a command on the TV, the same way the Makefile does (tools/tv-ssh)."""
    cmd = ssh_argv(tv, remote_cmd)
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)


def make_argv(target_args):
    """The `make` command line every invocation in this file uses — FLAVOR included.

    The flavour is threaded HERE and not at the call sites, of which there are eight across four
    functions (kill x3, all, deploy, run, run-stream, and stream_case's Popen). One of them would
    be forgotten, and a `make kill` that forgot it closes the app users installed while this run
    drives the developer one — then grades a log nobody wrote. There is nothing to forget if the
    wrapper owns it.
    """
    return ["make", "-s", "-C", REPO_ROOT] + target_args + [f"FLAVOR={FLAVOUR}"]


def tv_awake(tv, timeout=8):
    """Does the television answer ssh at all. One probe, no lease, no side effects."""
    try:
        return ssh(tv, "true", timeout=timeout).returncode == 0
    except Exception:
        return False


def require_tv(tv, name):
    """**A sleeping television is not a test result.** Abort the run instead of grading it.

    This set drops to standby on its own (`.agents/skills/wake-tv/`), and when it does mid-suite
    EVERY assertion of every remaining case fails at once — `stream_path`, `load_decl`, `codec`,
    `video_bound`, `pos_climb`, `server_wire` — because the app never launched and the log is a
    boot banner. Measured 2026-08-28: a 19-case ABR tier reported **14 failures** that way, four
    cases in, and the shape of it is indistinguishable at a glance from a catastrophic regression
    in the code under test. It cost a full re-run to tell apart, and it is exactly the "plausible
    wrong data" the repo's one-television rule warns about — the damage is not a clean failure.

    So the run stops at the first case it cannot reach, naming the television rather than the
    change. It does NOT wake the set itself: waking is a lease-holding operation with its own
    skill, and a harness that silently resurrects the hardware hides how often this happens.
    """
    if tv_awake(tv):
        return
    sys.exit(
        f"\nTELEVISION UNREACHABLE before `{name}` — stopping rather than grading.\n"
        f"  Every assertion would fail at once and would read as a code regression.\n"
        f"  Wake it and re-run:  .agents/skills/wake-tv/wake-tv.sh\n"
    )


def make(target_args, timeout, capture=True):
    """Invoke a make target from the repo root (absolute cwd so nothing drifts)."""
    return subprocess.run(make_argv(target_args), capture_output=capture, text=True,
                          timeout=timeout)


def make_query(goals, flavour=None):
    """Ask the Makefile for derived values (`make -s print-<x> …`) — the only supported way.

    SEVERAL GOALS GO IN ONE INVOCATION, which is why this takes a tuple: the Makefile's query
    targets compose, its PURE_QUERY guard is satisfied as long as EVERY goal is a query, and one
    make start-up is cheaper than one per value — `resolve_flavour` was parsing a 900-line Makefile
    four times at harness start. `tools/stream-screen.py` already worked this way; this is the
    same helper, not a second one. Returns a list, one line per goal, in the order asked.

    NEVER `make -p` / `make -pn`, which is the obvious-looking alternative and is a trap: it prints
    a recursive variable's UNEXPANDED DEFINITION, so `TV` comes back as the literal
    `$(strip $(shell tools/tv-config.sh host ...))`, every ssh built from it fails, and the tool reports an
    unreachable television that is awake and answering (tools/tv-session.sh documents the same trap
    from the shell side). These goals are real echo recipes of real values, and the Makefile's
    PURE_QUERY guard keeps a query free of side effects.
    """
    goals = [goals] if isinstance(goals, str) else list(goals)
    cmd = ["make", "-s", "-C", REPO_ROOT, *goals] + ([f"FLAVOR={flavour}"] if flavour else [])
    shown = " ".join(cmd)
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as e:
        sys.exit(f"cannot ask the Makefile which install to drive (`{shown}`): {e}")
    if p.returncode != 0:
        # A bad flavour lands here: the Makefile's parse-time $(error) names the value and the
        # whitelist, which is a better message than anything this file could invent.
        sys.exit(f"`{shown}` failed:\n{(p.stderr or p.stdout).strip()}")
    out = p.stdout.strip().splitlines()
    if len(out) != len(goals):
        sys.exit(f"`{shown}` printed {len(out)} lines for {len(goals)} goals — is {REPO_ROOT} this repo?")
    return out[0] if len(goals) == 1 else out


def resolve_flavour(args, manifest):
    """Decide which install this run drives, then ask the Makefile for everything that follows.

    Order: --flavor, else the overlay's `flavour` key, else the Makefile's own default (which is
    `debug`, deliberately — the dangerous id has to be typed). Nothing below is computed from the
    flavour here. The app id, the runtime root and the event log come back from the same Makefile
    the launch will use, so this harness cannot arm triggers in one root and grade a log in
    another — and a future change to the naming rule reaches the harness for free.
    """
    global FLAVOUR, APPID, RUNDIR, EVENTLOG, APPDIR, RUN_STREAM_MARK
    # ONE invocation for all four. `print-flavor` comes back first and answers the default when
    # neither --flavor nor the overlay named one, so the old ask-then-ask-again round trip is gone.
    FLAVOUR = args.flavor or manifest.get("flavour") or ""
    FLAVOUR, APPID, RUNDIR, EVENTLOG, APPDIR = make_query(
        ("print-flavor", "print-appid", "print-rundir", "print-eventlog", "print-appdir"),
        FLAVOUR or None)
    # The tail `make run-stream` ends in. _run_stream_pids() matches it against `ps` to find this
    # harness's OWN ssh clients, so it has to be the text the Makefile actually runs: a copy that
    # stops matching reaps nothing, and every case then leaks an ssh client holding a remote
    # `tail -F` (125 of them piled up against the TV in one session the last time that happened).
    RUN_STREAM_MARK = f"tail -F -n +1 {EVENTLOG}"
    return FLAVOUR


# ---------------------------------------------------------------------------
# Trigger derivation
# ---------------------------------------------------------------------------
def triggers_for_case(case, url_base):
    """
    Map a case to the dispatcharrtv-* files to write in the TV's runtime root.
    Returns a list of (filename, content-or-None) pairs; None => `touch` (empty marker).

    A case names a generated file served over HTTP from this machine and declares its own Load
    payload (`dispatcharrtv-playurl`, `player::playurl::PlayUrl`).
    """
    # JSON, whole-file. `separators` drops the spaces `json.dumps` would put after ':' and ',':
    # they are legal JSON and the parser takes them, but this string is about to be printed in
    # the case header and pasted into issues, and short is legible. apply_triggers shell-quotes
    # the complete value rather than relying on what today's fields happen not to contain.
    spec = dict(case.get("declare", {}))
    spec["url"] = f"{url_base}/{case['fixture']}"
    files = [("dispatcharrtv-playurl", json.dumps(spec, separators=(",", ":"))),
             ("dispatcharrtv-stats", None)]
    # `dispatcharrtv-replay=<n>` — how many times a FINISHED playback restarts itself (LG #46).
    # Keyed off `expect.replays`, so the number the app is TOLD and the number the harness
    # GRADES are one statement. Absent => the trigger is not written at all.
    n = case.get("expect", {}).get("replays", 0)
    if n:
        files.append(("dispatcharrtv-replay", str(n)))
    gst_debug = case.get("gst_trace", {}).get("debug")
    if gst_debug:
        files.append(("dispatcharrtv-gstlog", gst_debug))
    # Arbitrary extra dev triggers, `{"name": content}` → `dispatcharrtv-<name>` in the runtime root
    # (`null` for a bare flag). For the one-run experiments a trigger exists for — `sinkmax`,
    # `nofps` — without teaching the harness a key per knob; the case's own triggers above win on
    # a name clash because they are written first and the app reads each name once.
    for name, content in (case.get("triggers") or {}).items():
        files.append((f"dispatcharrtv-{name}", content))
    return files


def apply_triggers(tv, files):
    """Clear every dispatcharrtv-* trigger in THIS install's runtime root (sparing the *.log files),
    then create the ones this case needs, in one ssh round-trip. GLOB-based, not an enumerated list,
    so a newly-added app trigger can never bleed between cases — a stale dispatcharrtv-novsync would
    uncap vsync, a stale dispatcharrtv-sinkmax would change the next case's Load.

    The glob is scoped to RUNDIR, which is what keeps the two installs out of each other's way: the
    stable root is /tmp itself, and `/tmp/dispatcharrtv-*` cannot match the flavoured root beside it
    (`/tmp/com.magness.dispatcharrtv.debug` — the separator is a DOT for exactly this reason, since a
    directory called `dispatcharrtv-debug` would read as an armed trigger to `dev::any_trigger_present`
    and silently suppress the other install's dev boot).
    """
    # The root has to EXIST and be world-writable before anything is written into it. Two uids
    # write here and neither can be made to go second: this ssh is ROOT and arms triggers before the
    # app has ever booted, while the app runs jailed under its own uid and creates its logs here.
    # Whoever creates the directory sets its mode — and umask masks mkdir's, hence the explicit
    # chmod — so an owner-only mode locks the other one out. A root-owned event log the app cannot
    # write stays 0 bytes, which every assertion in this file reports as "no line found", i.e.
    # exactly like a total regression. A no-op for the stable flavour, whose root is /tmp (1777).
    parts = [f"mkdir -p {RUNDIR} && chmod 1777 {RUNDIR}"]
    # wipe every trigger, keeping only the append-only logs (events/stderr/crash)
    parts.append(f'for f in {RUNDIR}/dispatcharrtv-*; do case "$f" in *.log) ;; *) rm -f "$f";; esac; done')
    # GST_DEBUG_FILE_OVERWRITE is honoured only once libpf installs its logger. Remove the old
    # trace here as well, before launch, so an app that fails before that point cannot be graded
    # against the previous case's perfectly plausible per-frame log.
    if any(name == "dispatcharrtv-gstlog" for name, _ in files):
        parts.append(f"rm -f {RUNDIR}/dispatcharrtv-gst.log")
    for name, content in files:
        if content is None:
            parts.append(f"touch {RUNDIR}/{name}")
        else:
            parts.append(f"printf '%s' {sh_squote(content)} > {RUNDIR}/{name}")
    ssh(tv, "; ".join(parts))


# Grades the codec NAME, not the raw AV_CODEC_ID: that enum renumbers between FFmpeg
# majors (H264 is 28 / 27, HEVC 174 / 173 / 172 across n3.3 / 6 / 9), so asserting the
# number quietly made this suite a test of which FFmpeg the app happened to be using.
RE_CODEC = re.compile(r"ff: v=#0 codec=(\S+) codec_id=(\d+)\s+(\d+)x(\d+)")
# the 1Hz media position carried on the render heartbeat (app.rs). Anchored to loop= so it can
# never pick up a `pos=` that some other line grows later. loop= and NOT fps=, because `pos=` sits
# between them on the line and because a paused-but-alive player still emits loop=.
RE_POS = re.compile(r"loop=\d+ .*?\bpos=(\d+)s")
# the `stream: host=.. path=<url>` line the demux logs when it opens the media URL -- the
# ground truth of direct-play (`/library/parts/..`) vs transcode (`/transcode/universal/..`).
# This is the ONLY universal decision signal: the smart/fast direct-play path in build_stream
# short-circuits and never logs a `decision:` line (that is emitted only when server_decision
# runs), so keying solely on `decision:` false-FAILs every plain direct-play case.
# NON-greedy up to the FIRST path= — the transcode URL is
# `path=/video/:/transcode/..start.mkv?path=%2Flibrary..`, and a greedy .* would grab the
# inner (URL-encoded) query param instead of the real request path.
RE_STREAM_PATH = re.compile(r"stream:.*?path=(\S+)")
# The Load payload's DECLARATION, as `player::engine` writes it once per streamed playback:
#   load: v=H264 a="AC3" fps=24.000 dv=present:0 P0/0 el:0 atmos:0
# This is the only place an event log says what the app told the television the stream WAS, as
# opposed to what the demuxer found in it. The two are independent, and a case sets the first one
# directly through its `declare` block.
RE_LOAD = re.compile(r'load: v=(\S+) a="([^"]*)" fps=([\d.]+) '
                     r'dv=present:(\d) P(-?\d+)/(-?\d+) el:(\d) atmos:(\d)'
                     r'(?: max=(\d+)x(\d+)@(\d+))?')
# The PIPELINE's echo of the sink envelope it was handed — `smp_cb type=5`, the one line in the
# log that is the television's reading of our `adaptiveStreaming` block rather than our own
# (`docs/webos10-lab-report.md` §3.2: `… video/x-h264 (null) (null) 3840 2160 (null) 60.000000 …`).
# `load_max` above grades what we MEANT to declare; this grades what the set SAW, and the
# `SMP loadCompleted` that has to follow it grades whether the set accepted it.
RE_SINK_ECHO = re.compile(r"smp_cb type=5 .*?video/x-(h264|h265|hevc)\b.*?\b(\d{3,4}) (\d{3,4}) \S+ ([\d.]+)")
# The audio LANE the demuxer actually fed, off the same `ff: v=#0 …` line: `a=#<index>`.
RE_ALANE = re.compile(r"ff: v=#0 .*\ba=#(-?\d+)")
# The RATIONAL the Load payload's esInfo actually carries — `engine::fps_rational`'s output, as
# opposed to the float that went into it. Emitted on every playback that declares a non-zero rate.
RE_ESINFO = re.compile(r"esInfo: videoFps (\d+/\d+)")
# GStreamer's monotonic debug timestamp. GST is used only for lxvideosink's per-picture cadence;
# Starfish type 4 below is the coded-resolution authority.
RE_GST_CLOCK = re.compile(r"(?<!\d)(\d+):(\d{2}):(\d{2})\.(\d{1,9})(?!\d)")
# Starfish callback type 4 is the decoder's source-info event. Unlike LG's GST caps logger, it
# reports every coded-size transition on the measured firmware (including a return to an earlier
# size), so it is the authoritative dynamic-resolution signal for this gate.
RE_SMP_SOURCE_INFO = re.compile(r"smp_cb type=4 num=-?\d+ str=(\{.*\})")
# The audio feeder logs the first four attempts and every 200th thereafter. reply=O is Starfish
# accepting a compressed audio AU: not proof a loudspeaker made sound, but the strongest event-log
# readiness fact available and exactly what the badly interleaved first fixture never reached.
RE_AUDIO_FEED = re.compile(r"\bfeed a#(\d+)\s+sz=(\d+)\s+fed=(-?\d+)\s+reply=(.)\s+qbytes=(\d+)")
def codec_ids(lines):
    """All (codec_name, width, height) from `ff: v=#0` lines, in order."""
    out = []
    for ln in lines:
        m = RE_CODEC.search(ln)
        if m:
            out.append((m.group(1), int(m.group(3)), int(m.group(4)), ln))
    return out


def playpos_secs(lines):
    """All `pos=<S>s` values from the once/sec render heartbeat, in order.

    Same SHARED.playpos_ns the /:/timeline reporter posts, but sampled at 1 Hz instead of
    every 10s (app.rs). Density is the whole point: seeing a 15s climb through 10s samples
    needs ~30s of playback, so the sparse signal charged every case roughly double its real
    floor. The app only emits the field while genuinely presenting frames, which is what
    keeps a direct-play resume's pre-roll 0 out of the series (see player::is_playing).
    """
    return [(int(m.group(1)), ln) for ln in lines for m in [RE_POS.search(ln)] if m]


def find(lines, needle):
    for ln in lines:
        if needle in ln:
            return ln
    return None


def a_codec(lines, expected, min_width, size=None):
    """What the DEMUXER found: the codec, and how big the picture is.

    `size` ("1920x1080") is an EXACT assertion and is what the resolution x codec matrix
    (LG App Self Checklist #50/#51) actually grades — `min_width` cannot tell 720x480 from
    720x576, and a matrix answered with "at least 1900 wide" is the "pieces are covered" answer
    that item is asking us to stop giving. Read out of `AVCodecParameters::width/height`, i.e.
    the CROPPED dimensions the container declares, so 1080 is 1080 and not the coded 1088.
    The two are independent: a case may declare either, both, or neither.
    """
    cs = codec_ids(lines)
    if not cs:
        return False, "no `ff: v=#0 codec=` line found"
    name, w, h, ln = cs[0]
    ok = (name == expected) and (w >= min_width)
    want = f"{expected} w>={min_width}"
    if size:
        ok = ok and (f"{w}x{h}" == size)
        want = f"{expected} {size}"
    return ok, f"codec={name} {w}x{h} (want {want}) :: {ln.strip()}"


def a_no_error(lines):
    for ln in lines:
        if "smp_cb type=18" in ln or "Playing error" in ln:
            return False, f"error surfaced :: {ln.strip()}"
    return True, "no `smp_cb type=18` / `Playing error`"


def a_video_bound(lines):
    """The video plane bound. `videoplane: BOUND` is the player machine's one edge line for it,
    whichever way the plane was bound; `setMediaVideoData sent` is the ACB arm's own step. The
    dev set (webOS 6.5.3) binds through the exported window and logs only the first, so grading
    the ACB line alone failed clips that played at 24 fps (2026-10-16)."""
    ln = find(lines, "videoplane: BOUND") or find(lines, "setMediaVideoData sent")
    return (ln is not None), (ln.strip() if ln else "no `videoplane: BOUND` (video plane never bound)")


def a_timeline_climb(lines, min_climb):
    """Media position advanced by >= min_climb seconds, read from the 1 Hz render heartbeat.

    This is the floor on every case and it is a real one: playback is 1x realtime, so a 15s
    climb can never cost less than 15s of wall clock.
    """
    ts = playpos_secs(lines)  # scanned once — the grade runs twice a second
    if len(ts) < 2:
        return False, f"only {len(ts)} media-position sample(s); need >=2 that climb"
    lo = min(t for t, _ in ts)
    hi = max(t for t, _ in ts)
    ok = (hi - lo) >= min_climb
    # The RATE rides along, reported on every case and asserted by none of them: a climb bound
    # says the clip got somewhere, and says nothing about how long it took.
    rate = ""
    mean_pm, worst_pm, _n, legs = playback_rate(lines)
    if mean_pm is not None:
        rate = f"; {mean_pm}pm of real time (worst 10s window {worst_pm}pm over {legs} leg(s))"
    return ok, f"heartbeat pos= {lo}s..{hi}s (climb {hi-lo}s, need >={min_climb}s) over {len(ts)} samples{rate}"


# ---- clip assertions ----
def a_stream_path(lines, fixture):
    """The demuxer opened THIS case's stream.

    What this catches is a case grading a stream it was never pointed at. Two ways it happens: a
    hand-armed trigger from a by-hand session (`apply_triggers` glob-wipes before every case, so
    that one needs effort to reach), and a tuned live channel, which the engine takes before the
    playurl trigger — the reason the account is moved aside for the run. Either way the case
    would grade some other stream's log, silently, and the assertion is one line.
    """
    for ln in lines:
        m = RE_STREAM_PATH.search(ln)
        if m:
            path = m.group(1).split("?", 1)[0]
            got = os.path.basename(path)
            return (got == fixture), f"opened {got!r} (want {fixture!r}) :: {ln.strip()}"
    return False, "no `stream: ... path=` line — the demuxer never opened anything"


def a_load_decl(lines, exp):
    """The Load payload declared what this case said it would.

    THE assertion that makes the declaration path mean anything, and the reason is a false PASS
    waiting to happen: the engine maps an unrecognised audio codec through a `_ =>` arm to `"AC3"`
    and a non-"hevc" video codec to the H264 payload — so a trigger that never got read at all
    (unplumbed, misparsed, wiped by a stale glob) produces EXACTLY the right payload for the AC-3
    baseline case, which then passes and proves nothing. That is why the tier carries cases whose
    expected `load_audio` is "AC3 PLUS" and "AAC": they cannot be reached by accident.
    """
    hit = next((m for ln in lines for m in [RE_LOAD.search(ln)] if m), None)
    if hit is None:
        return False, "no `load:` line — nothing was declared (or the binary predates it)"
    line = hit.string.strip()
    v, a, fps, present, prof, blc, _el, atmos, max_w, max_h, max_fps = hit.groups()
    # "P8/1" — profile/bl_compat, or "none". One token because the two are only ever meaningful
    # together: a profile with the wrong compatibility id describes a different file.
    dv = "none" if present == "0" else f"P{prof}/{blc}"
    checks = [("load_video", "v", v), ("load_audio", "a", a), ("load_dovi", "dv", dv),
              ("load_atmos", "atmos", atmos == "1")]
    got = []
    for key, label, actual in checks:
        if key not in exp:
            continue
        want = bool(exp[key]) if key == "load_atmos" else exp[key]
        got.append(f"{label}={actual!r}")
        if actual != want:
            return False, f"declared {label}={actual!r}, want {want!r} :: {line}"
    if "load_fps" in exp:
        # The float the conversion started from. Tolerance is what three decimals can express.
        want_fps = float(exp["load_fps"])
        got.append(f"fps={fps}")
        if abs(float(fps) - want_fps) > 0.005:
            return False, f"declared fps={fps}, want {want_fps:.3f} :: {line}"
    if "load_fps_rational" in exp:
        # ...and the RATIONAL it produced, which is the thing the frame-rate fixtures exist for.
        # `engine::fps_rational` has one branch for the 1001-denominator broadcast rates and
        # another for the integers, and grading only the float above would grade the INPUT to that
        # split — leaving the branch a fixture was built to reach unobserved. It also pins the two
        # copies of the broadcast-rate table (this repo has one in Rust and one in the generator's
        # `_rate_arg`) against each other at the only place they can meet: the wire.
        rat = next((m.group(1) for ln in lines for m in [RE_ESINFO.search(ln)] if m), None)
        if rat is None:
            return False, f"no `esInfo: videoFps` line — the rate never reached the payload :: {line}"
        got.append(f"esInfo={rat}")
        if rat != exp["load_fps_rational"]:
            return False, f"esInfo videoFps {rat}, want {exp['load_fps_rational']} :: {line}"
    if "load_max" in exp:
        # The `adaptiveStreaming` ceiling the app declared — `WxH@F`. Until 2026-09-03 this was a
        # 4K60 constant for every stream; now it is derived per session (`engine::sink_envelope`),
        # and the cases that carry this key pin each cell of that rule on the device.
        if max_w is None:
            return False, f"no `max=` on the load line — the binary predates the envelope :: {line}"
        declared = f"{max_w}x{max_h}@{max_fps}"
        got.append(f"max={declared}")
        if declared != exp["load_max"]:
            return False, f"declared max={declared}, want {exp['load_max']} :: {line}"
    if "sink_echo" in exp:
        # What the TELEVISION says it was handed, and whether it then completed the Load. The
        # pipeline echoes the sink as `type=5` before it decides; `SMP loadCompleted` after it is
        # the acceptance. A refusal is `type=18` after the echo and `no_playing_error` catches it,
        # but an echo that names a different envelope than `load_max` is a splice bug this alone
        # can see.
        echo_at = next(((i, m) for i, ln in enumerate(lines)
                        for m in [RE_SINK_ECHO.search(ln)] if m), None)
        if echo_at is None:
            return False, f"no `smp_cb type=5` sink echo — the pipeline never read the envelope :: {line}"
        i, m = echo_at
        _codec, ew, eh, efps = m.groups()
        echoed = f"{ew}x{eh}@{int(float(efps))}"
        got.append(f"echo={echoed}")
        if echoed != exp["sink_echo"]:
            return False, f"the television echoed {echoed}, want {exp['sink_echo']} :: {m.group(0).strip()}"
        if not any("SMP loadCompleted" in ln for ln in lines[i:]):
            return False, f"sink echoed {echoed} but no `SMP loadCompleted` followed it"
    if not got:
        return False, f"case declares no load_* expectation to grade :: {line}"
    return True, f"declared {' '.join(got)} :: {line}"


def a_audio_lane(lines, want_idx):
    """The demuxer fed the audio stream the declaration named.

    `ff::audio_stream_matching` picks the first audio stream whose codec equals the Load payload's
    audio codec — deliberately, because `av_find_best_stream` picks "highest quality" and on an
    8-track file chose DTS over the AC-3 the payload declared, which leaves the audio ES
    unconfigured and, with audioSync, wedges the video forever. That selection is a pure function
    of the declaration and the file, which is exactly what this tier can vary one at a time: one
    fixture with three audio codecs, two cases declaring different ones, two different lanes.
    """
    hit = next((m for ln in lines for m in [RE_ALANE.search(ln)] if m), None)
    if hit is None:
        return False, "no `ff: v=#0 … a=#` line — no audio lane was chosen"
    got = int(hit.group(1))
    return got == want_idx, (f"fed audio lane a=#{got} (want a=#{want_idx}) :: "
                             f"{hit.string.strip()}")


def a_load_count(lines, want):
    """An adaptive coded-size change must stay inside one Starfish Load/session."""
    got = sum(bool(RE_LOAD.search(ln)) for ln in lines)
    return got == want, f"saw {got} Load declaration(s), want exactly {want}"



def early_exit_allowed(case, cfg):
    """(may this case stop as soon as every assertion holds, why not).

    Early exit is sound only because assertions are monotone once satisfied: more time cannot
    un-satisfy them. `gst_trace` breaks that: GST_DEBUG_FILE is not tailed by run-stream, so those
    assertions can only be graded after the run, and an event-log-only partial grade must not end
    the case first.
    """
    if cfg.get("no_early"):
        return False, ""                      # the operator already said so; no need to explain
    if case.get("gst_trace"):
        return False, "gst_trace assertions are graded only after the run"
    return True, ""


# **Above this a forward jump is a RELOCATION, not a delivery.** A reload or a replay moves the
# media clock by much more than a lump; a queue running dry moves it by a few seconds at most.
LUMP_SEEK_S = 10


def _runs(series, is_bad):
    """`(runs, hits)` — the lengths of every maximal run of CONSECUTIVE bad beats, and the total.

    `playback_stalls` and `playback_lumpiness` ask the same question of the same `pos=` series and
    differ only in what makes a beat bad, so the walk is here once. Both had their own copy,
    including the trailing flush — the half that is easy to fix in one and forget in the other.
    """
    runs, cur, hits = [], 0, 0
    for before, after in zip(series, series[1:]):
        if is_bad(before, after):
            cur += 1
            hits += 1
        elif cur:
            runs.append(cur)
            cur = 0
    if cur:
        runs.append(cur)
    return runs, hits


def playback_stalls(lines):
    """`(max_continuous_s, total_s, samples)` of REAL playback stall, from the 1 Hz `pos=` series.

    A stall is the media clock failing to advance between two consecutive heartbeats. It is NOT
    the starvation horizon, not `buffered < threshold`, and not the controller's `starving()`
    boolean — those are the model's opinion about the future, and this is what happened.

    RESOLUTION IS +/-1 s AND CANNOT BE BETTER FROM THIS SOURCE: `pos=` is integer seconds on a
    once-per-second heartbeat (`RE_POS`), so a sub-second gap is invisible and a 1 s reading may be
    rounding. Quote it as a floor on stall duration, never as a precise one.
    """
    series = [p for p, _ in playpos_secs(lines)]
    if len(series) < 2:
        return None, None, len(series)
    runs, _ = _runs(series, lambda before, after: after <= before)
    return (max(runs) if runs else 0), sum(runs), len(series)


def playback_lumpiness(lines):
    """`(lumpy_beats, longest_run, beats)` — the clock arriving in SEGMENT-SIZED lumps.

    A third failure shape, and the only one of the three that every other metric on this line
    reads as healthy. `playback_stalls` grades the clock STOPPING and `playback_rate` grades it
    advancing too SLOWLY; this grades it advancing in the right amount at the wrong granularity —
    `2,0,2,0,2,0` instead of `1,1,1,1,1,1`. The mean rate over that is exactly 1000 pm, no beat
    is a stall longer than one, and the picture is visibly juddering.

    Device-measured (`pipe_abr_down_outrun`, 2026-08-28): after three downshift attempts drained
    the reserve to 168 ms, the media clock ran `123 125 125 127 129 129 131 133 133 …` for ~30
    beats. `playback_stalls` scored max=8 total=13 — all of it the earlier true stall — and
    `playback_rate` scored ~1000 pm. Both were right and both were blind: playback was running
    straight off the network one 2 s segment at a time, because the queue was empty and each
    arrival advanced the clock by a whole segment.

    A beat is LUMPY when the clock moved by 2 s or more in one 1 Hz beat — i.e. it delivered at
    least a whole extra segment's worth in the time one second of media should have taken. That
    threshold is not a tuning knob: 2 is the smallest integer step this ±1 s source can
    distinguish from real time at all (a true 1 s advance may read 0 or 1, never 2).

    A seek moves the clock discontinuously and is not lumpiness; `playback_rate`'s leg splitting
    is the wrong tool here because a lump IS a forward step, so this instead ignores any step
    larger than `LUMP_SEEK_S`, above which a jump is a relocation rather than a delivery.
    """
    series = [p for p, _ in playpos_secs(lines)]
    if len(series) < 2:
        return None, None, len(series)
    runs, lumpy = _runs(series, lambda before, after: 2 <= after - before <= LUMP_SEEK_S)
    return lumpy, (max(runs) if runs else 0), len(series)


def playback_rate(lines, window=10):
    """Media seconds per WALL second — whether the film is running at speed.

    Returns `(mean_pm, worst_pm, beats, legs)` in per mille of real time, or `(None, None, n, 0)`
    when there is not enough of a series to say. 1000 is real time; 670 is the picture crawling.

    **This is the only thing here that can see a slow film.** Every other metric on this line is a
    RESERVE, and a reserve is media time measured against the playhead — so when the playhead
    itself slows down, the reserve stops draining, `slope` goes quiet, and `min_buf_ms` reads
    healthy while the picture runs at two thirds speed. `max_stall_s` cannot see it either: a
    stall is the clock not advancing at all, and this is the clock advancing too slowly.
    Measured on the corpus 2026-08-27: `pipe_abr_band_20000` holds 670 for ~30 s with `buf`
    parked at 2.2 s, `slope` decaying to −29 ms/s and every buffer signal reading fine.

    The wall clock is the HEARTBEAT COUNT: `app.rs`'s `loop_tick` emits one line per wall second
    whatever the frame rate, so beats are seconds. Arrival stamps are used instead when `lines`
    carries them (a live `StampedLines`), because they measure wall time directly and survive a
    beat the ssh hop dropped; a log read back off disk has none and falls back to the count.

    RESOLUTION: `pos=` is INTEGER seconds (`RE_POS`), so a single beat is ±1 s and only a window
    is meaningful. Over `window` beats the quantization is ±1000/(window−1) pm — ±111 pm at the
    default 10 — which is why nothing shorter is reported and why a bound below ~900 is the only
    kind this can carry honestly.

    A seek or a reload moves the media clock discontinuously and is NOT a rate. Legs are split on
    any backward step, and on a forward step larger than the wall gap plus one second of slack;
    each leg is measured on its own and legs shorter than `window` are dropped.
    """
    stamps = getattr(lines, "stamps", None)
    beats = []
    for i, ln in enumerate(lines):
        m = RE_POS.search(ln)
        if m:
            beats.append((int(m.group(1)),
                          stamps[i] if stamps and i < len(stamps) else float(len(beats))))
    if len(beats) < window:
        return None, None, len(beats), 0
    legs, cur = [], []
    for b in beats:
        if cur:
            d_media, d_wall = b[0] - cur[-1][0], b[1] - cur[-1][1]
            if d_media < 0 or d_media > d_wall + 1.0:
                legs.append(cur)
                cur = []
        cur.append(b)
    legs.append(cur)
    legs = [l for l in legs if len(l) >= window]
    if not legs:
        return None, None, len(beats), 0
    worst, media, wall = None, 0.0, 0.0
    for leg in legs:
        media += leg[-1][0] - leg[0][0]
        wall += leg[-1][1] - leg[0][1]
        for i in range(len(leg) - window + 1):
            a, b = leg[i], leg[i + window - 1]
            span = b[1] - a[1]
            if span <= 0:
                continue
            r = int(round(1000 * (b[0] - a[0]) / span))
            if worst is None or r < worst:
                worst = r
    mean = int(round(1000 * media / wall)) if wall > 0 else None
    return mean, worst, len(beats), len(legs)


def report_and_record(cfg, name, passed, results, lines, elapsed, run_secs, early, settled,
                     verbose):
    """Save the log, print the verdict, print the characterisation."""
    saved = save_case_log(cfg, name, lines)
    if saved:
        print(f"    log saved: {saved} ({len(lines)} lines)")
    report_case(passed, results, elapsed, run_secs, early, settled, verbose)
    notes = characterisation(lines)
    if notes:
        print("    characterisation (recorded, not graded):")
        for note in notes:
            print(f"       {note}")


# The video sink's own PRESENTED-frame counter, forwarded by libpf every 200 ms as callback
# type 47 (`non-flushable-displayed-frames`, armed by the payload's
# `streamQualityInfoNonFlushable`) and its dropped counter as type 46 (`dropped-frames`, armed by
# `streamQualityInfo`, emitted only when non-zero). Decompiled 2026-09-03; the only instrument on
# a non-Dolby path that counts frames the sink displayed rather than ticks of a position clock.
RE_SINK_LINE = re.compile(r"^(?:(\d+)\s+)?sink: (displayed|dropped)=(-?\d+)\b")
# The RAW webOS-4 types, for logs written before the app logged the `sink:` line. Only consulted
# when no `sink:` line exists in the log at all — on a webOS 5+ set the same counters arrive as
# 48/49 and only the app-side normalisation can say so.
RE_SINK_COUNT = re.compile(r"^(?:(\d+)\s+)?smp_cb type=(46|47) num=(-?\d+)\b")


def sink_counter_events(lines):
    """(kind, value, stamp_ms|None) per counter line, kind in {'displayed','dropped'}: the app's
    firmware-normalised `sink:` line when the log has one, else the raw webOS-4 types."""
    out = []
    for ln in lines:
        m = RE_SINK_LINE.search(ln)
        if m:
            out.append((m.group(2), int(m.group(3)), int(m.group(1)) if m.group(1) else None))
    if out:
        return out
    for ln in lines:
        m = RE_SINK_COUNT.search(ln)
        if m:
            kind = "dropped" if m.group(2) == "46" else "displayed"
            out.append((kind, int(m.group(3)), int(m.group(1)) if m.group(1) else None))
    return out


def sink_counter_intervals(values):
    """The type-47 series as PER-INTERVAL frame counts, whatever shape the firmware gave it.

    Two shapes exist: this firmware's per-interval count (a small number that goes up and down,
    or sits flat at 5 for a 24p stream) and a cumulative total (strictly increasing at nearly
    every poll, and soon far beyond anything one 200 ms poll could show). Monotonicity alone
    cannot tell them apart — a flat `5,5,5,…` is monotone — so cumulative means: at least four
    steps in five strictly increase AND the series climbs past 20, which no per-interval read
    reaches below 100 fps. A cumulative total that DECREASES restarted (a second Load resets the
    sink): that step is read as a boundary — the new value is the count since the reset — rather
    than reclassifying the whole run. Returns (intervals, cumulative)."""
    if len(values) < 3:
        return list(values), False
    steps = list(zip(values, values[1:]))
    rising = sum(1 for a, b in steps if b > a)
    falling = sum(1 for a, b in steps if b < a)
    # Flat steps are evidence of nothing: a cumulative total sits flat through a pause or a stall
    # exactly as a per-interval count sits flat at 5. Only the steps that MOVE say which shape
    # this is — a cumulative counter almost never moves down (one reset per Load), a
    # per-interval one moves down as often as up.
    moving = rising + falling
    cumulative = rising >= 3 and rising * 5 >= moving * 4 and max(values) > 20
    if not cumulative:
        return list(values), False
    out, prev = [], None
    for v in values:
        out.append(0 if prev is None else (v - prev if v >= prev else v))
        prev = v
    return out, True


def presented_fps(lines):
    """(fps over the whole span, worst 1 s window, sample count, dropped, cumulative) from the
    type-47/46 counters, or None when fewer than two type-47 samples exist. Timestamps are the
    log's own millisecond stamps when present, else the 200 ms poll period is assumed; the
    counter's shape is decided by `sink_counter_intervals`."""
    shown, dropped = [], 0
    for kind, n, ms in sink_counter_events(lines):
        if kind == "dropped":
            dropped = max(dropped, n)
        else:
            shown.append((ms, n))
    if len(shown) < 2:
        return None
    if any(t is None for t, _ in shown):
        shown = [(i * 200, n) for i, (_, n) in enumerate(shown)]
    intervals, cumulative = sink_counter_intervals([n for _, n in shown])
    stamped = list(zip([t for t, _ in shown], intervals))
    # per-poll counts normalised by each poll's real length (nominally 200 ms)
    per_s = []
    for (t0, _), (t1, n) in zip(stamped, stamped[1:]):
        dt = (t1 - t0) / 1000.0
        if dt > 0:
            per_s.append(n / dt)
    if not per_s:
        return None
    mean = sum(per_s) / len(per_s)
    # worst 1 s window: sum of polls inside any 1 s span, over its span
    worst = None
    j = 0
    for i, (ti, _) in enumerate(stamped):
        while j < len(stamped) and stamped[j][0] - ti < 1000:
            j += 1
        if j < len(stamped) and stamped[j][0] > ti:
            w = sum(n for _, n in stamped[i + 1:j + 1]) / ((stamped[j][0] - ti) / 1000.0)
            worst = w if worst is None else min(worst, w)
    if worst is None:
        # shorter than one second: the worst single poll is the only "window" there is
        worst = min(per_s)
    return round(mean, 1), round(worst, 1), len(shown), dropped, cumulative

RE_HEART = re.compile(r"loop=\d+ .*?\bpos=(\d+)s(?: play=(-?\d+)pm)?")


def presented_windows(lines):
    """Presented frames per second, one number per HEALTHY wall second, from the sink counter.

    A window is the stretch between two consecutive heartbeats whose media position advanced by
    one second (±1 s of integer quantisation) and whose `play=` — where the line carries one —
    reads real time (900..1100 pm). That is what excludes pre-roll, a pause, a seek flush and a
    stall WITHOUT reading any of those events: a window in which the film did not run at speed is
    not a window in which the sink was asked to show `declared` frames. Inside a window the
    type-47 samples (nominally five, at 200 ms) are summed and normalised to five; a window with
    fewer than four samples is dropped as a torn read. A cumulative counter is differenced first.
    Returns (windows, sample_count)."""
    events = []  # (kind, value) in log order: ("beat", pos, play) | ("shown", n)
    use_raw = not any(RE_SINK_LINE.search(ln) for ln in lines)
    for ln in lines:
        m = RE_SINK_LINE.search(ln)
        if m:
            if m.group(2) == "displayed":
                events.append(("shown", int(m.group(3))))
            continue
        if use_raw:
            m = RE_SINK_COUNT.search(ln)
            if m and m.group(2) == "47":
                events.append(("shown", int(m.group(3))))
                continue
        h = RE_HEART.search(ln)
        if h:
            events.append(("beat", int(h.group(1)), None if h.group(2) is None else int(h.group(2))))
    shown = [e[1] for e in events if e[0] == "shown"]
    intervals, _cumulative = sink_counter_intervals(shown)
    it = iter(intervals)
    events = [("shown", next(it)) if e[0] == "shown" else e for e in events]
    windows, bucket, last_beat = [], [], None
    for e in events:
        if e[0] == "shown":
            bucket.append(e[1])
            continue
        _, pos, play = e
        if last_beat is not None:
            lpos, lplay = last_beat
            healthy = 1 <= pos - lpos <= 2 and (play is None or 900 <= play <= 1100) \
                and (lplay is None or 900 <= lplay <= 1100)
            if healthy and len(bucket) >= 4:
                windows.append(sum(bucket) * 5.0 / len(bucket))
        bucket, last_beat = [], (pos, play)
    return windows, len(shown)


def a_presented_rate(lines, exp):
    """The frame rate the sink PRESENTED matches the rate the app DECLARED.

    Graded by default on every playback case since 2026-09-03, because the two instruments the
    suite already had were both blind to the failure this catches: a 4K H.264 24p stream declared
    at `maxFrameRate: 60` was presented at 13 fps while the media clock ran at real time
    (`play_rate` 1000 pm), the heartbeat's `fps=` sat at 60 (our GL swaps) and the pipeline's own
    drop counter (type 46) stayed at 0 — the sink does not count a frame it never presented. The
    maintainer saw it by eye; nothing here could. The number comes from the app's `sink:
    displayed=` line (raw callback 47 on webOS 4, 49 on 5+), the sink's `non-flushable-displayed-frames` polled by libpf every 200 ms once the Load payload
    says `streamQualityInfoNonFlushable` (decompiled from libpf, `player/CLAUDE.md`).

    The declared rate is the `load:` line's `fps=` — what the app told the television the stream
    runs at. A transcode declares no rate (`fps=0.000`) and is SKIPPED, honestly: there is
    nothing to match against until the route carries the source rate. `expect.presented_rate:
    false` opts a case out; `presented_rate_tol_pct` (default 6) widens the band.

    Two bounds on the healthy windows (see `presented_windows`): the MEDIAN within ±tol of the
    declared rate — one bad second is a hitch, half of them is the lattice — and no more than one
    window in five below 85 %. At least five healthy windows are required, so under early exit a
    case has to have played five clean seconds before this can be satisfied; a degradation after
    that is unobserved, like every other floor here, and `--no-early` is the longer look."""
    if exp.get("presented_rate") is False:
        return True, "opted out"
    hit = next((m for ln in lines for m in [RE_LOAD.search(ln)] if m), None)
    if hit is None:
        return False, "no `load:` line — nothing was declared"
    declared = float(hit.group(3))
    if declared <= 0:
        return True, "skipped: the app declared no rate (fps=0.000 — a transcode), nothing to match"
    windows, samples = presented_windows(lines)
    if samples == 0:
        return False, ("no `sink: displayed=` counter line (nor a raw webOS-4 type=47) — the Load "
                       "payload lacks streamQualityInfoNonFlushable, or the binary predates it")
    if len(windows) < 5:
        return False, (f"only {len(windows)} healthy window(s) with a counter read ({samples} samples)"
                       " — need five clean seconds of playback to grade the presented rate")
    tol = float(exp.get("presented_rate_tol_pct", 6)) / 100.0
    ordered = sorted(windows)
    median = ordered[len(ordered) // 2]
    low = sum(1 for w in windows if w < 0.85 * declared)
    ev = (f"declared {declared:.3f} fps; presented median {median:.1f}, min {ordered[0]:.1f}, "
          f"max {ordered[-1]:.1f} over {len(windows)} healthy s ({samples} counter samples); "
          f"{low} window(s) below 85%")
    if not (declared * (1 - tol) <= median <= declared * (1 + tol)):
        return False, f"presented median {median:.1f} fps is outside ±{tol*100:.0f}% of the declared {declared:.3f} :: {ev}"
    if low * 5 > len(windows):
        return False, f"{low}/{len(windows)} windows below 85% of the declared rate :: {ev}"
    return True, ev


def characterisation(lines):
    """What every case prints and never grades: whether the clip actually ran.

    `report_case` prints an assertion's evidence only when it FAILS (or under `--verbose`), so a
    case that stalled for three seconds and still satisfied every bound printed nothing about the
    stall. A stall is a fact about the playback, so it prints on every case, pass or fail.
    """
    out = []
    stall_max, stall_total, beats = playback_stalls(lines)
    if stall_max is not None:
        mean_pm, worst_pm, _n, legs = playback_rate(lines)
        rate = "n/a" if mean_pm is None else f"{mean_pm}pm mean / {worst_pm}pm worst over {legs} leg(s)"
        # +/-1 s and a floor, never a precise duration: `pos=` is integer seconds on a 1 Hz
        # heartbeat, so a sub-second stall is invisible. Said at the point of reading.
        # Three shapes, printed together because each is blind to the other two: the clock
        # STOPPING, the clock running SLOW, and the clock arriving in LUMPS at a correct mean rate.
        lumpy, lump_run, _ = playback_lumpiness(lines)
        out.append(
            f"playback: max_stall={stall_max}s (total {stall_total}s over {beats} beats, +/-1s) "
            f"rate={rate} lumpy={lumpy} beat(s), longest run {lump_run}"
        )
    shown = presented_fps(lines)
    if shown is not None:
        mean, worst, n, dropped, monotone = shown
        out.append(
            f"presented: {mean} fps mean, worst 1s window {worst} fps "
            f"(sink displayed-frames counter, {n} samples, "
            f"{'cumulative' if monotone else 'per-interval'}) dropped={dropped}"
        )
    return out


def a_audio_feed_ready(lines):
    """At least one compressed audio AU reached and was accepted by Starfish."""
    attempts = [m for line in lines for m in [RE_AUDIO_FEED.search(line)] if m]
    accepted = [m for m in attempts if m.group(4) == "O"]
    if accepted:
        last = accepted[-1]
        return True, (f"Starfish accepted audio feed a#{last.group(1)} "
                      f"({last.group(2)} bytes, pts={int(last.group(3)) / 1e9:.3f}s)")
    if attempts:
        replies = "".join(m.group(4) for m in attempts)
        return False, f"audio feed was attempted but no logged AU was accepted (replies={replies})"
    return False, "no `feed a#… reply=O` line — the audio lane never reached Starfish"


def _source_info_shape(node):
    """Find the `video.width/height` pair inside a type-4 JSON envelope."""
    if not isinstance(node, dict):
        return None
    video = node.get("video")
    if isinstance(video, dict):
        try:
            return f"{int(video['width'])}x{int(video['height'])}"
        except (KeyError, TypeError, ValueError):
            pass
    for value in node.values():
        shape = _source_info_shape(value)
        if shape:
            return shape
    return None


def starfish_resolution_sequence(lines):
    """Collapsed WxH sequence from valid Starfish source-info callbacks."""
    got = []
    for line in lines:
        match = RE_SMP_SOURCE_INFO.search(line)
        if not match:
            continue
        try:
            shape = _source_info_shape(json.loads(match.group(1)))
        except json.JSONDecodeError:
            continue
        if shape and (not got or got[-1] != shape):
            got.append(shape)
    return got


def a_starfish_resolution_sequence(lines, want):
    """The decoder itself must observe every in-band coded-size transition."""
    got = starfish_resolution_sequence(lines)
    return got == want, (f"Starfish source-info {' -> '.join(got) if got else '<none>'}; "
                         f"want exactly {' -> '.join(want)}")


def a_no_reload(lines):
    """Reject either reload path even when a later session recovers and otherwise passes."""
    bad = next((ln for ln in lines
                if "reload_at:" in ln or "reload_transcode:" in ln), None)
    return bad is None, ("no reload path entered" if bad is None else
                         f"session reloaded :: {bad.strip()}")


def gst_clock_ms(line):
    """GStreamer debug's H:MM:SS.nanoseconds clock as milliseconds, or None."""
    m = RE_GST_CLOCK.search(line)
    if not m:
        return None
    hours, minutes, seconds = (int(m.group(i)) for i in range(1, 4))
    nanos = int(m.group(4).ljust(9, "0"))
    return ((hours * 60 + minutes) * 60 + seconds) * 1000.0 + nanos / 1_000_000.0


def gst_frame_times(lines, pattern):
    """Sorted unique per-picture sink timestamps selected by a case-owned regex."""
    selector = re.compile(pattern, re.IGNORECASE)
    return sorted({stamp for line in lines if selector.search(line)
                   for stamp in [gst_clock_ms(line)] if stamp is not None})


def _nearest_rank(values, percentile):
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, int((percentile * len(ordered) + 99) // 100))
    return ordered[min(rank, len(ordered)) - 1]


def a_gst_resolution_trace(lines, exp, trace):
    """Bound presentation gaps around the fixture's known content-time transitions.

    This deliberately grades LG's own pipeline clock, not the app's 5 Hz position heartbeat.
    Source-info callbacks grade the exact resolution sequence separately; LG's GST caps logger did
    not repeat the returning 720p caps event on the measured firmware. The fixture owns its 8/16 s
    boundaries, so the first presented-picture clock plus those content offsets is a stronger and
    firmware-independent cadence reference.
    """
    try:
        frames = gst_frame_times(lines, trace["frame_pattern"])
    except (KeyError, re.error) as e:
        return False, f"invalid GST trace selector: {e}"

    if len(frames) < 3:
        return False, f"only {len(frames)} per-picture GST timestamp(s)"

    gaps = [(frames[i - 1], frames[i], frames[i] - frames[i - 1])
            for i in range(1, len(frames))]
    try:
        boundary_times = [frames[0] + float(seconds) * 1000.0
                          for seconds in exp["resolution_boundaries_s"]]
    except (KeyError, TypeError, ValueError) as e:
        return False, f"invalid resolution boundary declaration: {e}"
    window = float(exp.get("gst_boundary_window_ms", 1000))
    first_max = float(exp.get("gst_first_frame_max_ms", 125))
    boundary_gaps = []
    for boundary in boundary_times:
        after = next((t for t in frames if t >= boundary), None)
        if after is None:
            return False, f"no presented picture after content boundary at {boundary:.3f} ms"
        if after - boundary > first_max:
            return False, (f"first picture after {boundary:.3f} ms content boundary took "
                           f"{after - boundary:.3f} ms, limit {first_max:g} ms")
        near = [gap for left, right, gap in gaps
                if right >= boundary - window and left <= boundary + window]
        if not near:
            return False, f"no picture intervals around content boundary at {boundary:.3f} ms"
        boundary_gaps.append(max(near))

    baseline = [gap for left, right, gap in gaps
                if all(right < boundary - window or left > boundary + window
                       for boundary in boundary_times)]
    baseline_p99 = _nearest_rank(baseline, 99)
    if baseline_p99 is None:
        # A short captured trace can consist entirely of the two boundary windows. The declared
        # source rate is still a conservative reference, while the absolute floor remains binding.
        baseline_p99 = 1000.0 / float(exp.get("load_fps", 24.0))
    limit = max(float(exp.get("gst_boundary_gap_floor_ms", 120)),
                baseline_p99 * float(exp.get("gst_boundary_gap_p99_multiplier", 3.0)))
    worst = max(boundary_gaps, default=0.0)
    if worst > limit:
        return False, (f"boundary picture gap {worst:.3f} ms exceeds {limit:.3f} ms "
                       f"(off-boundary p99 {baseline_p99:.3f} ms)")
    return True, (f"GST {len(frames)} pictures; boundary max {worst:.3f} ms <= {limit:.3f} ms "
                  f"(off-boundary p99 "
                  f"{baseline_p99:.3f} ms)")


def a_server_wire(delta, min_opens, min_range, exact_opens=None, exact_range=None):
    """What the fixture SERVER saw — the one assertion no log line can give.

    Counting the bodies the server served is the server-side proof that the TV fetched THIS case's
    clip at all (zero means it never connected). The ranged-request bounds are for a case whose
    demuxer must reopen with a `Range` — a container whose index sits at the end of the file.
    """
    opens, ranges = delta[0], delta[1]
    accepts = delta[2] if len(delta) > 2 else None
    if exact_opens is not None and opens != exact_opens:
        return False, f"the fixture server served {opens} body/bodies, want exactly {exact_opens}"
    if exact_range is not None and ranges != exact_range:
        return False, f"the fixture server saw {ranges} ranged request(s), want exactly {exact_range}"
    if opens < min_opens:
        return False, f"the fixture server served {opens} body/bodies, need >={min_opens}"
    if ranges < min_range:
        return False, (f"{ranges} ranged (206) request(s), need >={min_range} — the demuxer "
                       f"never reopened with a Range")
    reuse = ""
    if accepts is not None:
        reuse = f", {accepts} accept(s)"
        if opens > 1 and accepts == 1:
            reuse += " — keep-alive reuse"
        elif opens > 1 and accepts >= opens:
            reuse += " — no socket reuse"
    return True, f"server saw {opens} open(s), {ranges} ranged{reuse}"


def a_replayed(lines, want):
    """The finished stream STARTED AGAIN, and the second run really played — LG #46's second half.

    Three signals, and each is worthless without the other two:

      * `replay: starting the finished stream again` exactly `want` times (`app.rs`, at the EOS
        site). COUNTED, not merely found: a replay that fires more often than the case armed is a
        loop, and a loop satisfies every other assertion here while meaning the opposite.
      * at least `want + 1` `load:` lines. `engine::start_bufferfeed` writes one per SESSION and
        `teardown` clears the URL, so a second line is what says `player::playurl::playurl()` was re-read and
        the payload rebuilt — the thing that distinguishes a real restart from a pipeline that
        never tore down.
      * the media position FELL and then climbed again. A replay that resumed where the first run
        ended would produce both lines above and no second viewing; the drop is the only evidence
        that the stream restarted rather than continued.

    The position series is the same `pos=` heartbeat every other case reads, deliberately: a binary
    that stopped emitting it fails here the way it fails everywhere else, instead of passing this
    case by having nothing left to contradict.

    NB the replay COUNT is an equality, which makes this the third exception to the early-exit
    soundness rule above `SETTLE_S` — see that note for what it can and cannot see, and why the
    real defence against a runaway loop is `replay_left` in `app.rs` rather than anything here.
    """
    fired = [ln for ln in lines if "replay: starting the finished stream again" in ln]
    if len(fired) != want:
        why = ("the app never re-entered the player — is `dispatcharrtv-replay` armed, and does this "
               "binary carry the replay arm at all?" if not fired
               else "a replay that fires more often than it was asked to is a loop")
        return False, f"{len(fired)} `replay:` line(s), want {want} — {why}"
    loads = [ln for ln in lines if RE_LOAD.search(ln)]
    if len(loads) < want + 1:
        return False, (f"{len(loads)} `load:` line(s) for {want} replay(s) — the second playback "
                       f"never rebuilt its payload, so the pipeline was never restarted")
    ts = [t for t, _ in playpos_secs(lines)]
    if len(ts) < 2:
        return False, f"only {len(ts)} media-position sample(s); a replay cannot be seen in them"
    # The DROP, as the deepest fall anywhere in the series — so a case replaying twice still reads
    # as one number, and a single late sample cannot hide it. The INDEX is kept too, because the
    # climb below has to be measured from there.
    peak, drop, at = ts[0], 0, 0
    for i, t in enumerate(ts):
        peak = max(peak, t)
        if peak - t > drop:
            drop, at = peak - t, i
    if drop < 5:
        return False, (f"the media position never fell (peak {max(ts)}s, deepest drop {drop}s) — "
                       f"the `replay:` line fired but playback carried on from where it was")
    # ...and it CLIMBED after falling, which is the second viewing rather than a restart that
    # stalled at the join.
    #
    # Anchored at the deepest DROP, not at the global floor. The floor form is the shape this
    # shipped with and it was a FALSE PASS: the floor is a VALUE, and viewing 2 only reaches
    # viewing 1's minimum value by coincidence — the `pos=` heartbeat is 1 Hz and free-running, so
    # viewing 1 logging `pos=0s` while viewing 2's first sample lands at `pos=1s` puts the anchor
    # back in viewing 1 and measures VIEWING 1'S OWN CLIMB. `[0,5,10,19,1]` then read as "fell 18s
    # then climbed 19s" and passed with the second viewing having produced one sample and zero
    # seconds of playback — which is precisely the near-miss named below, in the sample ordering
    # the field will actually produce, and the state the harness normally grades because it exits
    # the moment every assertion passes.
    tail = ts[at:]
    climb = max(tail) - min(tail)
    if climb < 5:
        return False, (f"the position fell {drop}s but only climbed {climb}s afterwards — the "
                       f"replay restarted and then did not play")
    return True, (f"{want} replay(s), {len(loads)} `load:` line(s), position fell {drop}s then "
                  f"climbed {climb}s over {len(ts)} samples")


def a_finished(lines):
    """The stream ran OUT, and the app left the player instead of freezing on the last frame.

    LG App Self Checklist #46 is "replay after completion", and this is its first half — the
    completion. Two links, in order, because either alone is satisfiable by something that is not
    a finish:

      * `EOS reached: playpos=Ns/Ms → ended` (pump.rs) — the producer hit file EOF AND the pipeline
        played out to within a second of the duration. Not merely "the socket closed": the pump
        gates the flag on `eos_pushed && pos >= dur - 1s`, so a truncated transfer does not reach
        it.
      * `stop_bufferfeed: torn down` (engine.rs::teardown) AFTER that line — `app.rs` calls
        `finish_playback` on `player::ended()`, which with nothing queued is `exit_player`. The
        ORDER is the assertion: this app tears the engine down on every stop, including the
        harness's own close at the end of a case, so an unordered match would pass on a clip that
        never ended at all.

    "After" is a SEARCH FROM `eos`, not a comparison against the first teardown in the log, and
    the difference is a false regression rather than a nicety: `teardown` writes that same line on
    a `for_reload` stop too — a seek that escalated to `reload_at`, or an app-switch suspend — so a
    first-match index can sit BEFORE the EOS while every teardown that matters comes after it. The
    comparison form fails such a case for its whole `run_secs`, with the evidence reading "the
    player froze on the last frame", which is exactly the reading this assertion exists to avoid.

    What it deliberately does NOT claim is the SECOND half of #46 — that the same content can then
    be started again. That is [`a_replayed`]'s job and `pipe_replay_after_eos`'s, and the two stay
    apart because the failures are different: a stream that never ends, and a stream that ends and
    cannot be restarted. One case would report either as the other.
    """
    eos = next((i for i, ln in enumerate(lines) if "EOS reached" in ln), None)
    if eos is None:
        ts = playpos_secs(lines)
        far = max((t for t, _ in ts), default=-1)
        return False, (f"no `EOS reached` line — the stream never ran out (deepest position "
                       f"{far}s). A fixture longer than this case's cap cannot end inside it.")
    torn = next((i for i, ln in enumerate(lines) if i > eos
                 and "stop_bufferfeed: torn down" in ln), None)
    if torn is None:
        return False, (f"reached EOS but never tore the engine down after it — the player froze on "
                       f"the last frame :: {lines[eos].strip()}")
    return True, f"{lines[eos].strip()} -> {lines[torn].strip()}"


# ---------------------------------------------------------------------------
# Streaming run — grade the event log AS IT ARRIVES, stop as soon as a case passes.
#
# The old path slept a fixed run_secs (60/70/75/90 per case — 1190s, ~20 min of pure
# sleep across the 18 cases) and only graded afterwards. Almost every case satisfies its
# assertions well before that. What a case can NOT beat is `min_pos_climb_s` seconds
# of real playback, because playback is 1x realtime — that is the floor, and it is a
# coverage decision, not waste. So run_secs stops being the runtime and becomes the CAP:
# a FAILING case still costs exactly what it costs today; a passing one costs what it needs.
#
# Why grading a prefix is sound: almost every assertion is monotone once satisfied — it looks
# for a line that has appeared, or for a max/climb that only grows. The exception is the ABSENCE
# check, which starts out true and can only flip the other way:
#   * a_no_error            — `smp_cb type=18` / `Playing error`
# ...and, since 2026-08-23, one COUNT that is an equality rather than a floor:
#   * a_replayed            — exactly N `replay:` lines (more than N is a loop)
# It is the same shape as the one above and it is here because this file asks a new exception to
# be reasoned about rather than added. What it cannot see: an app that replays FOREVER fires its
# next `replay:` line only after another full viewing (~20 s on the short clip), and the case can
# satisfy everything else at ~28 s, so an early exit two seconds later stops before the evidence
# arrives. The primary defence is therefore app-side and not here — `replay_left` is decremented
# before each re-arm and its parsing is host-tested (`app::replay_budget_tests`) — and `--no-early`
# is how to go looking for a loop deliberately. Deliberately NOT closed by forcing this one case to
# burn its whole 100 s cap: that is ~1.5 min on every suite run to guard three lines whose bound is
# already gradeable by `make check`, and the trade is worth re-taking only if that bound moves.
# So early exit can never turn a FAIL into a PASS on the evidence *seen*; what it can do is
# stop before evidence that would have failed the case arrives. SETTLE_S keeps watching for a
# moment after the last assertion flips, and --no-early restores the full fixed window.
# (Note the old window was itself arbitrary — an error at 70s of a 60s case was never caught.)
SETTLE_S = 2.0
EVAL_EVERY_S = 0.5
# How long the app gets to produce its FIRST log line before we give up and grade an empty log.
# Generous on purpose: boot is ~5-10s, and a TV that is merely slow should fail on its
# assertions, not on a harness timeout that looks identical to a total regression.
BOOT_GRACE_S = 45.0


def failed_for_good(case, lines):
    """The reason a case can no longer pass however long it runs, or None if it still might.

    Returned as a STRING, not a bool, because stopping early can change which failure the case
    reports: assertions are written to name the FIRST thing missing, and cutting a case short can
    leave an earlier check unsatisfied for no reason but the clock, so the reported evidence would
    point at a symptom of the early exit rather than at the real disqualifier. Surfacing the
    settled reason next to the verdict keeps the true cause instead of throwing it away.

    Only an ABSENCE check can conclude this, and it is the mirror image of the rule that
    limits early PASS: once the disqualifying line exists it never un-exists, so the rest of the
    cap is pure waste. Every OTHER failure means "the line has not appeared YET" — exactly what
    more time could still fix — so those must run to the cap. This matters because a red suite is
    the one you iterate against.
    """
    if case["expect"].get("no_playing_error", True):
        ok, evidence = a_no_error(lines)
        if not ok:
            return evidence
    return None


class StampedLines(list):
    """The log as a plain list of strings, plus WHEN each line reached the harness.

    A `list` subclass rather than a second parallel structure, so every existing caller — and
    there are dozens — keeps treating the log as the list of strings it has always been, while
    `.stamps[i]` carries the harness monotonic clock reading for `self[i]`.

    It exists to place an app observation on the SHAPER's timeline. The fixture server runs in
    this process, so its phase clock and these stamps are the same `time.monotonic()`; that is
    what lets a segment be attributed to an injected leg without asking the controller anything.

    The stamp is ARRIVAL, not emission: it lags the app by the ssh/`tail -F` hop. Sub-second
    against legs measured in seconds, and the direction is knowable — arrival is always later —
    but it is a real limit and every consumer says so.
    """

    def __init__(self, items=(), stamps=()):
        super().__init__(items)
        self.stamps = list(stamps)

    def append(self, item):
        self.stamps.append(time.monotonic())
        super().append(item)

    def snapshot(self):
        """A copy carrying its stamps. `list(x)` would silently drop them."""
        return StampedLines(list(self), list(self.stamps))


def _drain(stream, sink, done):
    """Reader thread: filter the type=43 flood at the door so the grader never re-scans it."""
    try:
        for raw in stream:
            ln = raw.rstrip("\n")
            if not TYPE43_SPAM.search(ln):
                sink.append(ln)
    finally:
        done.set()


def _run_stream_pids(run_cmd=subprocess.run):
    """PIDs of every local ssh/sshpass process carrying a run-stream tail.

    RUN_STREAM_MARK is the tail `make run-stream` ends in, and it is DERIVED (resolve_flavour) from
    `make -s print-eventlog` rather than written out here — the two installs tail two different
    files, and a mark that stops matching the remote command text reaps nothing at all.
    """
    # A process argv is an arbitrary byte sequence on POSIX. macOS `ps` normally emits UTF-8, but
    # one unrelated local process with a legacy byte used to crash the ENTIRE TV teardown after a
    # passing scene. Decode the process table explicitly and replace only that unprintable byte;
    # the ASCII event-log marker and PID remain exact.
    raw = run_cmd(["ps", "-Ao", "pid,command"], capture_output=True).stdout
    out = raw.decode("utf-8", errors="replace") if isinstance(raw, bytes) else raw
    pids = set()
    for ln in out.splitlines():
        if RUN_STREAM_MARK in ln and " ps -Ao" not in ln:
            try:
                pids.add(int(ln.split(None, 1)[0]))
            except ValueError:
                pass
    return pids


def teardown(tv):
    """Leave the TV as we found it. Runs on EVERY exit — pass, fail, Ctrl-C, SIGTERM, crash,
    and whether this file was run as a program or imported and called.

    Three things outlive the harness otherwise, and none of them are cosmetic:

    * THE APP KEEPS RUNNING. Nothing closes it at the end — `make kill` only runs at the START
      of the next case, so the last case's clip just carries on, long after the suite printed
      its summary.
    * THE ACCOUNT STAYS ASIDE. `run_pipeline_suite` restores it in a `finally`, which covers an
      exception and Ctrl-C; this covers whatever that misses, and an interrupted run's leftover
      is restored by the next run before it moves anything.
    * ssh CLIENTS. Per-case reaping covers a case that ends normally, but not the harness dying
      between cases.

    All three are scoped to the install this run drove and to nothing else: `make kill` carries
    the flavour (so `closeByAppId` names this id, and `fuser -k` is INODE-scoped on this app dir —
    a name-based kill would take the other install down with it), and the trigger wipe is a glob
    inside this flavour's runtime root. A run against the developer build must leave the app users
    installed exactly as it found it, running or not.

    Never raises: a teardown that throws on an unreachable TV would mask the real failure (and
    on Ctrl-C would replace the user's interrupt with a traceback about ssh).

    IDEMPOTENT, because two things arm it — the atexit hook in `arm_teardown` and the `__main__`
    wrapper — and on the ordinary path both fire. Doing the work twice is only wasteful, but the
    second `make kill` prints over the summary the user is reading, and a reader who sees the
    cleanup banner twice reasonably wonders which run it belonged to.
    """
    global _TEARDOWN_DONE
    if _TEARDOWN_DONE:
        return
    _TEARDOWN_DONE = True
    try:
        make(["kill", f"TV={tv}"], timeout=40)
    except Exception:
        pass
    try:
        ssh(tv, account_aside_cmd(account_paths(APPID, APPDIR, RUNDIR), aside=False))
    except Exception:
        pass
    try:
        apply_triggers(tv, [])
    except Exception:
        pass
    for pid in _run_stream_pids():
        try:
            os.kill(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass


# The TV to clean on exit, or None while the harness has not driven one yet. Armed at the point we
# commit to driving the TV — NOT at startup — so `--list`, a bad `--filter` and a missing pack all
# exit without closing an app the user may be watching.
_TEARDOWN_TV = None
_TEARDOWN_DONE = False

TVLOCK = os.path.join(REPO_ROOT, "tools", "tv-lock.sh")
_LOCK_TAKEN = False


def acquire_tv_lock(tv, why):
    """Take the television's lock, or refuse to run.

    The whole suite is one long exclusive session: it closes the app between cases, wipes the
    runtime root, moves the account aside and grades a log it assumes only it is writing. A second
    job on the set during any of that does not produce a clean failure — it produces a plausible
    wrong one (a bogus pos_climb, a presented rate taken while another lane's deploy was landing),
    which is indistinguishable from a real regression when you read the summary.

    A lease this lane already holds is inherited rather than re-taken, so
    `tools/tv-lock.sh with -- ./tests/run.py` and a hand-held session both work unchanged.
    """
    global _LOCK_TAKEN
    if not os.access(TVLOCK, os.X_OK):
        return
    env = dict(os.environ, TV=tv)
    held = subprocess.run([TVLOCK, "status"], env=env, capture_output=True, text=True)
    if "HELD BY THIS LANE" in held.stdout:
        print("TV lock: already held by this lane — inheriting it")
        return
    # `--wait 0`: a harness that blocks for ten minutes inside somebody's tool call is worse than
    # one that says who has the set and exits. `--wait <s>` is the caller's choice, not ours.
    r = subprocess.run([TVLOCK, "acquire", "--why", why, "--as", "tests/run.py"], env=env)
    if r.returncode != 0:
        sys.exit("refusing to run: the television is held by another lane (see above). "
                 "Wait for it (tools/tv-lock.sh acquire --wait 540), or run host-side work "
                 "meanwhile (make check, make sim).")
    _LOCK_TAKEN = True


def release_tv_lock(tv):
    """Give the set back. Never raises — it runs in the same finally as teardown."""
    if not _LOCK_TAKEN:
        return
    try:
        subprocess.run([TVLOCK, "release"], env=dict(os.environ, TV=tv),
                       capture_output=True, timeout=30)
    except Exception:
        pass


def arm_teardown(tv):
    """Commit to driving `tv`, and guarantee the cleanup from THIS MOMENT ON, whatever happens.

    The atexit hook is the guarantee, and it is not redundant with the `__main__` wrapper below.
    That wrapper only runs when this file is the program; anything that does `import run;
    run.main()` -- a wrapper script, tests/test_harness.py, an agent probing one exit path -- got
    no cleanup at all, and the failure is invisible: the run prints its summary, exits 0, and
    leaves the app playing and the account moved aside. That is the outcome teardown() exists to
    prevent, and the missing cleanup happened here on 2026-08-22.

    Both paths stay: atexit does not run for SIGTERM (the wrapper's handler converts it to an
    unwind) and prints after the interpreter has begun shutting down, so the wrapper keeps giving
    the ordinary path its output in the right place. `teardown` is idempotent, so both firing is
    fine and neither firing is not possible."""
    global _TEARDOWN_TV
    _TEARDOWN_TV = tv
    atexit.register(lambda: teardown(_TEARDOWN_TV) if _TEARDOWN_TV else None)


# The app's first log line, written before anything can fail (app/mod.rs): which install
# produced this log, and whether it was built with the dev triggers at all.
RE_INSTALL = re.compile(r"install: id=(\S+) flavour=\S+ .*\bfeatures=(\S+)")


def check_install(lines, cfg):
    """Refuse to grade a log that did not come from the install this run drove.

    Returns True once the boot line has been seen and matched, False while it has not arrived yet;
    aborts the WHOLE run otherwise — not the case, for the reason below.

    Nothing else can answer this question. Both binaries are named `dispatcharrtv`, so `pidof` matches
    both on this busybox set, and `pkg/dispatcharrtv` is a path every flavour and every configuration
    writes, so an md5 against the local build proves only that SOME build matches. This one line is
    the only witness, which is why its absence is also a refusal (see require_install).

    The un-caught failure is what makes this worth an abort. A RELEASE build reads no triggers at
    all — `devtriggers` is compiled out, so `devtrig::read` is None at COMPILE time — which means the
    playurl trigger is ignored and the app boots to Setup having played nothing. Every assertion then fails as "the line has not appeared YET",
    which `failed_for_good` deliberately never settles, so every case burns its full run_secs and
    the summary reads like a catastrophic regression — for a build that is working perfectly.
    Grading the OTHER install's log is the same failure, quieter.
    """
    for ln in lines:
        m = RE_INSTALL.search(ln)
        if not m:
            continue
        got, feats = m.group(1), m.group(2)
        if got != APPID:
            raise SystemExit(
                f"WRONG INSTALL: the app that booted logs id={got}, but this run drives "
                f"{APPID} (flavour {FLAVOUR}). Every path this harness used belongs to "
                f"{APPID} — the triggers, the account move, {EVENTLOG} — so the whole "
                f"run would grade a log it never armed. Deploy the flavour you meant "
                f"(`make FLAVOR={FLAVOUR} install` once, then `make FLAVOR={FLAVOUR} deploy`), or "
                f"select the other one with --flavor.")
        if feats != "dev":
            raise SystemExit(
                f"RELEASE BUILD on {got}: its boot line says features={feats}, so the whole "
                f"`devtriggers` surface was compiled out and it reads NOTHING under {RUNDIR} — not "
                f"one trigger this harness wrote. It boots to Setup and plays nothing, and every "
                f"case would burn its full cap "
                f"failing as if the player were broken. Ship a dev build to this install: "
                f"`make FLAVOR={FLAVOUR} deploy` without RELEASE=1.\n"
                f"  NB the STABLE install is normally a release build by design — `make deploy` "
                f"refuses a dev build on that id unless ALLOW_DEV_ON_STABLE=1 — so `--flavor "
                f"stable` lands here unless you deliberately put a dev build there.")
        return True
    return False


def require_install(lines, cfg):
    """check_install for a COMPLETE log, where an absent boot line is itself a refusal."""
    if check_install(lines, cfg) or not lines:
        return  # an empty log is graded by the assertions themselves ("no line found")
    raise SystemExit(
        f"no `install:` boot line in {EVENTLOG}. The deployed binary predates it (app/mod.rs "
        f"writes it first, before anything can fail), so nothing in this log says which "
        f"of the two installs produced it — and an unattributable log is exactly what this check "
        f"exists to refuse. `make FLAVOR={FLAVOUR} deploy` ships one that carries it.")


def stream_case(case, cfg, cap_s, evaluator, early=True):
    """Launch via `make run-stream` and grade the log as it streams.

    Returns (lines, elapsed_s, stopped_early, settled_reason). Lines come back already filtered;
    settled_reason is non-None only when the case was cut short by an already-decided failure.

    `evaluator` is (case, lines) -> (passed, results), and the early-exit poll below runs the SAME
    grading the verdict uses — a poll that graded something else would either stop a case that had
    not passed or run one that had.
    """
    # Snapshot BEFORE launching so the teardown can kill exactly the clients this case started.
    # killpg is not sufficient on its own: sshpass forks ssh into its OWN process group (measured:
    # make pgid=P, sshpass pid=A pgid=P, ssh pid=B pgid=B ppid=A), so the group signal never
    # reaches B, and B reparents to init holding an ssh connection and a remote `tail -F`. These
    # accumulate one per case — 125 of them piled up against the TV in a single session before
    # this was noticed, which is real load on a device whose dropbear has a connection limit.
    pre_pids = _run_stream_pids()
    proc = subprocess.Popen(make_argv(["run-stream", f"TV={cfg['tv']}"]),
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                            text=True, bufsize=1,
                            # own process group: terminating `make` alone would orphan the
                            # sshpass/ssh child and leave the remote tail (and the app) attached.
                            start_new_session=True)
    lines, done = StampedLines(), threading.Event()
    threading.Thread(target=_drain, args=(proc.stdout, lines, done), daemon=True).start()

    install_ok = False
    started = time.monotonic()
    # cap_s is APP runtime, so the clock starts at the app's first log line — not here. Anchoring
    # it at ssh-start instead would silently shorten every case by the close+launch overhead
    # (BOOT_SH's own `sleep 2` plus connect), which `make run RUN_SECS=` spent BEFORE it started
    # counting. That is a couple of seconds off exactly the cases that run to the cap.
    deadline = None
    passed_since = None
    stopped_early = False
    settled = None
    try:
        while True:
            time.sleep(EVAL_EVERY_S)
            ended = done.is_set()  # sample BEFORE grading, so we grade what arrived last
            now = time.monotonic()
            # As early as possible: a wrong or release install disqualifies the whole run, and
            # finding that out at the first case costs one cap instead of the entire suite's.
            if not install_ok:
                install_ok = check_install(list(lines), cfg)
            if deadline is None:
                if lines:
                    deadline = now + cap_s
                elif now - started >= BOOT_GRACE_S:
                    break  # app never wrote a line — grade the empty log, same as a dead TV
            elif now >= deadline:
                break
            if early:
                snap = list(lines)
                ok, _ = evaluator(case, snap)
                if not ok:
                    passed_since = None
                    settled = failed_for_good(case, snap)
                    if settled:
                        stopped_early = True
                        break  # verdict is settled — don't burn the rest of the cap
                elif passed_since is None:
                    passed_since = now
                elif now - passed_since >= SETTLE_S:
                    stopped_early = True
                    break
            if ended:
                break  # ssh hung up (TV asleep / connection lost) — grade what we got
    finally:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            pass
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        # Reap the ssh clients killpg could not reach (see pre_pids). SIGKILL, not SIGTERM —
        # sshpass/ssh demonstrably survive the TERM the group already sent them. Only pids that
        # appeared while this case ran are touched, so a stray unrelated session is left alone.
        for pid in _run_stream_pids() - pre_pids:
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
    # And once more now the log is complete. The in-loop check cannot refuse an ABSENT boot line —
    # while a case is still running, "not there" and "not there yet" are the same thing.
    if not install_ok:
        require_install(list(lines), cfg)
    return lines.snapshot(), time.monotonic() - started, stopped_early, settled


def report_case(passed, results, elapsed, run_secs, stopped_early, settled, verbose):
    """Print one case's verdict."""
    for label, ok, evidence in results:
        mark = "PASS" if ok else "FAIL"
        if ok and not verbose:
            print(f"      [{mark}] {label}")
        else:
            print(f"      [{mark}] {label}: {evidence}")
    how = f"{elapsed:.0f}s" + (f" (early, cap {run_secs}s)" if stopped_early else f" (full cap {run_secs}s)")
    print(f"    => {'PASS' if passed else 'FAIL'} in {how}")
    if settled:
        # the true disqualifier — which is NOT always the assertion printed above (see
        # failed_for_good), so losing it would point a reader at a symptom instead of the cause.
        print(f"       stopped early — settled: {settled}")


# ---------------------------------------------------------------------------
# The fixture pack.
#
# Cases drive `dispatcharrtv-playurl`: a generated file served off this machine by
# `tests/serve_fixtures.py`, plus the Load-payload declaration to play it with. What runs is the
# same engine, byte for byte, as a live channel: `stream.rs`'s raw-socket GET, `ff.rs`'s demux over
# the custom AVIO, the two-lane AU queues, the Starfish `Feed()` pump and the video-plane bind.
#
# What this CANNOT prove: that the declaration it feeds is the one a real channel would produce.
# It writes the route's stream declaration itself, so a channel's codec detection and correction
# (`live_codec_correction`) are bypassed and a regression there passes here green.
# ---------------------------------------------------------------------------
@functools.lru_cache(maxsize=None)
def _probe_fixture(path):
    """`(duration_s, [(codec_type, codec_name), ...])` for one fixture, or None.

    ffprobe rather than the generator's `fixtures.json`, deliberately: the file on disk is the
    thing the television will actually play, and a sidecar record can be stale, hand-edited, or
    written by a version of the generator that spelled its fields differently. None (no ffprobe on
    this machine, an unreadable file) disables every check built on it rather than failing them —
    refusing to run a suite because a *diagnostic* is unavailable is the wrong trade.

    Cached by PATH because SEVERAL cases share one fixture — a seek case beside its play case, an
    audio-lane pair on the multi-audio clip — so `--list` would otherwise ffprobe the same file
    twice. Stated as the property rather than as a count: this line read "three of the twelve" and
    was wrong on both halves even before the resolution matrix widened the denominator (it was four
    of twelve then, and is four of nineteen now). The caching is right at any of those numbers.
    The streams come back in container order, which is the order `ff.rs` walks when it matches the
    declared audio codec, so the list index IS the `a=#<n>` the log reports.
    """
    try:
        out = subprocess.run(
            ["ffprobe", "-v", "error", "-of", "json",
             "-show_entries", "format=duration:stream=codec_type,codec_name", path],
            capture_output=True, text=True, timeout=20)
        doc = json.loads(out.stdout)
        dur = float(doc["format"]["duration"])
        return dur, [(st.get("codec_type"), st.get("codec_name")) for st in doc.get("streams", [])]
    except (OSError, ValueError, KeyError, subprocess.SubprocessError):
        return None


def _declaration_mismatch(case, streams):
    """Why this case's `declare` does not describe the file it names, or None.

    The tier's single premise is that the declaration is honest about the media, and until this
    existed nothing checked it: the pack's generator verifies `declare` against what it built, but
    the harness sends the manifest's copy, and the two are authored separately. The three cases
    that deliberately override a shape's declaration to select another audio lane are exactly the
    ones the generator's own check cannot cover — and they are the ones carrying a hand-counted
    stream index.

    A mismatch is a SKIP, not a failure, for the same reason a too-short pack is: the fixture is
    wrong, the player is not, and a case that fails here would point squarely at the player.
    """
    dec = case.get("declare", {})
    vid = [c for t, c in streams if t == "video"]
    aud = [(i, c) for i, (t, c) in enumerate(streams) if t == "audio"]
    if dec.get("vcodec") and vid and dec["vcodec"] != vid[0]:
        return f"declares vcodec {dec['vcodec']!r}, the file's video stream is {vid[0]!r}"
    if dec.get("acodec") and aud and dec["acodec"] not in [c for _, c in aud]:
        return (f"declares acodec {dec['acodec']!r}, which no audio stream carries "
                f"({', '.join(c for _, c in aud)})")
    want_idx = case.get("expect", {}).get("audio_stream_index")
    if want_idx is not None and dec.get("acodec"):
        # `ff.rs::audio_stream_matching` feeds the FIRST audio stream whose codec matches, so the
        # expected index is derivable — computed here from ffprobe rather than trusted from the
        # manifest, which makes the hand-counted number an assertion instead of a premise.
        first = next((i for i, c in aud if c == dec["acodec"]), None)
        if first != want_idx:
            return (f"expects audio_stream_index {want_idx}, but the first {dec['acodec']!r} "
                    f"stream in this file is #{first}")
    return None


def _case_depth_s(case):
    """The deepest media position this case needs to reach, in seconds: its position-climb floor."""
    return case.get("expect", {}).get("min_pos_climb_s", 0)


def _resolve_fixtures(cases, root):
    """Mark each case runnable (`path`) or not (`skip`, and NO `path`).

    A case that cannot run gets `skip` and no `path`, so a partition that is ever wrong raises
    KeyError naming the case rather than driving the TV at some default. Three ways to be
    unrunnable, and they are different answers: the pack was never built, it was built SHORTER
    than this case must play, or — for the one case that wants the stream to run OUT — it was built
    LONGER than the case can play through. The last two are the quiet ones: a pack regenerated
    at `--secs 300` makes `a_finished` fail as though the player had regressed.
    """
    for c in cases:
        name = c.get("fixture")
        if not name:
            c["skip"] = "case declares no `fixture`"
            continue
        path = os.path.join(root, name)
        if not os.path.isfile(path):
            c["skip"] = f"no fixture {name!r} in {root} — run `make fixtures-pipeline`"
            continue
        probe = _probe_fixture(path)
        if probe is not None:
            dur, streams = probe
            depth = _case_depth_s(c)
            if depth and depth > dur * 0.8:
                c["skip"] = (f"{name} is {dur:.0f}s but this case needs {depth:.0f}s — regenerate "
                             f"the pack longer (`make fixtures-pipeline FIXTURES_ARGS=--secs=<n>`)")
                continue
            # ...and the opposite bound, for `reaches_eos` alone. That case has to boot the app,
            # join the stream and then play the WHOLE clip at 1x inside its cap — ONCE PER VIEWING,
            # so a case that also replays needs the clip `replays + 1` times over. The clip must be
            # comfortably shorter than the cap rather than merely shorter: 0.6 leaves 40% of the
            # window for close+launch and one pre-roll per viewing, against a boot-to-playing
            # measured well under 15 s. A `--secs`-regenerated pack is the realistic way to trip
            # this, and it must skip — `a_finished` failing on a 300 s clip reads as the app
            # freezing on the last frame, and `a_replayed` failing on one reads as the replay arm
            # being absent from the binary.
            exp = c.get("expect", {})
            if exp.get("reaches_eos"):
                cap = c.get("run_secs", 30)
                views = 1 + exp.get("replays", 0)
                if dur * views > cap * 0.6:
                    c["skip"] = (f"{name} is {dur:.0f}s and this case must play it to the END "
                                 f"{views}x within a {cap}s cap — rebuild the pack at its declared "
                                 f"length (`make fixtures-pipeline`, no --secs/--quick)")
                    continue
            why = _declaration_mismatch(c, streams)
            if why:
                c["skip"] = f"{name} does not match this case: {why} — regenerate the pack"
                continue
        c["path"] = path


def evaluate_pipeline(case, lines, srv_delta, gst_lines=None):
    """Every assertion for a pipeline case. Returns (passed, [(label, ok, evidence)]).

    """
    exp = case["expect"]
    results = [("stream_path", *a_stream_path(lines, case["fixture"]))]
    results.append(("load_decl", *a_load_decl(lines, exp)))
    if "load_count_exact" in exp:
        results.append(("load_count", *a_load_count(lines, exp["load_count_exact"])))
    if exp.get("require_audio_feed_ready"):
        results.append(("audio_feed", *a_audio_feed_ready(lines)))
    if exp.get("no_reload"):
        results.append(("no_reload", *a_no_reload(lines)))
    if "codec" in exp:
        results.append(("codec", *a_codec(lines, exp["codec"], exp.get("min_video_width", 0),
                                          exp.get("video_size"))))
    if exp.get("require_video_bound", True):
        results.append(("video_bound", *a_video_bound(lines)))
    results.append(("pos_climb", *a_timeline_climb(lines, exp.get("min_pos_climb_s", 8))))
    if "audio_stream_index" in exp:
        results.append(("audio_lane", *a_audio_lane(lines, exp["audio_stream_index"])))
    if exp.get("reaches_eos"):
        results.append(("finished", *a_finished(lines)))
    if exp.get("replays"):
        results.append(("replayed", *a_replayed(lines, exp["replays"])))
    if exp.get("no_playing_error", True):
        results.append(("no_error", *a_no_error(lines)))
    results.append(("presented_rate", *a_presented_rate(lines, exp)))
    if "starfish_resolution_sequence" in exp:
        results.append(("starfish_resolution", *a_starfish_resolution_sequence(
            lines, exp["starfish_resolution_sequence"])))
    if "resolution_boundaries_s" in exp:
        results.append(("gst_cadence", *a_gst_resolution_trace(
            gst_lines or [], exp, case.get("gst_trace", {}))))

    # Last: the wire counters corroborate stream_path from the server side.
    results.append(("server_wire", *a_server_wire(
        srv_delta, exp.get("server_opens_min", 1), exp.get("server_range_opens_min", 0),
        exp.get("server_opens_exact"), exp.get("server_range_opens_exact"))))
    return all(ok for _, ok, _ in results), results


def pull_runtime_log(tv, name):
    """Read a case-owned diagnostic after playback; an absent/unreadable trace grades empty."""
    proc = ssh(tv, f"cat {RUNDIR}/{name}", timeout=30)
    return proc.stdout.splitlines() if proc.returncode == 0 else []


def save_case_log(cfg, name, lines):
    """Persist one case's event log, if --save-logs asked for it.

    The app truncates `dispatcharrtv-events.log` at every launch and every case relaunches, so the
    only copy of a trace is the one the harness holds in memory. Increment I2 needs those traces
    (transaction records, per-segment acquisition) and re-running a device case to recover a log
    costs a television lease.
    """
    outdir = cfg.get("save_logs")
    if not outdir:
        return None
    os.makedirs(outdir, exist_ok=True)
    path = os.path.join(outdir, f"{name}.log")
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")
    return path


ASIDE = ".harness-aside"


def account_paths(appid, appdir, rundir):
    """Every place the app looks for its Dispatcharr account and prefs
    (`plx_base::paths::dispatcharr_candidates`, device order)."""
    out = []
    for stem in ("dispatcharr", "dispatcharr-prefs"):
        out += [f"/media/developer/{appid}-{stem}.json", f"/media/internal/.{appid}-{stem}.json",
                f"{appdir}/{stem}.json", f"{rundir}/{stem}.json"]
    return out


def account_aside_cmd(paths, aside):
    """The remote shell that moves the account out of the app's way (`aside`) or puts it back.

    **Why the cases need it:** a set with an account resumes its last channel on launch, a tuned
    channel sets the route's URL, and the engine takes the route's URL before the playurl trigger
    (2026-10-16: three cases graded a live channel and never requested their clip). With no
    account the app boots to Setup, where nothing resumes. `mv` keeps owner and mode, so the
    restore puts back exactly the file that was there. A leftover from an interrupted run is
    restored before anything is moved, so an aside file can never be the only copy that a second
    move overwrites."""
    restore = [f"[ ! -e {sh_squote(p + ASIDE)} ] || mv -f {sh_squote(p + ASIDE)} {sh_squote(p)}"
               for p in paths]
    if not aside:
        return "; ".join(restore) + "; true"
    move = [f"[ ! -e {sh_squote(p)} ] || mv -f {sh_squote(p)} {sh_squote(p + ASIDE)}" for p in paths]
    return "; ".join(restore + move) + "; true"


def run_pipeline_case(case, cfg, srv, url_base, verbose):
    """One case: `make kill` (the app is a singleton on the television, and the previous case's
    is still up), `apply_triggers`' single round-trip that recreates the runtime root 1777 and
    glob-wipes stale triggers, then `stream_case` — early-exit grading, the install check, and the
    killpg-plus-ssh-reap in its `finally`."""
    name = case["name"]
    tv = cfg["tv"]
    run_secs = case.get("run_secs", 30)
    print(f"\n=== {name}  ({case['fixture']}, {case.get('title','')}) ===")
    print(f"    covers: {', '.join(case.get('covers', []))}")

    require_tv(tv, name)
    make(["kill", f"TV={tv}"], timeout=40)
    files = triggers_for_case(case, url_base=url_base)
    apply_triggers(tv, files)
    print("    triggers: " + ", ".join(n + ("=" + c if c is not None else "") for n, c in files))

    before = srv.stats()

    def grade(c, ls):
        """The wire counters are read LIVE, so the early-exit poll grades the same
        `server_wire` the verdict will — a case cannot stop early on a seek whose Range reopen
        has not happened yet."""
        now = srv.stats()
        return evaluate_pipeline(c, ls, tuple(n - b for n, b in zip(now, before)))

    early, why = early_exit_allowed(case, cfg)
    if not early and why:
        print(f"    early exit disabled: {why}")
    print(f"    run-stream (cap {run_secs}s{'' if early else ', early exit off'}) ...")
    lines, elapsed, stopped_early, settled = stream_case(case, cfg, run_secs, grade, early=early)
    after = srv.stats()
    delta = tuple(a - b for a, b in zip(after, before))

    gst_lines = pull_runtime_log(tv, "dispatcharrtv-gst.log") if case.get("gst_trace") else None
    if gst_lines is not None:
        print(f"    GST trace: {len(gst_lines)} line(s)")
    passed, results = evaluate_pipeline(case, lines, delta, gst_lines=gst_lines)
    report_and_record(cfg, name, passed, results, lines, elapsed, run_secs, stopped_early,
                      settled, verbose)
    if delta[0] == 0:
        # Every case that reaches the television opens at least one body. Zero means the TV never
        # connected AT ALL, and on macOS the overwhelmingly likely reason is not the app: the
        # application firewall drops connections to an ad-hoc python listener silently — no
        # refusal, no log line, the app's open just reads empty. Every assertion then fails as
        # "no line found", which is precisely what a total regression looks like. Say so once.
        print("       NOTE: the fixture server saw NO request from the TV. If this repeats for "
              "every case, it is almost certainly the macOS application firewall — accept the "
              "'allow incoming connections' prompt for this python once, with a human present.")
    return passed, results, lines


def run_pipeline_suite(cases, cfg, srv, url_base, verbose, skipped=()):
    summary = []
    paths = account_paths(APPID, APPDIR, RUNDIR)
    tv = cfg["tv"]
    make(["kill", f"TV={tv}"], timeout=40)
    ssh(tv, account_aside_cmd(paths, aside=True))
    print("    account: moved aside for the run (restored at the end)")
    try:
        for c in cases:
            try:
                passed, results, _ = run_pipeline_case(c, cfg, srv, url_base, verbose)
            except Exception as e:  # keep the batch going; record the failure
                print(f"    ERROR running {c['name']}: {e}")
                passed, results = False, [("harness", False, str(e))]
            summary.append((c["name"], passed, [l for l, ok, _ in results if not ok]))
    finally:
        # Before `teardown` releases the lock (atexit runs after this): the app is closed first so
        # it cannot write the account while it is being put back.
        make(["kill", f"TV={tv}"], timeout=40)
        ssh(tv, account_aside_cmd(paths, aside=False))
        print("    account: restored")

    print("\n" + "=" * 72)
    print("SUMMARY")
    print("=" * 72)
    npass = nfail = 0
    for nm, passed, fails in summary:
        npass, nfail = (npass + 1, nfail) if passed else (npass, nfail + 1)
        print(f"  [{'PASS' if passed else 'FAIL'}] {nm}" + ("" if passed else "  <- " + ", ".join(fails)))
    for nm, reason in skipped:
        print(f"  [SKIP] {nm}  <- {reason}")
    tail = f", {len(skipped)} skipped" if skipped else ""
    print(f"\n{npass} passed, {nfail} failed of {len(summary)}{tail}")
    return 0 if nfail == 0 else 1


# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
def do_build(tv):
    """`make all` then `make deploy`, with the fast `tvdev` cargo profile: the cases are functional,
    and none grades frame timing."""
    # The Makefile owns the whole build (it drives cargo +nightly with the load-bearing
    # cortex-a9 flags itself) — shelling out to it keeps run.py from drifting a second
    # copy of the toolchain invocation (the old hand-rolled zigbuild here did exactly that).
    print("=== BUILD: make -> make deploy ===")
    if make(["all"], timeout=1200, capture=False).returncode != 0:
        sys.exit("make failed")
    if make(["deploy", f"TV={tv}"], timeout=180, capture=False).returncode != 0:
        sys.exit("make deploy failed")
    print("=== BUILD OK ===")


# `bench: kind=<push|modal> cycle=<i>/<n> target=<name> worst_ms=<f> frames=<k> dur_ms=<d>
# rss_kb=<r> tex=<n>/<kB> first_ms=<f> missed=<k> open=<half>[ close=<half>]` — one line per
# completed stress-bench cycle (`dev::scenarios::push_bench_tick` / `modal_bench_tick`), and a
# terminal `bench: kind=<k> done cycles=<n>` once every cycle ran. `missed` is the GATED number:
# refreshes on which the panel repeated a picture, counted from present-to-present intervals
# (`dev::scenarios::bench::HalfStats`). `worst_ms` is the largest Top->Swap, which on this driver
# includes the frame's own vsync wait, so it is reported but not graded. Each `<half>` is
# `first:<ms>,worst:<ms>@<i>,iv:<ms>,missed:<k>[,defer_first:<n>]` — the open (press) and close
# (back) halves. `iv` is the half's worst present-to-present interval (graded by the optional
# `bench_iv_max_ms`); `defer_first` counts loop iterations the capture fence held BEFORE the
# half's first presented frame (input-to-first-frame latency: reported, never in `missed`). A
# deferral AFTER the first present is inside `iv`/`missed`. Older lines lack `defer_first`.
BENCH_TAIL = (r"(?: tex=\S+)? first_ms=(?P<first>\d+(?:\.\d+)?) missed=(?P<missed>\d+)"
              r" open=(?P<open>\S+)(?: close=(?P<close>\S+))?")


def main():
    # LIVE OUTPUT. This suite drives a television for minutes, and Python block-buffers stdout the
    # moment it is not a tty -- so piping it to a file showed NOTHING until the run ended, which is
    # indistinguishable from a hung harness. Line buffering costs nothing here.
    try:
        sys.stdout.reconfigure(line_buffering=True)
        sys.stderr.reconfigure(line_buffering=True)
    except AttributeError:  # pragma: no cover — pre-3.7
        pass
    ap = argparse.ArgumentParser(description="Dispatcharr TV on-device player regression harness")
    ap.add_argument("--build", action="store_true", help="cargo + make + make deploy before running")
    ap.add_argument("--filter", "--only", dest="filter", default=None,
                    help="run only cases whose name contains this substring")
    ap.add_argument("--list", action="store_true", help="list cases and exit")
    ap.add_argument("--save-logs", metavar="DIR", default=None,
                    help="write each case's full event log to DIR/<case>.log. The app truncates "
                         "its log every launch and each case overwrites the previous one, so a "
                         "trace not captured here is gone when the run ends.")
    ap.add_argument("--tv", default=None, help="override TV IP (default from the TV config)")
    # Deliberately NOT `choices=`: the Makefile's FLAVORS list is the one whitelist, and a second
    # copy here would be the copy that goes stale. A bad value fails on the first `make -s print-*`
    # with the Makefile's own parse-time error, which names the allowed set.
    ap.add_argument("--flavor", default=None, metavar="stable|debug",
                    help="which INSTALL to drive (default: the Makefile's own default). The two "
                         "builds have separate app ids and runtime roots; everything this run "
                         "touches is derived from this one choice")
    ap.add_argument("--verbose", action="store_true", help="print evidence for passing assertions too")
    ap.add_argument("--no-early", action="store_true",
                    help="don't stop a case as soon as it passes — run the full manifest run_secs. "
                         "Slower by design: it widens the window for a LATE `Playing error` to show "
                         "up, which early exit trades away for speed.")
    ap.add_argument("--fixtures-dir", default=None, metavar="DIR",
                    help=f"where the generated pack lives (default $FIXTURES_OUT/pipeline, "
                         f"i.e. {FIXTURES_ROOT})")
    ap.add_argument("--fixtures-port", type=int, default=0, metavar="PORT",
                    help="port for the fixture HTTP server (default: pick a free one). Pin it when "
                         "a firewall rule names a port")
    args = ap.parse_args()

    manifest = load_manifest(tv_override=args.tv, for_listing=args.list)
    # BEFORE anything else, including --list: every path this run uses hangs off it, and the
    # queries are offline and side-effect free (see make_query / the Makefile's PURE_QUERY).
    resolve_flavour(args, manifest)
    cfg = {
        # `.get`, not `[…]`: under `--list` there may be no address at all. Nothing on a listing
        # path dials it, and a RUN cannot reach here without one (`load_manifest` exits).
        "tv": args.tv or manifest.get("tv", ""),
        "no_early": args.no_early,
        "save_logs": args.save_logs,
    }
    root = args.fixtures_dir or FIXTURES_ROOT
    cases = manifest.get("pipeline_cases", [])
    if args.filter:
        cases = [c for c in cases if args.filter in c["name"]]
    _resolve_fixtures(cases, root)

    if args.list:
        for c in cases:
            d = c.get("declare", {})
            decl = f"{d.get('vcodec','?')}/{d.get('acodec','?')}"
            if d.get("dovi", {}).get("profile"):
                decl += f"+DV{d['dovi']['profile']}"
            if d.get("atmos"):
                decl += "+atmos"
            # Only when it is not the 24p every other fixture runs at — a column that repeats
            # the same number hides the rows that are the point of it.
            if abs(float(d.get("fps", 24.0)) - 24.0) > 0.01:
                decl += f"@{d['fps']:g}"
            # The RESOLUTION, which the matrix cases exist to vary (#50/#51) and which half the
            # filenames do not carry.
            size = c.get("expect", {}).get("video_size", "-")
            mark = f"  [SKIP: {c['skip']}]" if c.get("skip") else ""
            print(f"{c['name']:30s} {decl:22s} {size:11s} {c.get('fixture','?')}{mark}")
        print(f"\nfixtures: {root}")
        print(f"install:  {APPID} [{FLAVOUR}] — triggers and log under {RUNDIR}")
        print("\nNeeds only a TV address and a pack built by `make fixtures-pipeline`.")
        return 0

    cases, skipped = partition_skips(cases)
    # Bail BEFORE the server is started and before arm_teardown: arming commits to driving the
    # television, and its cleanup closes the app on EVERY exit path including this one. A run
    # with nothing to grade must not close an app somebody is watching.
    if not cases:
        sys.exit(f"no case can run (fixtures dir: {root})"
                 + (":\n  " + "\n  ".join(f"{n}  <- {r}" for n, r in skipped)
                    if skipped else f" — --filter {args.filter!r} matched nothing"))
    srv, url_base = serve(root, port=args.fixtures_port,
                          sink=(lambda m: print(f"      [srv] {m}")) if args.verbose else None,
                          peer=cfg["tv"])
    # LIFO: registered after the server and BEFORE arm_teardown, so on the way out the TV is
    # cleaned FIRST and the bytes are pulled second. The other order stops serving an app that
    # is still playing, which turns every interrupted run into a demux failure in the log.
    atexit.register(srv.shutdown)
    print(f"install: {APPID} [{FLAVOUR}] — triggers and log under {RUNDIR}")
    print(f"fixtures: {root}")
    print(f"serving:  {url_base}  (the TV fetches from this address)")
    # The television is a mutex: this run closes the app between cases, wipes the runtime root,
    # moves the account aside and grades a log it assumes only it is writing. Immediately before
    # `arm_teardown`, because `teardown` is what releases the lease, so any statement between
    # taking it and arming is a window where a failure leaks the television.
    acquire_tv_lock(cfg["tv"], f"tests/run.py ({len(cases)} cases) [{FLAVOUR}]")
    arm_teardown(cfg["tv"])
    if args.build:
        do_build(cfg["tv"])
    return run_pipeline_suite(cases, cfg, srv, url_base, args.verbose, skipped)


if __name__ == "__main__":
    # SIGTERM has to UNWIND rather than kill the interpreter outright, or `kill <harness pid>`
    # leaves exactly the state teardown() exists to prevent. SIGINT already raises
    # KeyboardInterrupt, which the same finally catches; a `sys.exit(...)` inside main raises
    # SystemExit, which also runs the finally on its way out (and keeps its own exit status).
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    code = 1
    try:
        code = main()
    except KeyboardInterrupt:
        print("\ninterrupted — closing the app and clearing the TV")
        code = 130
    finally:
        if _TEARDOWN_TV:
            teardown(_TEARDOWN_TV)
            # AFTER teardown, never before: the close, the account restore and the trigger wipe are
            # still device work, and handing the lock back first would let the next lane start
            # driving an app this one is still closing.
            release_tv_lock(_TEARDOWN_TV)
    sys.exit(code)

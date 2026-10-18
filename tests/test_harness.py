#!/usr/bin/env python3
"""
Host unit tests for the harness itself (`tests/run.py`) and for `tools/netcond.py` — stdlib
`unittest`, no TV, no PMS, no OUTBOUND network. Run by `make check` beside the Rust suite and
`ci/flavor.py --selftest`.

("no network" needs one qualification since the netcond tests landed: they bind and drive real
LOOPBACK sockets, because a token bucket is only interesting where it meets a socket, and a test
against the arithmetic alone would grade a function the proxy does not call. Nothing leaves the
machine, and nothing there binds a fixed port.)

Why this file exists at all: run.py is 2100 lines of Python that decides WHAT gets driven on the
one television, and until 2026-08-22 nothing tested a line of it. The specific thing it guards is
the skip channel — the rule that an `item` key this installation cannot resolve skips the cases
that need it instead of killing the run. That rule is invisible on the maintainer's machine, whose
overlay resolves all twelve keys, so a regression in it would be found only by the next stranger
who tried the suite and concluded the harness was broken. Every assertion here is about a code path
that a full local overlay never enters.

The load_manifest tests deliberately read the REAL tests/manifest.json (only the overlay is
synthetic): the invariants being checked — every case ends with an `rk` or a `skip`, a nested key
skips only its owner — are properties of the tracked matrix as it actually stands, and a case added
tomorrow that breaks one of them should fail here rather than on the television.
"""
import concurrent.futures
import functools
import http.client
import importlib.util
import inspect
import io
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(TESTS_DIR)
sys.path.insert(0, TESTS_DIR)
# APPENDED, not inserted: `tools/` is a grab-bag of scripts, and putting it ahead of `tests/` means
# the day somebody adds a `tools/run.py` the import below silently binds the wrong module and this
# whole suite grades a file nobody meant.
sys.path.append(os.path.join(REPO_ROOT, "tools"))
import netcond  # noqa: E402
import run  # noqa: E402  (path juggling above is the point)
import serve_fixtures  # noqa: E402

_FIXTURE_GEN_SPEC = importlib.util.spec_from_file_location(
    "plx_make_fixtures", os.path.join(TESTS_DIR, "fixtures", "make_fixtures.py"))
fixturegen = importlib.util.module_from_spec(_FIXTURE_GEN_SPEC)
_FIXTURE_GEN_SPEC.loader.exec_module(fixturegen)


def _manifest():
    with open(run.MANIFEST) as f:
        return json.load(f)


class TeardownProcessTable(unittest.TestCase):
    def test_non_utf8_argv_cannot_hide_or_crash_a_run_stream_pid(self):
        marker = "/tmp/com.magness.dispatcharrtv.debug/dispatcharrtv-events.log"
        raw = b"431 ssh " + marker.encode("ascii") + b" \xdflegacy\n"
        fake = mock.Mock(return_value=subprocess.CompletedProcess([], 0, stdout=raw))
        with mock.patch.object(run, "RUN_STREAM_MARK", marker):
            self.assertEqual(run._run_stream_pids(fake), {431})
        fake.assert_called_once_with(["ps", "-Ao", "pid,command"], capture_output=True)


class AccountAside(unittest.TestCase):
    """The account move-aside the run wraps every case in, executed for real against a temp tree.

    A set with an account resumes its last channel on launch and the tuned channel outranks the
    clip trigger (2026-10-16: three cases graded a live channel). The command must move every
    candidate file out of the way, put back exactly what was there, and never let a leftover from
    an interrupted run be overwritten by a second move."""

    def _paths(self, root):
        return run.account_paths("com.example.app", os.path.join(root, "app"), os.path.join(root, "rt"))

    def _sh(self, cmd):
        subprocess.run(["sh", "-c", cmd], check=True)

    def test_every_candidate_location_is_covered(self):
        paths = run.account_paths("com.example.app", "/a", "/r")
        for want in ("/media/developer/com.example.app-dispatcharr.json",
                     "/media/internal/.com.example.app-dispatcharr.json",
                     "/a/dispatcharr.json", "/r/dispatcharr.json", "/a/dispatcharr-prefs.json"):
            self.assertIn(want, paths)

    def test_aside_then_restore_puts_back_exactly_what_was_there(self):
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, "app")); os.makedirs(os.path.join(root, "rt"))
            paths = self._paths(root)
            acct = os.path.join(root, "app", "dispatcharr.json")
            with open(acct, "w") as f:
                f.write("REAL")
            os.chmod(acct, 0o600)
            self._sh(run.account_aside_cmd(paths, aside=True))
            self.assertFalse(os.path.exists(acct), "the app must find no account during the run")
            self._sh(run.account_aside_cmd(paths, aside=False))
            with open(acct) as f:
                self.assertEqual(f.read(), "REAL")
            self.assertEqual(os.stat(acct).st_mode & 0o777, 0o600, "mv keeps the mode")
            self.assertFalse(any(os.path.exists(p + run.ASIDE) for p in paths))

    def test_a_leftover_from_an_interrupted_run_is_restored_not_overwritten(self):
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, "app")); os.makedirs(os.path.join(root, "rt"))
            paths = self._paths(root)
            acct = os.path.join(root, "app", "dispatcharr.json")
            with open(acct + run.ASIDE, "w") as f:
                f.write("REAL")   # an interrupted run left the real account aside
            with open(acct, "w") as f:
                f.write("SETUP")  # ...and the app has since written a fresh one
            self._sh(run.account_aside_cmd(paths, aside=True))
            self._sh(run.account_aside_cmd(paths, aside=False))
            with open(acct) as f:
                self.assertEqual(f.read(), "REAL", "the real account survives two moves")

    def test_nothing_to_move_is_not_an_error(self):
        with tempfile.TemporaryDirectory() as root:
            self._sh(run.account_aside_cmd(self._paths(root), aside=True))
            self._sh(run.account_aside_cmd(self._paths(root), aside=False))


class PipelineTier(unittest.TestCase):
    """The Plex-free tier. Every assertion here is about a path the maintainer's own machine takes
    only when it runs --pipeline, and about the two ways this tier can silently do the wrong thing:
    drive the television with nothing to grade, or forge a shell command through the trigger."""

    def _pipeline_cases(self):
        return _manifest()["pipeline_cases"]

    def test_every_case_ends_with_a_path_or_a_skip(self):
        """The invariant behind `case["fixture"]`/`case["path"]` downstream. Run against an EMPTY
        pack, which is every machine that has not built one."""
        cases = self._pipeline_cases()
        self.assertTrue(cases, "the tracked manifest has no pipeline cases?")
        with tempfile.TemporaryDirectory() as empty:
            run._resolve_fixtures(cases, empty)
        for c in cases:
            self.assertTrue(("path" in c) != bool(c.get("skip")),
                            f"{c['name']}: path={c.get('path')!r} skip={c.get('skip')!r}")

    def test_a_present_fixture_resolves_and_a_missing_one_skips(self):
        cases = [{"name": "have", "fixture": "a.mkv"}, {"name": "havenot", "fixture": "b.mkv"},
                 {"name": "unnamed"}]
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "a.mkv"), "wb") as f:
                f.write(b"\0" * 16)   # not real media: ffprobe fails, so the length check no-ops
            run._resolve_fixtures(cases, d)
            self.assertEqual(cases[0]["path"], os.path.join(d, "a.mkv"))
            self.assertNotIn("skip", cases[0])
        self.assertNotIn("path", cases[1])
        self.assertIn("b.mkv", cases[1]["skip"])
        self.assertIn("no `fixture`", cases[2]["skip"])


    def test_the_trigger_payload_is_json_the_app_can_read_and_the_shell_cannot_break(self):
        """Pin both the app-readable JSON and today's compact, pasteable payload vocabulary.

        `apply_triggers` now quotes arbitrary apostrophes too, so this is no longer the command's
        security boundary; the no-apostrophe property remains useful for copied case headers and
        has a twin in rust-modules/media/src/player/playurl.rs (`the_harness_payload_carries_no_apostrophe`).
        """
        for c in self._pipeline_cases():
            files = run.triggers_for_case(c, url_base="http://192.0.2.10:8020")
            self.assertEqual(files[0][0], "dispatcharrtv-playurl")
            payload = files[0][1]
            self.assertNotIn("'", payload, f"{c['name']}: would break the single-quoted printf")
            spec = json.loads(payload)
            self.assertEqual(spec["url"], f"http://192.0.2.10:8020/{c['fixture']}")
            for k, v in c.get("declare", {}).items():
                self.assertEqual(spec[k], v)


    def test_a_declaration_that_was_never_read_fails_rather_than_passing(self):
        """THE false-PASS this tier is most exposed to: the engine's fallthrough arm produces
        `a="AC3"` for an unrecognised or EMPTY audio codec, so a trigger that never got read at all
        yields exactly the right payload for the AC-3 baseline. That is why cases exist whose
        expected load_audio is "AC3 PLUS" and "AAC" — they cannot be reached by accident."""
        unread = ['load: v=H264 a="AC3" fps=0.000 dv=present:0 P0/0 el:0 atmos:0']
        ok, _ = run.a_load_decl(unread, {"load_video": "H265", "load_audio": "AC3 PLUS"})
        self.assertFalse(ok, "an unread declaration must not satisfy an HEVC/E-AC-3 case")
        expected = ['load: v=H265 a="AC3 PLUS" fps=24.000 dv=present:1 P8/1 el:0 atmos:0']
        ok, why = run.a_load_decl(expected, {"load_video": "H265", "load_audio": "AC3 PLUS",
                                             "load_dovi": "P8/1", "load_atmos": False})
        self.assertTrue(ok, why)
        # And the manifest must actually carry such cases, or the defence above is theoretical.
        audios = {c["expect"].get("load_audio") for c in self._pipeline_cases()}
        self.assertTrue({"AC3 PLUS", "AAC"} <= audios,
                        f"no case declares an audio codec the fallthrough cannot produce: {audios}")

    def test_a_missing_load_line_is_a_failure_not_a_silent_pass(self):
        ok, why = run.a_load_decl(["ff: v=#0 codec=h264 codec_id=27 1920x1080 a=#1"],
                                  {"load_video": "H264"})
        self.assertFalse(ok)
        self.assertIn("no `load:` line", why)

    def test_the_wire_assertion_sees_a_seek_that_never_reached_the_demuxer(self):
        """The pump logs its seek intent whether or not the AVIO was ever reached, so a 206 counted
        on the wire is the only proof the Range reopen actually happened."""
        self.assertFalse(run.a_server_wire((3, 0), 2, 1)[0])
        self.assertTrue(run.a_server_wire((3, 1), 2, 1)[0])
        self.assertFalse(run.a_server_wire((0, 0), 1, 0)[0])

    def test_the_stream_path_assertion_catches_playing_the_wrong_thing(self):
        """A stale dispatcharrtv-play from a by-hand session plays a LIBRARY ITEM through a pipeline
        case. Everything else would still pass; the opened path is what tells them apart."""
        good = ["stream: host=192.0.2.10 port=8020 path=/pipe_h264_ac3_1080p.mkv"]
        self.assertTrue(run.a_stream_path(good, "pipe_h264_ac3_1080p.mkv")[0])
        bad = ["stream: host=192.0.2.10 port=32400 path=/library/parts/12/file.mkv?X-Plex-Token=x"]
        self.assertFalse(run.a_stream_path(bad, "pipe_h264_ac3_1080p.mkv")[0])
        self.assertFalse(run.a_stream_path([], "pipe_h264_ac3_1080p.mkv")[0])

        # A tuned live channel (the account resumed it) is what this assertion is FOR: the case
        # would otherwise grade a channel's log as its clip's.
        live = ["stream: 10.0.0.2:9191 path=/proxy/ts/stream/f2fbd334-1c7a-4f40-9a2e-7b8b5a1d0c11"]
        self.assertFalse(run.a_stream_path(live, "pipe_h264_aac_mp4.mp4")[0])

    def test_the_audio_lane_assertion_reads_the_fed_index(self):
        ln = ["ff: v=#0 codec=h264 codec_id=27 1920x1080 trc=1 pri=1 spc=1 a=#2 dur_ns=60000000000"]
        self.assertTrue(run.a_audio_lane(ln, 2)[0])
        self.assertFalse(run.a_audio_lane(ln, 3)[0])
        self.assertFalse(run.a_audio_lane([], 2)[0])

    def test_pos_climb_reads_only_the_heartbeat(self):
        """The position comes from the 1 Hz render heartbeat alone; an absent heartbeat must fail
        rather than fold back to some other series and read as a pass."""
        self.assertFalse(run.a_timeline_climb(["timeline playing t=10s/60s",
                                               "timeline playing t=20s/60s"], 8)[0])
        heartbeat = [f"loop=60 route=player overlay=none pos={t}s vtick=5" for t in (2, 14)]
        self.assertTrue(run.a_timeline_climb(heartbeat, 8)[0])

    def test_the_harness_needs_only_a_tv_address(self):
        """A stranger with nothing configured but a TV address must still load the cases."""
        m = run.load_manifest(tv_override="10.0.0.9")
        self.assertEqual(m["tv"], "10.0.0.9")
        self.assertTrue(m["pipeline_cases"])
        # ...and a TV address is the ONLY thing this path can die for. `.tv-host` / the per-user
        # tv-host are the maintainer's own fallback and exist on this machine, so point the
        # resolver at a script that cannot be run.
        saved_script = run.tv_config.SCRIPT
        run.tv_config.SCRIPT = os.path.join(TESTS_DIR, "no-such-tv-config.sh")
        try:
            with self.assertRaises(SystemExit):
                run.load_manifest(tv_override=None)
            self.assertNotIn("tv", run.load_manifest(tv_override=None, for_listing=True))
        finally:
            run.tv_config.SCRIPT = saved_script


class ResolutionSpike(unittest.TestCase):
    """The offline half of a single-Load 720p -> 1080p -> 720p device experiment."""

    @staticmethod
    def _case():
        return next(c for c in _manifest()["pipeline_cases"]
                    if c["name"] == "pipe_h264_aac_resolution_spike")

    @staticmethod
    def _gst_line(ms, payload):
        whole = int(ms // 1000)
        nanos = int(round((ms - whole * 1000) * 1_000_000))
        return f"0:00:{whole:02d}.{nanos:09d} 123 GST_DEBUG {payload}"

    def _good_trace(self):
        return [self._gst_line(
                    500 + i * (1000 / 24),
                    "gst_lx_videosink_render:<lxvideosink0> [PLAYING] received buffer")
                for i in range(410)]

    @staticmethod
    def _source_info(width, height):
        return ('smp_cb type=4 num=0 str={"context":"test","video":'
                f'{{"width":{width},"height":{height}}}}}')

    def test_manifest_requires_one_load_one_wire_open_and_no_reload(self):
        c = self._case()
        self.assertEqual(c["run_secs"], 35)
        self.assertEqual(c["expect"]["starfish_resolution_sequence"],
                         ["1280x720", "1920x1080", "1280x720"])
        self.assertEqual(c["expect"]["resolution_boundaries_s"], [8, 16])
        self.assertEqual(c["expect"]["load_count_exact"], 1)
        self.assertTrue(c["expect"]["require_audio_feed_ready"])
        self.assertTrue(c["expect"]["no_reload"])
        self.assertEqual(c["expect"]["server_opens_exact"], 1)
        self.assertEqual(c["expect"]["server_range_opens_exact"], 0)
        files = dict(run.triggers_for_case(c, url_base="http://192.0.2.10:8020"))
        self.assertEqual(files["dispatcharrtv-gstlog"], c["gst_trace"]["debug"])
        self.assertEqual(files["dispatcharrtv-gstlog"], "lxvideosink:6")

    def test_apply_triggers_removes_a_stale_trace_before_arming(self):
        saved = run.RUNDIR
        run.RUNDIR = "/tmp/com.magness.dispatcharrtv.debug"
        try:
            with mock.patch.object(run, "ssh") as ssh:
                run.apply_triggers("192.0.2.20", [("dispatcharrtv-gstlog", "GST_EVENT:6")])
            command = ssh.call_args.args[1]
            self.assertIn("rm -f /tmp/com.magness.dispatcharrtv.debug/dispatcharrtv-gst.log", command)
            self.assertIn("dispatcharrtv-gstlog", command)
        finally:
            run.RUNDIR = saved

    def test_exact_session_and_wire_counts_reject_hidden_reopens(self):
        load = 'load: v=H264 a="AAC" fps=24.000 dv=present:0 P0/0 el:0 atmos:0'
        self.assertTrue(run.a_load_count([load], 1)[0])
        self.assertFalse(run.a_load_count([load, load], 1)[0])
        self.assertTrue(run.a_no_reload([load])[0])
        self.assertFalse(run.a_no_reload([load, "reload_at: 8000ms"])[0])
        self.assertTrue(run.a_server_wire((1, 0), 1, 0, 1, 0)[0])
        self.assertFalse(run.a_server_wire((2, 1), 1, 0, 1, 0)[0])

    def test_audio_readiness_requires_an_accepted_starfish_feed(self):
        accepted = "feed a#4 sz=512 fed=42666667 reply=O qbytes=1024"
        rejected = "feed a#1 sz=512 fed=0 reply=B qbytes=2048"
        self.assertTrue(run.a_audio_feed_ready([accepted])[0])
        self.assertFalse(run.a_audio_feed_ready([rejected])[0])
        self.assertFalse(run.a_audio_feed_ready([])[0])

    def test_starfish_source_info_requires_the_exact_resolution_sequence(self):
        c = self._case()
        lines = [self._source_info(1280, 720),
                 ('smp_cb type=4 num=0 str={"sourceInfo":{"context":"test","video":'
                  '{"width":1920,"height":1080}}}'),
                 self._source_info(1280, 720)]
        want = c["expect"]["starfish_resolution_sequence"]
        self.assertTrue(run.a_starfish_resolution_sequence(lines, want)[0])
        self.assertFalse(run.a_starfish_resolution_sequence(lines[:-1], want)[0])

    def test_physical_packet_verifier_rejects_a_track_stored_in_one_lump(self):
        good = [
            {"stream_index": 0, "pts_time": "0.000", "pos": "100"},
            {"stream_index": 1, "pts_time": "0.000", "pos": "200"},
            {"stream_index": 1, "pts_time": "0.021", "pos": "300"},
            {"stream_index": 0, "pts_time": "0.042", "pos": "400"},
            {"stream_index": 1, "pts_time": "0.043", "pos": "500"},
            {"stream_index": 0, "pts_time": "0.083", "pos": "600"},
        ]
        skew, counts = fixturegen.physical_av_interleave(list(reversed(good)), 0, 1)
        self.assertLess(skew, 0.050)  # input order is irrelevant; byte `pos` is the authority
        self.assertEqual(counts, {0: 3, 1: 3})

        lumped = [
            {"stream_index": 0, "pts_time": f"{i / 10:.1f}", "pos": str(i * 100)}
            for i in range(11)
        ] + [
            {"stream_index": 1, "pts_time": f"{i / 10:.1f}", "pos": str(2000 + i * 100)}
            for i in range(11)
        ]
        skew, _ = fixturegen.physical_av_interleave(lumped, 0, 1)
        self.assertGreaterEqual(skew, 1.0)
        with self.assertRaises(ValueError):
            fixturegen.physical_av_interleave(lumped[:4], 0, 1)

    def test_gst_trace_accepts_metronomic_boundaries(self):
        c = self._case()
        ok, why = run.a_gst_resolution_trace(self._good_trace(), c["expect"], c["gst_trace"])
        self.assertTrue(ok, why)

    def test_gst_trace_rejects_a_boundary_stall(self):
        c = self._case()
        stalled = [ln for ln in self._good_trace()
                   if "received buffer" not in ln or not (8500 <= run.gst_clock_ms(ln) <= 8800)]
        ok, why = run.a_gst_resolution_trace(stalled, c["expect"], c["gst_trace"])
        self.assertFalse(ok)
        self.assertIn("first picture", why)


class DefaultTier(unittest.TestCase):
    """Which tier a bare `./tests/run.py` runs. Inverted 2026-08-22: the synthetic pipeline tier
    is the default and `--server` opts into the library-backed one. Pinned here because the
    inversion is invisible from any single code path — it is one boolean in `main()` — and getting
    it backwards means a bare command either demands credentials nobody has, or silently grades a
    tier the operator did not ask for."""

    @staticmethod
    def _list(*flags):
        """`./tests/run.py <flags> --list` as a completed process. `--list` is offline and
        side-effect free, which is what makes spawning the real CLI the honest way to ask which
        tier a set of flags selects — the rule lives in `main()` and cannot be imported."""
        return subprocess.run([sys.executable, os.path.join(TESTS_DIR, "run.py"), *flags, "--list"],
                              capture_output=True, text=True, timeout=120)

    def test_the_bare_command_is_the_synthetic_tier(self):
        """Documented in three places (tests/README.md, docs/agent-reference.md, --help); pinned in one."""
        out = self._list()
        self.assertIn("pipe_", out.stdout, "a bare --list must show the synthetic cases")
        self.assertNotIn("dp_h264_ac3_1080p", out.stdout,
                         "a bare --list must NOT show the library-backed cases")


    def test_contradictory_tier_flags_refuse(self):
        """`--pipeline` names the default, so pairing it with --server/--fps is two instructions,
        not a preference — honouring either one silently is how somebody trusts the wrong result."""
        for extra in ("--server", "--fps", "--fps-player"):
            self.assertNotEqual(self._list("--pipeline", extra).returncode, 0,
                                f"--pipeline {extra} should refuse")


    def test_the_manifest_declares_a_frame_rate_axis(self):
        """Every fixture ran at 24p until 2026-08-22, so `engine::fps_rational`'s branches had one
        input between them. Pin that the matrix now carries both sides of its split: a
        1001-denominator broadcast rate and an integer one."""
        rates = {c["declare"].get("fps") for c in _manifest()["pipeline_cases"]}
        self.assertTrue(any(abs(r - 59.94) < 0.01 for r in rates if r),
                        f"no 1001-denominator rate in the matrix: {rates}")
        self.assertTrue(any(r and r > 24 and float(r).is_integer() for r in rates),
                        f"no integer high frame rate in the matrix: {rates}")

    def test_the_direct_play_payload_matrix_is_fully_covered(self):
        """The player direct-plays exactly {H264,H265} x {AC3,AC3 PLUS,AAC} — route.rs's codec gate
        and plex::DP_AUDIO_CODECS. Six payload combinations, and this suite is the only tier that
        can reach all six without owning media of every shape."""
        combos = {(c["expect"].get("load_video"), c["expect"].get("load_audio"))
                  for c in _manifest()["pipeline_cases"]}
        want = {(v, a) for v in ("H264", "H265") for a in ("AC3", "AC3 PLUS", "AAC")}
        self.assertEqual(want - combos, set(), f"uncovered Load payload combinations: {want - combos}")


class ResolutionMatrix(unittest.TestCase):
    """LG App Self Checklist #50 / #51, which is graded as a MATRIX and was answered as pieces.

    Two halves, and the second is the one that rots: the assertion has to be EXACT, and the matrix
    has to stay complete as cases are added and renamed. A `min_video_width` cannot tell 720x480
    from 720x576, so a matrix built on it would read as covered while grading nothing.
    """

    CELLS = {("h264", "720x480"), ("h264", "1280x720"), ("h264", "1920x1080"),
             ("h264", "3840x2160"), ("hevc", "720x480"), ("hevc", "1280x720"),
             ("hevc", "1920x1080"), ("hevc", "3840x2160")}

    FF = ("ff: v=#0 codec={0} codec_id=27 {1} trc=1 pri=1 spc=1 a=#1 dur_ns=60000000000\n")

    @staticmethod
    def _cases():
        return _manifest()["pipeline_cases"]

    def test_all_eight_cells_exist(self):
        got = {(c["expect"]["codec"], c["expect"]["video_size"]) for c in self._cases()
               if "resolution-matrix" in c.get("covers", [])}
        self.assertEqual(self.CELLS - got, set(), f"uncovered resolution x codec cells: "
                                                  f"{self.CELLS - got}")

    def test_every_matrix_cell_grades_the_size_exactly(self):
        """...and none of them falls back to a width bound, which is the shape being replaced."""
        for c in self._cases():
            if "resolution-matrix" not in c.get("covers", []):
                continue
            self.assertIn("video_size", c["expect"], c["name"])
            self.assertNotIn("min_video_width", c["expect"],
                             f"{c['name']}: video_size subsumes min_video_width — keeping both "
                             f"leaves two statements of one number that nothing keeps in step")

    def test_a_codec_rejects_the_wrong_raster(self):
        """The whole value of the exact form: a 4:3 SD clip must not satisfy a 16:9 SD cell."""
        ok, why = run.a_codec([self.FF.format("h264", "720x480")], "h264", 0, "720x480")
        self.assertTrue(ok, why)
        ok, why = run.a_codec([self.FF.format("h264", "720x576")], "h264", 0, "720x480")
        self.assertFalse(ok, "720x576 must not pass a 720x480 cell")
        self.assertIn("720x576", why)
        # ...and a width bound would have passed it, which is the point.
        self.assertTrue(run.a_codec([self.FF.format("h264", "720x576")], "h264", 700)[0])

    def test_a_codec_still_grades_the_codec_and_the_width_alone(self):
        """The non-matrix cases pass no `size`, and their behaviour must be bit-identical."""
        self.assertTrue(run.a_codec([self.FF.format("hevc", "3840x2160")], "hevc", 3800)[0])
        self.assertFalse(run.a_codec([self.FF.format("h264", "3840x2160")], "hevc", 3800)[0])
        self.assertFalse(run.a_codec([self.FF.format("hevc", "1920x1080")], "hevc", 3800)[0])
        self.assertFalse(run.a_codec([], "hevc", 0, "1920x1080")[0])

    def test_each_matrix_fixture_is_named_by_exactly_one_cell(self):
        """A cell reusing another's file would be a matrix with a hole in it that reads as full —
        the exact failure `pipe_hevc_aac_mp4`-as-the-FHD-HEVC-rung would have been."""
        fixtures = [c["fixture"] for c in self._cases()
                    if "resolution-matrix" in c.get("covers", [])]
        self.assertEqual(len(fixtures), len(set(fixtures)), f"a fixture serves two cells: "
                                                            f"{sorted(fixtures)}")


class CompletionCase(unittest.TestCase):
    """LG #46's first half — a stream that runs OUT and an app that leaves the player.

    Every other case in both tiers is built so the clip CANNOT end inside its window, so this is
    the one place the finish path is exercised at all, and the assertion behind it has to refuse
    three near-misses that each look like a pass.
    """

    EOS = "EOS reached: playpos=19s/20s -> ended\n"
    TORN = "stop_bufferfeed: torn down\n"
    POS = "loop=60 route=player overlay=none pos=19s vtick=5\n"

    def test_the_finish_needs_both_lines_in_order(self):
        self.assertTrue(run.a_finished([self.POS, self.EOS, self.TORN])[0])

    def test_a_teardown_without_an_eos_is_not_a_finish(self):
        """Every stop tears the engine down, the harness's own close included — so an unordered
        match would pass on a clip that never ended."""
        ok, why = run.a_finished([self.POS, self.TORN])
        self.assertFalse(ok)
        self.assertIn("EOS reached", why)
        ok, why = run.a_finished([self.TORN, self.EOS])
        self.assertFalse(ok, "a teardown BEFORE the EOS is the previous session's, not this one's")

    def test_an_earlier_reload_teardown_does_not_poison_the_finish(self):
        """`teardown` writes the same line for a `for_reload` stop — a seek that escalated to
        `reload_at`, or an app-switch suspend. Comparing the FIRST teardown's index against the
        EOS fails such a run for its whole cap, reading as "the player froze on the last frame",
        which is precisely the false regression this assertion exists to avoid."""
        ok, why = run.a_finished([self.TORN, self.POS, self.EOS, self.TORN])
        self.assertTrue(ok, why)

    def test_an_eos_that_never_tore_down_is_a_frozen_last_frame(self):
        ok, why = run.a_finished([self.POS, self.EOS])
        self.assertFalse(ok)
        self.assertIn("froze", why)

    def test_only_a_case_that_asks_to_reach_the_end_may_name_the_short_clip(self):
        """The rule is not "exactly one case", which is what it said while there was one — it is
        that a case graded through a TEARDOWN has to have asked for one. The short clip ends inside
        every window, so any case naming it without `reaches_eos` would have its assertions cut off
        by a finish it never declared."""
        cases = _manifest()["pipeline_cases"]
        eos = [c for c in cases if c["expect"].get("reaches_eos")]
        self.assertTrue(eos, "no case reaches EOS — LG #46 has nothing behind it")
        short = {c["fixture"] for c in eos}
        self.assertEqual(len(short), 1, f"the completion cases disagree on which clip ends: {short}")
        strays = [c["name"] for c in cases
                  if c["fixture"] in short and not c["expect"].get("reaches_eos")]
        self.assertFalse(strays, f"the short clip ENDS mid-window; {strays} would be graded "
                                 f"through a teardown they never asked for")

    def test_the_replay_case_grades_both_halves_of_46(self):
        """#46 is "replay AFTER completion", so the case that grades the replay must also be the
        one that reaches the end — a replay asserted without a finish behind it would pass on a
        stream that was merely restarted mid-play."""
        cases = _manifest()["pipeline_cases"]
        replays = [c for c in cases if c["expect"].get("replays")]
        self.assertEqual(len(replays), 1, f"expected one replay case, got {[c['name'] for c in replays]}")
        c = replays[0]
        self.assertTrue(c["expect"].get("reaches_eos"), f"{c['name']} replays without finishing")
        # The second viewing must fetch the clip again — `teardown` closed the socket and cleared
        # the URL — so a floor of 2 opens is the wire-side half of the same assertion.
        self.assertGreaterEqual(c["expect"].get("server_opens_min", 0), 2, c["name"])

    def test_the_replay_trigger_carries_the_number_the_case_grades(self):
        """One statement, not two: `expect.replays` is what the harness writes into
        `dispatcharrtv-replay` AND what `a_replayed` counts, so they cannot drift."""
        c = next(c for c in _manifest()["pipeline_cases"] if c["expect"].get("replays"))
        files = dict(run.triggers_for_case(c, url_base="http://192.0.2.10:8020"))
        self.assertEqual(files.get("dispatcharrtv-replay"), str(c["expect"]["replays"]))
        # ...and every other case must NOT arm it, or a one-shot boot silently becomes a loop.
        for other in _manifest()["pipeline_cases"]:
            if other["expect"].get("replays"):
                continue
            self.assertNotIn("dispatcharrtv-replay",
                             dict(run.triggers_for_case(other, url_base="http://192.0.2.10:8020")),
                             other["name"])

    def test_a_replay_is_seen_as_a_fall_then_a_climb(self):
        """The three signals, and the three near-misses that each look like a pass."""
        rep = "replay: starting the finished stream again (0 left)\n"
        load = 'load: v=H264 a="AC3" fps=24.000 dv=present:0 P0/0 el:0 atmos:0\n'
        pos = [f"loop=60 route=player overlay=none pos={t}s vtick=5\n" for t in (2, 10, 19)]
        pos2 = [f"loop=60 route=player overlay=none pos={t}s vtick=5\n" for t in (1, 9, 18)]
        good = [load] + pos + [rep, load] + pos2
        ok, why = run.a_replayed(good, 1)
        self.assertTrue(ok, why)

        # (a) the app never re-entered — no `replay:` line at all.
        ok, why = run.a_replayed([load] + pos, 1)
        self.assertFalse(ok)
        self.assertIn("never re-entered", why)
        # (b) it fired more often than asked: a loop, which every other signal would accept.
        ok, why = run.a_replayed([load] + pos + [rep, load] + pos2 + [rep, load] + pos2, 1)
        self.assertFalse(ok)
        self.assertIn("loop", why)
        # (c) it fired and the payload was never rebuilt — one `load:` for one replay.
        ok, why = run.a_replayed([load] + pos + [rep] + pos2, 1)
        self.assertFalse(ok)
        self.assertIn("rebuilt its payload", why)
        # (d) it fired, reloaded, and playback CARRIED ON rather than restarting.
        carried = [f"loop=60 route=player overlay=none pos={t}s vtick=5\n" for t in (20, 28, 37)]
        ok, why = run.a_replayed([load] + pos + [rep, load] + carried, 1)
        self.assertFalse(ok)
        self.assertIn("never fell", why)
        # (e) it restarted and then stalled at the join — a fall with no second viewing.
        stalled = ["loop=60 route=player overlay=none pos=0s vtick=5\n"] * 3
        ok, why = run.a_replayed([load] + pos + [rep, load] + stalled, 1)
        self.assertFalse(ok)
        self.assertIn("did not play", why)

    def test_the_climb_is_measured_from_the_drop_and_not_from_the_global_floor(self):
        """THE false PASS this assertion shipped with, and the ordering the field will produce.

        The first version anchored the post-drop climb at the global floor, which is a VALUE — so
        it only landed in viewing 2 when viewing 2 happened to reach viewing 1's minimum. The
        `pos=` heartbeat is 1 Hz and free-running, so viewing 1 logging `pos=0s` while viewing 2's
        first sample lands at `pos=1s` put the anchor back in viewing 1 and measured VIEWING 1'S
        OWN CLIMB: `[0,5,10,19,1]` read as "fell 18s then climbed 19s" and PASSED with the second
        viewing having produced one sample and zero seconds of playback.

        It is also the state the harness normally grades, because it exits the moment every
        assertion passes — so this was not a corner, it was the common case. The class's other
        near-miss test passes only because its series starts at 2 rather than 0, which is exactly
        the kind of accident a regression test is for.
        """
        rep = "replay: starting the finished stream again (0 left)\n"
        load = 'load: v=H264 a="AC3" fps=24.000 dv=present:0 P0/0 el:0 atmos:0\n'

        def pos(t):
            return f"loop=60 route=player overlay=none pos={t}s vtick=5\n"

        # Viewing 1 reaches 0; viewing 2's only sample is 1, so the global floor sits in viewing 1.
        stalled = [load, pos(0), pos(5), pos(10), pos(19), rep, load, pos(1)]
        ok, why = run.a_replayed(stalled, 1)
        self.assertFalse(ok, "a replay with one sample and no playback must not pass")
        self.assertIn("did not play", why)
        # ...and the same shape with a real second viewing still passes.
        played = [load, pos(0), pos(5), pos(10), pos(19), rep, load, pos(1), pos(9), pos(18)]
        ok, why = run.a_replayed(played, 1)
        self.assertTrue(ok, why)

    def test_a_pack_too_short_to_be_played_twice_skips_the_replay_case(self):
        """The EOS bound is per VIEWING: a clip that fits once inside the cap need not fit twice."""
        case = {"name": "rep", "fixture": "c.mkv", "run_secs": 60,
                "expect": {"reaches_eos": True, "replays": 1, "min_pos_climb_s": 8}}
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "c.mkv"), "wb") as f:
                f.write(b"\0" * 16)
            saved = run._probe_fixture
            try:
                # 25 s fits once inside 60 * 0.6 = 36, and twice does not.
                run._probe_fixture = lambda p: (25.0, [("video", "h264"), ("audio", "ac3")])
                run._resolve_fixtures([case], d)
                self.assertIn("skip", case)
                self.assertIn("2x", case["skip"])
                once = dict(case)
                once.pop("skip", None)
                once["expect"] = dict(case["expect"], replays=0)
                run._resolve_fixtures([once], d)
                self.assertNotIn("skip", once)
            finally:
                run._probe_fixture = saved

    def test_a_pack_too_long_to_finish_skips_rather_than_fails(self):
        """A `--secs`/`--quick` regeneration is the realistic way to break this, and `a_finished`
        failing on a 300 s clip reads as the app freezing on the last frame."""
        case = {"name": "eos", "fixture": "c.mkv", "run_secs": 60,
                "expect": {"reaches_eos": True, "min_pos_climb_s": 8}}
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "c.mkv")
            with open(path, "wb") as f:
                f.write(b"\0" * 16)
            saved = run._probe_fixture
            try:
                run._probe_fixture = lambda p: (300.0, [("video", "h264"), ("audio", "ac3")])
                run._resolve_fixtures([case], d)
                self.assertIn("skip", case)
                self.assertIn("to the END", case["skip"])
                # ...and the same clip inside the budget resolves.
                short = dict(case)
                short.pop("skip", None)
                run._probe_fixture = lambda p: (20.0, [("video", "h264"), ("audio", "ac3")])
                run._resolve_fixtures([short], d)
                self.assertNotIn("skip", short)
                self.assertEqual(short["path"], path)
            finally:
                run._probe_fixture = saved


class FixtureKeepAlive(unittest.TestCase):
    """Sequential media GETs against serve_fixtures.py must reuse one TCP connection.

    The pipeline server used to force `Connection: close` on every 2xx, so keep-alive in
    stream.rs was never exercised by the local player path. These tests bind loopback only.
    """

    def _serve(self, root):
        srv = serve_fixtures.FixtureServer(root, 0, bind="127.0.0.1")
        thread = threading.Thread(target=srv.serve_forever, daemon=True)
        thread.start()
        return srv

    def test_sequential_gets_reuse_one_accept(self):
        with tempfile.TemporaryDirectory() as root:
            with open(os.path.join(root, "clip.bin"), "wb") as stream:
                stream.write(b"ABCDEFGH")
            srv = self._serve(root)
            try:
                host, port = srv.server_address
                conn = http.client.HTTPConnection(host, port, timeout=5)
                conn.request("GET", "/clip.bin")
                first = conn.getresponse()
                body1 = first.read()
                header1 = (first.getheader("Connection") or "").lower()
                self.assertEqual(first.status, 200)
                self.assertEqual(body1, b"ABCDEFGH")
                self.assertNotEqual(header1, "close")
                conn.request("GET", "/clip.bin")
                second = conn.getresponse()
                body2 = second.read()
                self.assertEqual(second.status, 200)
                self.assertEqual(body2, b"ABCDEFGH")
                self.assertEqual(srv.n_opens, 2)
                self.assertEqual(srv.n_accepts, 1)
                conn.close()
            finally:
                srv.shutdown()
                srv.server_close()

    def test_range_get_still_answers_206_on_a_kept_alive_connection(self):
        with tempfile.TemporaryDirectory() as root:
            with open(os.path.join(root, "clip.bin"), "wb") as stream:
                stream.write(b"ABCDEFGH")
            srv = self._serve(root)
            try:
                host, port = srv.server_address
                conn = http.client.HTTPConnection(host, port, timeout=5)
                conn.request("GET", "/clip.bin")
                whole = conn.getresponse()
                self.assertEqual(whole.status, 200)
                self.assertEqual(whole.read(), b"ABCDEFGH")
                conn.request("GET", "/clip.bin", headers={"Range": "bytes=4-"})
                part = conn.getresponse()
                self.assertEqual(part.status, 206)
                self.assertEqual(part.read(), b"EFGH")
                self.assertEqual(int(part.getheader("Content-Length", -1)), 4)
                self.assertEqual(srv.n_opens, 2)
                self.assertEqual(srv.n_ranged, 1)
                self.assertEqual(srv.n_accepts, 1)
                conn.close()
            finally:
                srv.shutdown()
                srv.server_close()

    def test_error_replies_still_close_the_connection(self):
        with tempfile.TemporaryDirectory() as root:
            with open(os.path.join(root, "clip.bin"), "wb") as stream:
                stream.write(b"ABCDEFGH")
            srv = self._serve(root)
            try:
                host, port = srv.server_address
                conn = http.client.HTTPConnection(host, port, timeout=5)
                conn.request("GET", "/missing.bin")
                missing = conn.getresponse()
                self.assertEqual(missing.status, 404)
                missing.read()
                header = (missing.getheader("Connection") or "").lower()
                self.assertEqual(header, "close")
                conn.request("GET", "/clip.bin")
                ok = conn.getresponse()
                self.assertEqual(ok.status, 200)
                self.assertEqual(ok.read(), b"ABCDEFGH")
                self.assertEqual(srv.n_accepts, 2)
                conn.close()
            finally:
                srv.shutdown()
                srv.server_close()

    def test_client_reset_while_waiting_for_the_next_request_is_not_an_error(self):
        """A seek/teardown RST during the keep-alive wait must not traceback.

        BaseHTTPRequestHandler.handle loops on readline(); the TV's RST is
        ConnectionResetError, not the empty-line close the stdlib handles.
        """
        import struct
        with tempfile.TemporaryDirectory() as root:
            with open(os.path.join(root, "clip.bin"), "wb") as stream:
                stream.write(b"ABCDEFGH")
            notes = []
            srv = serve_fixtures.FixtureServer(
                root, 0, sink=notes.append, bind="127.0.0.1")
            thread = threading.Thread(target=srv.serve_forever, daemon=True)
            thread.start()
            try:
                host, port = srv.server_address
                conn = http.client.HTTPConnection(host, port, timeout=5)
                conn.request("GET", "/clip.bin")
                first = conn.getresponse()
                self.assertEqual(first.status, 200)
                self.assertEqual(first.read(), b"ABCDEFGH")
                conn.sock.setsockopt(
                    socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
                conn.close()
                deadline = time.monotonic() + 1.0
                while time.monotonic() < deadline and not any(
                    "reset keep-alive" in n for n in notes
                ):
                    time.sleep(0.02)
                self.assertTrue(
                    any("reset keep-alive" in n for n in notes),
                    f"handler did not swallow the RST; notes={notes!r}",
                )
                conn2 = http.client.HTTPConnection(host, port, timeout=5)
                conn2.request("GET", "/clip.bin")
                second = conn2.getresponse()
                self.assertEqual(second.status, 200)
                self.assertEqual(second.read(), b"ABCDEFGH")
                conn2.close()
                self.assertEqual(srv.n_accepts, 2)
            finally:
                srv.shutdown()
                srv.server_close()

    def test_finish_raising_on_a_reset_socket_does_not_traceback(self):
        """`finish()` runs in socketserver's own `finally`, independent of `handle()`.

        StreamRequestHandler.finish() only guards its own wfile.flush() against socket.error;
        the wfile/rfile close() calls right after it are unguarded, so closing an already-reset
        socket's wrapped file objects can still raise there. Force that exact raise and confirm
        FixtureHandler.finish swallows it instead of letting socketserver.ThreadingMixIn's
        process_request_thread print an uncaught traceback — the real-TV regression a normal
        seek/teardown exposed once keep-alive made this path live on every request, not just
        the first.
        """
        import socketserver
        with tempfile.TemporaryDirectory() as root:
            with open(os.path.join(root, "clip.bin"), "wb") as stream:
                stream.write(b"ABCDEFGH")
            notes = []
            srv = serve_fixtures.FixtureServer(
                root, 0, sink=notes.append, bind="127.0.0.1")
            thread = threading.Thread(target=srv.serve_forever, daemon=True)
            thread.start()
            stderr_capture = io.StringIO()
            try:
                with mock.patch.object(
                    socketserver.StreamRequestHandler, "finish",
                    side_effect=ConnectionResetError("simulated"),
                ), mock.patch("sys.stderr", stderr_capture):
                    host, port = srv.server_address
                    conn = http.client.HTTPConnection(host, port, timeout=5)
                    conn.request("GET", "/clip.bin")
                    r = conn.getresponse()
                    self.assertEqual(r.status, 200)
                    self.assertEqual(r.read(), b"ABCDEFGH")
                    conn.close()
                    deadline = time.monotonic() + 1.0
                    while time.monotonic() < deadline and not any(
                        "keep-alive (finish)" in n for n in notes
                    ):
                        time.sleep(0.02)
            finally:
                srv.shutdown()
                srv.server_close()
            self.assertTrue(
                any("keep-alive (finish)" in n for n in notes),
                f"finish() did not swallow the reset; notes={notes!r}",
            )
            self.assertNotIn(
                "Traceback", stderr_capture.getvalue(),
                "an exception from finish() must not reach socketserver's own handle_error",
            )


class NetcondRate(unittest.TestCase):
    """`tools/netcond.py`'s `rate:<kbps>` — the mode LG #43 CASE1's legs are produced with.

    Driven through the REAL proxy (`start_proxy` -> `serve_conn` -> `relay`) over loopback rather
    than against the bucket alone: the bucket is arithmetic and cannot be wrong in an interesting
    way, while everything that HAS been wrong here lives at the seam — the scope thrown away
    before it reached `relay`, tokens charged for bytes that never arrived, a mode that could not
    be changed under an open transfer.

    Graded from ABOVE only. A shaper must not EXCEED what was asked for; a lower bound would be
    grading this machine's scheduler under whatever else `make check` is running, which is the
    flaky direction.
    """

    #: 0.128 s per throttled pull. Every timing below is a multiple of that, and the class as a
    #: whole is budgeted at half a second: `make check` is the command run before every other one.
    N = 32 * 1024
    KBPS = 2048.0
    #: The wall time a full transfer CANNOT beat at KBPS.
    FLOOR_S = N * 8 / (KBPS * 1000)

    def setUp(self):
        # A cleaned-up temp dir, like every other temp use in this file: six of these leaked per
        # `make check` while it was a bare `mkdtemp`, and `make check` is the command run most.
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.ctl = os.path.join(self.tmp, "netcond.mode")
        # The proxy narrates every connection; a test runner is not where that belongs.
        saved_sink = netcond.SINK
        netcond.SINK = lambda _msg: None
        self.addCleanup(lambda: setattr(netcond, "SINK", saved_sink))
        self.origin, oport = netcond.start_origin(self.N)
        self.addCleanup(self.origin.close)
        self.mode = netcond.Mode(self.ctl, "pass")
        self._set("pass")
        self.proxy, self.port = netcond.start_proxy(
            0,
            ("127.0.0.1", oport),
            self.mode,
            bind="127.0.0.1",
            allow_clients=["127.0.0.1"],
        )
        self.addCleanup(self.proxy.close)

    def _set(self, raw):
        with open(self.ctl, "w") as f:
            f.write(raw)

    def _pull(self, path="/library/parts/1/file.mkv", on_first_bytes=None):
        netcond.BUCKET.reset()
        c = socket.create_connection(("127.0.0.1", self.port), timeout=30)
        try:
            c.sendall(f"GET {path} HTTP/1.1\r\nHost: x\r\n\r\n".encode())
            t0 = time.monotonic()
            got = b""
            while len(got) < self.N:
                b = c.recv(65536)
                if not b:
                    break
                if not got and on_first_bytes:
                    on_first_bytes()
                got += b
            return time.monotonic() - t0, got
        finally:
            c.close()

    def test_the_bucket_never_hands_out_more_than_the_rate(self):
        """1 kbps = 1000 bits/s, decimal — the unit the checklist item states its legs in."""
        b = netcond.RateBucket()
        t0 = time.monotonic()
        total = 0
        while time.monotonic() - t0 < 0.1:
            total += b.take(512, 1 << 20)
        # 512 kbps = 64000 B/s; over the elapsed window, plus the burst capacity it may hold.
        allowed = 64000.0 * (time.monotonic() - t0) + max(64000.0 * b.BURST_S, b.MIN_CAP)
        self.assertLessEqual(total, allowed, f"granted {total} bytes, ceiling {allowed:.0f}")

    def test_the_bucket_starts_empty(self):
        """A full one hands the first quarter-second a free burst — which, on transfers this
        short, is most of the transfer, and makes an absent throttle measure as a working one.

        Graded against what a FULL bucket would grant rather than against zero: `take` refills from
        the wall clock, so any descheduling between the constructor and the first call accrues real
        tokens (2 bytes = 31 us at 512 kbps, and an 8-up `make check` produces exactly that). An
        exact `== 0` here is a test of the machine's scheduler, not of the bucket.
        """
        b = netcond.RateBucket()
        fresh = b.take(512, 1 << 20)
        full = max(512 * 1000 / 8 * b.BURST_S, b.MIN_CAP)
        self.assertLess(fresh, full / 8,
                        f"a fresh bucket granted {fresh} bytes; a FULL one grants {full:.0f}")

    def test_a_rate_mode_shapes_a_real_transfer(self):
        self._set("pass")
        free_s, free = self._pull()
        self.assertEqual(len(free), self.N)
        self._set(f"rate:{self.KBPS:g}")
        slow_s, slow = self._pull()
        self.assertEqual(slow, free, "the shaper corrupted or truncated the body")
        self.assertGreater(slow_s, free_s, "rate: changed nothing")
        self.assertLessEqual(len(slow) * 8 / slow_s / 1000, self.KBPS * 1.35,
                             f"measured faster than the requested {self.KBPS:g} kbps "
                             f"(floor {self.FLOOR_S:.3f}s, took {slow_s:.3f}s)")

    def test_a_non_loopback_listener_requires_a_client_allowlist(self):
        """PMS URLs carry credentials; a LAN-wide forwarding proxy may not be open by default."""
        with self.assertRaisesRegex(ValueError, "requires at least one allowed client"):
            netcond.start_proxy(0, ("127.0.0.1", 1), self.mode)

    def test_a_client_outside_the_allowlist_is_closed_before_forwarding(self):
        blocked, port = netcond.start_proxy(
            0,
            ("127.0.0.1", 1),
            self.mode,
            bind="127.0.0.1",
            allow_clients=["192.0.2.1"],
        )
        self.addCleanup(blocked.close)
        client = socket.create_connection(("127.0.0.1", port), timeout=2)
        self.addCleanup(client.close)
        client.sendall(b"GET /private HTTP/1.1\r\nHost: x\r\n\r\n")
        try:
            body = client.recv(1)
        except ConnectionResetError:
            body = b""
        self.assertEqual(body, b"")

    def test_a_scoped_rate_leaves_other_connections_alone(self):
        """The half that was broken until 2026-08-23: `relay` took `Mode.split`, which discards the
        scope, so a scoped mode really applied to every open connection. Scoping is the whole
        reason a #43 leg can throttle the media stream while the control calls stay fast."""
        self._set(f"rate:{self.KBPS:g}@/library/parts")
        slow_s, slow = self._pull("/library/parts/1/file.mkv")
        fast_s, fast = self._pull("/:/timeline?x=1")
        self.assertEqual(len(slow), self.N)
        self.assertEqual(len(fast), self.N)
        # Both bounds sit on the SLOW side of what they grade: a throttled transfer can only be
        # made slower by a busy machine, and an unthrottled loopback pull measures in milliseconds
        # against a 64 ms allowance. Neither can be tripped by scheduling noise.
        self.assertGreater(slow_s, self.FLOOR_S * 0.8,
                           f"the in-scope connection was not throttled ({slow_s:.3f}s)")
        self.assertLess(fast_s, self.FLOOR_S * 0.5,
                        f"the out-of-scope connection WAS throttled ({fast_s:.3f}s) — the scope is "
                        f"not being honoured per connection")

    def test_a_malformed_mode_passes_rather_than_killing_the_connection(self):
        """The control file is edited by hand mid-experiment; `int()` raising inside a relay thread
        drops a live connection with a traceback that reads like a proxy bug."""
        self.assertIsNone(netcond.arg_of("rate:", "rate"))
        self.assertIsNone(netcond.arg_of("rate:fast", "rate"))
        self.assertIsNone(netcond.arg_of("delay:soon", "delay"))
        self.assertEqual(netcond.arg_of("rate:512", "rate"), 512.0)
        self.assertEqual(netcond.arg_of("delay:250", "delay"), 250.0)
        self.assertIsNone(netcond.arg_of("stall", "rate"))
        self._set("rate:oops")
        _s, body = self._pull()
        self.assertEqual(len(body), self.N)

    def test_the_mode_is_live_under_an_open_transfer(self):
        """One scripted run has to cover four CASE1 legs; four launches is not the same experiment,
        because the app's own state differs between them."""
        self._set(f"rate:{self.KBPS / 4:g}")
        # Released when the first throttled bytes arrive, not on a timer: a timer thread that a
        # loaded host runs late released nothing in time, and the test failed on the scheduler.
        live_s, body = self._pull(on_first_bytes=lambda: self._set("pass"))
        self.assertEqual(len(body), self.N)
        # A quarter of the rate is four times the floor; releasing it has to land well inside that.
        self.assertLess(live_s, self.FLOOR_S * 4,
                        "releasing the mode mid-transfer changed nothing")


# ---------------------------------------------------------------------------
# The ABR observation metrics added by increment I0 of docs/adaptive-playback-plan.md.
#
# Classification (plan §8): every test in this block is a MATHEMATICAL INVARIANT or an
# INTEGRATION test. None of them is a policy-choice test — nothing here asserts that any rung,
# threshold, cooldown or decision is correct, and nothing here may be given a bound taken from a
# run of the code it grades.
# ---------------------------------------------------------------------------
def _sample_line(current=10000, media=9800, net=40000, buf=8000, vbuf=8000,
                 abuf="8200ms", dur=2000, prod=300, n=5, decision="stay", target=0,
                 complete=1):
    # `buf` takes an int OR the literal string "none", exactly as the app emits it: the playable
    # reserve is not knowable on a segment whose audio lane has produced no timestamp since the
    # open or the seek, and the app says so rather than printing a zero that reads as empty.
    buf = f"{buf}ms" if buf != "none" else "none"
    return (f"[  12.345] abr: sample current={current}kbps media={media}kbps net={net}kbps "
            f"buf={buf} vbuf={vbuf}ms abuf={abuf} dur={dur}ms prod={prod}pm n={n} "
            f"decision={decision} target={target}kbps complete={complete} reason=None")


def _stamped(lines, stamps):
    """A `StampedLines` with the arrival times a live run would have recorded."""
    return run.StampedLines(lines, stamps)


class PlaybackClockMetrics(unittest.TestCase):
    """MATHEMATICAL INVARIANT: the metrics compute what their names say, on hand-built input."""


    def test_stalls_come_from_the_media_clock_not_from_the_buffer_model(self):
        """MATHEMATICAL INVARIANT: a stall is `pos=` failing to advance, and nothing else.

        Not the starvation horizon, not `buffered < threshold`, not `starving()`. The series here
        advances, holds for three beats, advances, holds for one: max 3, total 4.
        """
        beats = [1, 2, 3, 3, 3, 3, 4, 5, 5, 6]
        lines = [f"loop=60 fps=60 pos={p}s" for p in beats]
        self.assertEqual(run.playback_stalls(lines), (3, 4, len(beats)))

    def test_a_run_too_short_to_judge_reports_absence(self):
        self.assertEqual(run.playback_stalls(["loop=60 fps=60 pos=1s"]), (None, None, 1))

    def test_lumpiness_sees_what_the_stall_and_rate_metrics_are_both_blind_to(self):
        """MATHEMATICAL INVARIANT: `2,0,2,0` and `1,1,1,1` differ, and only this can tell them apart.

        The two series below cover the same media seconds in the same number of beats, so their
        mean rate is identical and neither contains a stall longer than one beat. One is smooth
        playback and the other is a queue running dry and advancing a whole segment per arrival —
        which is what a viewer sees as judder. Device-observed in `pipe_abr_down_outrun`.
        """
        smooth = [f"loop=60 fps=60 pos={p}s" for p in [10, 11, 12, 13, 14, 15, 16, 17]]
        lumpy = [f"loop=60 fps=60 pos={p}s" for p in [10, 12, 12, 14, 14, 16, 16, 17]]

        # Same span, same beat count: every OTHER instrument scores them alike.
        self.assertEqual(run.playback_stalls(smooth)[0], 0)
        self.assertEqual(run.playback_stalls(lumpy)[0], 1, "no run of held beats exceeds one")

        self.assertEqual(run.playback_lumpiness(smooth), (0, 0, len(smooth)))
        lumpy_beats, longest, beats = run.playback_lumpiness(lumpy)
        self.assertEqual((lumpy_beats, beats), (3, len(lumpy)))
        self.assertEqual(longest, 1, "the lumps alternate with holds, so no two are adjacent")

    def test_a_seek_is_a_relocation_and_not_a_lump(self):
        """A forward jump past `LUMP_SEEK_S` is the clock being MOVED, not the queue running dry.

        Without this the marker/seek cases would each report one phantom lump per seek.
        """
        seek = [f"loop=60 fps=60 pos={p}s" for p in [5, 6, 140, 141, 142]]
        self.assertEqual(run.playback_lumpiness(seek), (0, 0, 5))
        # ...and the boundary is inclusive on the lump side, so a long-segment pack still counts.
        edge = [f"loop=60 fps=60 pos={p}s" for p in [5, 5 + run.LUMP_SEEK_S, 99 + run.LUMP_SEEK_S]]
        self.assertEqual(run.playback_lumpiness(edge)[0], 1)


class StampedLog(unittest.TestCase):
    """MATHEMATICAL INVARIANT: the arrival clock survives the paths the harness actually uses."""

    def test_stamps_track_lines_and_survive_a_snapshot(self):
        log = run.StampedLines()
        log.append("a")
        log.append("b")
        self.assertEqual(len(log.stamps), 2)
        self.assertLessEqual(log.stamps[0], log.stamps[1])
        snap = log.snapshot()
        log.append("c")
        self.assertEqual(list(snap), ["a", "b"], "a snapshot must not follow later appends")
        self.assertEqual(len(snap.stamps), 2)

    def test_it_is_still_an_ordinary_list_to_every_existing_caller(self):
        log = run.StampedLines(["loop=60 fps=60 pos=3s"], [1.0])
        self.assertEqual(run.playpos_secs(log), [(3, "loop=60 fps=60 pos=3s")])
        self.assertEqual(list(log), ["loop=60 fps=60 pos=3s"])


# ---------------------------------------------------------------------------------------------
# The log-line contract between the Rust app and this harness.
#
# These two lines are the ONLY record of what a quality transaction cost and what a segment
# acquisition was made of, and the app truncates its event log every launch -- so a regex that
# silently stops matching does not fail loudly, it reports "no samples" and every derived
# statistic quietly becomes a statement about the empty set.
#
# The test is DIFFERENTIAL, not a golden literal: it reads the format strings out of ff.rs and
# compares the field names and their ORDER against what the regex captures. A frozen example line
# would keep passing after someone renamed a field in Rust, which is the exact failure this is
# here to catch.
# ---------------------------------------------------------------------------------------------

FF_RS = os.path.join(REPO_ROOT, "rust-modules", "media", "src", "ff.rs")
WINDOW_RS = os.path.join(REPO_ROOT, "rust-modules", "media", "src", "abr", "window.rs")

# `name=` in a format string or a regex pattern. Deliberately anchored on the `=`, because that is
# what both sides actually agree on -- the surrounding placeholder/capture syntax differs.
_FIELD = re.compile(r"([a-z][a-z0-9_]*)=")


def _rust_format_string(source, opening):
    """The single Rust string literal starting with `opening`, with `\\`-continuations joined.

    Rust eats the newline AND the next line's leading whitespace after a trailing backslash, so
    the rendered line is what this reconstructs -- not the source layout.
    """
    start = source.index(opening)
    out = []
    i = start + 1                       # step over the opening quote, or the loop ends at once
    while i < len(source):
        ch = source[i]
        if ch == "\\":
            nxt = source[i + 1]
            if nxt == "\n":
                i += 2
                while i < len(source) and source[i] in " \t":
                    i += 1
                continue
            out.append(ch + nxt)
            i += 2
            continue
        if ch == '"':
            break
        out.append(ch)
        i += 1
    return "".join(out)


class LateAudioFixture(unittest.TestCase):
    """The late-audio clip and its case (PR #603): a file whose audio starts 11 s after its video.

    Static only. The clip is only worth anything if it really puts more video than the video lane
    holds ahead of its first audio packet, and the case is only worth anything if it can tell a
    stalled clock and an audio lane that never reached Starfish from a healthy start.
    """

    KEY = "pipe_h264_eac3_late_audio"

    def _packets(self, audio_at, video_every=1 / 24.0, video_size=1000, seconds=14):
        pkts = [{"stream_index": 0, "pts_time": f"{i * video_every:.3f}", "size": str(video_size)}
                for i in range(int(seconds / video_every))]
        return pkts + [{"stream_index": 1, "pts_time": f"{audio_at + i * 0.032:.3f}",
                        "size": "700"} for i in range(20)]

    def test_the_lead_is_the_audio_start_and_the_video_bytes_before_it(self):
        first, ahead = fixturegen.late_audio_lead(self._packets(11.008), 0, 1)
        self.assertAlmostEqual(first, 11.008, places=3)
        # 24 fps, 1000 bytes a frame: frames at t < 11.008 are indices 0..264, i.e. 265 of them.
        self.assertEqual(ahead, 265 * 1000)

    def test_an_on_time_audio_track_has_no_video_ahead_of_it(self):
        first, ahead = fixturegen.late_audio_lead(self._packets(0.0), 0, 1)
        self.assertEqual((first, ahead), (0.0, 0))

    def test_a_file_without_audio_or_a_timestamp_is_refused_not_zeroed(self):
        with self.assertRaises(ValueError):
            fixturegen.late_audio_lead(self._packets(11.0)[:-20], 0, 1)
        broken = self._packets(11.0)
        broken[-1] = {"stream_index": 1, "size": "700"}
        with self.assertRaises(ValueError):
            fixturegen.late_audio_lead(broken, 0, 1)

    def test_the_shape_is_built_to_fill_the_video_lane_before_the_first_audio_packet(self):
        spec = fixturegen.PIPE_SHAPES[self.KEY]
        (audio,) = spec["audio"]
        self.assertAlmostEqual(audio["delay"], 11.008)
        # The design arithmetic, written down where a change to either number trips it: the
        # rate-targeted video must queue more than the lane's cap in the delay it sits behind,
        # even at the verify() band's lower edge (0.70x), or the clip can not park the demuxer.
        video_bytes_by_audio = spec["video"]["vbr"] * 1000 / 8 * audio["delay"] * 0.70
        self.assertGreater(video_bytes_by_audio, fixturegen.AQ_VIDEO_BYTES)
        self.assertEqual(spec["late_audio"]["min_video_bytes_before_audio"],
                         fixturegen.AQ_VIDEO_BYTES)
        self.assertLess(audio["delay"], fixturegen.PIPE_SECS - 20,
                        "the audio must leave the case a long stretch to climb through")

    def test_the_audio_input_is_offset_by_exactly_the_delay(self):
        """`-itsoffset` must precede the delayed track's `-i` and no other input's."""
        captured = []
        with tempfile.TemporaryDirectory() as d, \
                mock.patch.object(fixturegen, "run",
                                  side_effect=lambda argv, quiet=True: captured.append(list(argv)) or ""):
            work = fixturegen.Path(d)
            fixturegen.build_generic(
                self.KEY, fixturegen.PIPE_SHAPES[self.KEY], 4, work / "out.mkv", work)
        argv = captured[0]
        self.assertEqual(argv.count("-itsoffset"), 1, "only the delayed track is offset")
        at = argv.index("-itsoffset")
        self.assertEqual(argv[at + 1], "11.008")
        self.assertEqual(argv[at + 2:at + 5], ["-t", "4", "-f"], "it must sit right before the audio -i")
        self.assertIn("sine=", argv[at + 7], "and the input it offsets is the audio graph")

    def test_the_case_grades_the_stalled_clock_and_the_unfed_audio_lane(self):
        case = next(c for c in _manifest()["pipeline_cases"] if c["name"] == "pipe_late_audio_eac3")
        spec = fixturegen.PIPE_SHAPES[self.KEY]
        self.assertEqual(case["fixture"], self.KEY + ".mkv")
        for k in ("vcodec", "acodec", "fps"):
            self.assertEqual(case["declare"][k], spec["declare"][k], f"declare.{k} drifted from the shape")
        exp = case["expect"]
        # The two stalls are separate failures: a stalled clock and an audio lane that never
        # reached Starfish. A case that graded only the first would pass a fix for stall 1 alone.
        self.assertTrue(exp["require_audio_feed_ready"])
        self.assertGreaterEqual(exp["min_pos_climb_s"], 12)
        self.assertEqual(exp["audio_stream_index"], 1)
        self.assertEqual(exp["load_audio"], "AC3 PLUS")
        self.assertLess(exp["min_pos_climb_s"] + 5, case["run_secs"])
        self.assertLess(case["run_secs"], fixturegen.PIPE_SECS, "the clip must not end in the window")

    def test_a_stalled_clock_and_a_silent_audio_lane_both_fail_the_case_assertions(self):
        # What the broken player logs: heartbeat position pinned at 0, video fed, audio never.
        stalled = [f"loop=100 fps=60 pos=0s stage=Playing" for _ in range(6)]
        self.assertFalse(run.a_timeline_climb(stalled, 15)[0])
        climbing = [f"loop=100 fps=60 pos={t}s stage=Playing" for t in range(0, 20)]
        self.assertTrue(run.a_timeline_climb(climbing, 15)[0])
        self.assertFalse(run.a_audio_feed_ready(["feed v#1 sz=9 fed=0 reply=O qbytes=1"])[0])


def abr_shape_keys():
    """The bounds `a_abr_shape` actually implements, read from its SOURCE.

    This was a hand-written set of five, and it omitted the three metrics the function has read
    since increment I0 (`min_buf_ms`, `max_stall_s`, `raster_changes_max`) -- so a case using one
    of them would have been rejected as an unknown key even though the grader honours it.

    Deriving it from the implementation is the same trick as the log-line contract above, and for
    the same reason: a restated list drifts silently, and both directions of the drift are bad. An
    unlisted-but-implemented key blocks a legitimate bound; a listed-but-unimplemented key lets a
    case declare a bound that nothing ever reads, which passes forever.
    """
    src = inspect.getsource(run.a_abr_shape)
    return set(re.findall(r'spec\.get\("([a-z_]+)"', src))


class EarlyExitSoundness(unittest.TestCase):
    """Early exit is sound only for MONOTONE assertions. These are the exceptions.

    The failure it guards is a false PASS, not a false fail: `max_commits` counts events over
    whatever window it was given, so a run that stops early scores LOWER and a regression that
    adds rung changes can slip under the bound. Observed on the real matrix -- 7 changes on a full
    window, passing a 5-bound early, 8 on a later full window, same binary.
    """


    def test_gst_trace_never_grades_early(self):
        allowed, _ = run.early_exit_allowed({"expect": {}, "gst_trace": True}, {})
        self.assertFalse(allowed)


    def test_an_undelayed_operation_still_exits_early(self):
        """The rule is about SCHEDULING, not about seeks: an ordinary seek fires promptly."""
        case = {"expect": {"no_error": True},
                "operations": [{"op": "play"}, {"op": "seek", "mode": "inplace", "target_s": 40}]}
        allowed, _ = run.early_exit_allowed(case, {})
        self.assertTrue(allowed, "an operation with no delay_ms is not window-length sensitive")

    def test_an_ordinary_case_still_exits_early(self):
        allowed, why = run.early_exit_allowed({"expect": {"codec": "h264", "no_error": True}}, {})
        self.assertTrue(allowed, "early exit is the default and is what keeps the suite quick")
        self.assertEqual(why, "")

    def test_no_early_needs_no_explanation(self):
        allowed, why = run.early_exit_allowed({"expect": {}}, {"no_early": True})
        self.assertFalse(allowed)
        self.assertEqual(why, "", "the operator asked for it; printing a reason is noise")


class ThePlaybackRateIsTheOnlyThingThatSeesASlowFilm(unittest.TestCase):
    """The reserve metrics are blind to a film running slow, and this is the differential.

    `min_buf_ms`, `max_stall_s` and `slope` are all measured against the PLAYHEAD. When the
    playhead itself slows down, the reserve stops draining -- so every one of them reads healthy
    at exactly the moment the picture is worst. Measured on the corpus 2026-08-27:
    `pipe_abr_band_20000` sits at 670 per mille of real time for ~30 s with `buf` parked at
    2.2 s and `slope` decaying to -29 ms/s, and no bound in the harness could see it.
    """

    @staticmethod
    def beats(positions):
        """A heartbeat per wall second, carrying `pos=` in media seconds."""
        return [f"loop=60 route=player overlay=none pos={p}s vtick=5 vgap=201ms fps=60"
                for p in positions]

    def test_real_time_playback_reads_one_thousand(self):
        mean, worst, beats, legs = run.playback_rate(self.beats(range(30)))
        self.assertEqual(mean, 1000)
        self.assertEqual(worst, 1000)
        self.assertEqual((beats, legs), (30, 1))

    def test_a_slow_leg_is_found_even_when_the_mean_is_perfect(self):
        """The shape actually observed: crawl, then replay fast to catch up.

        A mean would score this 1000 and report nothing at all. The worst window is the assertion
        because the person watching saw thirty seconds of a broken film, not an average.
        """
        pos = list(range(0, 20))                      # 20 s at speed
        pos += [20 + (i * 2) // 3 for i in range(30)]  # 30 s at two thirds
        pos += [pos[-1] + 2 * i for i in range(1, 16)]  # catch up at 2x
        mean, worst, _b, _l = run.playback_rate(pos and self.beats(pos))
        self.assertLessEqual(worst, 700, "the 0.67x leg has to be visible")
        self.assertGreaterEqual(mean, 950, "…and the mean is exactly what hides it")

    def test_a_seek_is_not_a_rate(self):
        """A discontinuity splits legs; it never becomes a 40x window or a negative one."""
        forward = self.beats(list(range(20)) + list(range(300, 320)))
        _mean, worst, _b, legs = run.playback_rate(forward)
        self.assertEqual(legs, 2)
        self.assertEqual(worst, 1000, "the jump must not be measured as playback")
        back = self.beats(list(range(300, 320)) + list(range(20)))
        _mean, worst, _b, legs = run.playback_rate(back)
        self.assertEqual((legs, worst), (2, 1000))

    def test_too_short_a_series_says_so_rather_than_guessing(self):
        mean, worst, beats, legs = run.playback_rate(self.beats(range(4)))
        self.assertEqual((mean, worst, legs), (None, None, 0))
        self.assertEqual(beats, 4)


    def test_the_rate_rides_along_on_every_case_that_has_a_dense_series(self):
        """Reported without being asserted, so a slow film is legible in a log nobody bounded."""
        _ok, why = run.a_timeline_climb(self.beats([(i * 2) // 3 for i in range(40)]), 10)
        self.assertIn("pm of real time", why)
        self.assertIn("worst 10s window", why)


class PresentedFps(unittest.TestCase):
    """The sink's displayed-frame counter (`smp_cb type=47`) read as a rate, both shapes."""

    def test_cumulative_counter_with_stamps_yields_the_stream_rate_and_the_worst_window(self):
        lines = [f"{1000 + 200 * i}  smp_cb type=47 num={int(24 * 0.2 * i)} str=" for i in range(26)]
        lines.insert(5, "2000  smp_cb type=46 num=3 str=")
        mean, worst, n, dropped, monotone = run.presented_fps(lines)
        self.assertEqual((mean, n, dropped, monotone), (24.0, 26, 3, True))
        self.assertLessEqual(worst, 24.0)

    def test_per_interval_counter_without_stamps_assumes_the_200ms_poll(self):
        lines = ["smp_cb type=47 num=5 str=", "smp_cb type=47 num=4 str=",
                 "smp_cb type=47 num=5 str=", "smp_cb type=47 num=5 str="]
        mean, worst, n, dropped, monotone = run.presented_fps(lines)
        self.assertFalse(monotone)
        self.assertEqual((worst, n, dropped), (20.0, 4, 0))
        self.assertAlmostEqual(mean, 23.3, places=1)

    def test_a_flat_per_interval_series_is_not_a_cumulative_zero(self):
        lines = [f"{1000 + 200 * i}  smp_cb type=47 num=5 str=" for i in range(26)]
        mean, worst, n, dropped, cumulative = run.presented_fps(lines)
        self.assertFalse(cumulative)
        self.assertEqual((mean, worst), (25.0, 25.0))

    def test_the_apps_normalised_sink_line_is_read_before_the_raw_type(self):
        # a webOS 5+ set logs the same counter as raw type 49; the app's `sink:` line is what
        # the harness reads, and the raw 47 on such a set (something else) is ignored
        lines = [f"{1000 + 200 * i}  smp_cb type=49 num=5 str=" for i in range(26)]
        self.assertIsNone(run.presented_fps(lines))
        lines = [ln for i in range(26) for ln in
                 (f"{1000 + 200 * i}  smp_cb type=49 num=5 str=", f"{1000 + 200 * i}  sink: displayed=5 (type=49)")]
        lines.append("2000  smp_cb type=47 num=999 str=")
        mean, worst, n, dropped, cumulative = run.presented_fps(lines)
        self.assertEqual((mean, n), (25.0, 26))

    def test_a_cumulative_counter_through_a_pause_is_still_cumulative(self):
        vals = [int(24 * 0.2 * i) for i in range(30)] + [int(24 * 0.2 * 29)] * 40
        intervals, cumulative = run.sink_counter_intervals(vals)
        self.assertTrue(cumulative)
        self.assertEqual(intervals[-1], 0)

    def test_a_cumulative_counter_that_resets_on_a_second_load_is_split_at_the_reset(self):
        vals = [int(24 * 0.2 * i) for i in range(26)] + [int(24 * 0.2 * i) for i in range(26)]
        lines = [f"{1000 + 200 * i}  smp_cb type=47 num={v} str=" for i, v in enumerate(vals)]
        mean, worst, n, dropped, cumulative = run.presented_fps(lines)
        self.assertTrue(cumulative)
        self.assertAlmostEqual(mean, 24.0, delta=1.0)
        # the reset step reads as "frames since the reset", not as a huge negative delta
        self.assertGreaterEqual(worst, 0)

    def test_no_counter_is_none_not_zero(self):
        self.assertIsNone(run.presented_fps(["smp_cb type=0 num=1 str=", "loop=60 fps=60"]))


class PresentedRate(unittest.TestCase):
    """`presented_rate`: the sink counter against the declared rate, on synthetic logs shaped like
    the device's (one heartbeat per second, five type-47 samples between beats)."""

    LOAD = 'load: v=H264 a="AC3" fps=24.000 dv=present:0 P0/0 el:0 atmos:0 max=3840x2160@24'

    def _log(self, per_poll, secs=12, play=1000, pos_step=1, declared=LOAD):
        lines = [declared]
        pos = 0
        for s in range(secs):
            lines.append(f"loop=60 route=player overlay=none pos={pos}s play={play}pm vtick=5 vgap=201ms fps=60")
            pos += pos_step
            for _ in range(5):
                lines.append(f"smp_cb type=47 num={per_poll} str=")
        return lines

    def test_a_24p_stream_shown_at_24_passes(self):
        ok, why = run.a_presented_rate(self._log(5), {})
        self.assertTrue(ok, why)
        self.assertIn("median 25.0", why)

    def test_the_30_lattice_case_shown_at_13_fails(self):
        # the 2026-09-03 measurement: ~13 fps presented, media clock at real time
        lines = self._log(2, secs=6) + self._log(3, secs=6)[1:]
        ok, why = run.a_presented_rate(lines, {})
        self.assertFalse(ok)
        self.assertIn("outside", why)

    def test_paused_and_seeking_seconds_are_not_windows(self):
        # paused: position flat, nothing shown — must not count against the rate
        paused = self._log(0, secs=8, pos_step=0)[1:]
        ok, why = run.a_presented_rate(self._log(5, secs=6) + paused, {})
        self.assertTrue(ok, why)
        self.assertIn("over 5 healthy s", why)

    def test_a_transcode_with_no_declared_rate_is_skipped_not_failed(self):
        ok, why = run.a_presented_rate(self._log(5, declared=self.LOAD.replace("24.000", "0.000")), {})
        self.assertTrue(ok)
        self.assertIn("skipped", why)

    def test_a_missing_counter_is_a_loud_failure(self):
        lines = [ln for ln in self._log(5) if "type=47" not in ln]
        ok, why = run.a_presented_rate(lines, {})
        self.assertFalse(ok)
        self.assertIn("type=47", why)

    def test_a_cumulative_counter_with_a_long_pause_grades_only_the_moving_seconds(self):
        # six healthy seconds of a cumulative 24 fps counter, then eight seconds paused (flat)
        lines, n, pos = [self.LOAD], 0, 0
        for s in range(6):
            lines.append(f"loop=60 route=player pos={pos}s play=1000pm fps=60"); pos += 1
            for _ in range(5):
                n += 5
                lines.append(f"sink: displayed={n} (type=47)")
        for s in range(8):
            lines.append(f"loop=60 route=player pos={pos}s play=0pm fps=60")
            for _ in range(5):
                lines.append(f"sink: displayed={n} (type=47)")
        ok, why = run.a_presented_rate(lines, {})
        self.assertTrue(ok, why)
        self.assertIn("over 5 healthy s", why)

    def test_opt_out_and_a_cumulative_counter(self):
        self.assertTrue(run.a_presented_rate([], {"presented_rate": False})[0])
        lines, n = [self.LOAD], 0
        for s in range(12):
            lines.append(f"loop=60 route=player pos={s}s play=1000pm fps=60")
            for _ in range(5):
                n += 5
                lines.append(f"smp_cb type=47 num={n} str=")
        ok, why = run.a_presented_rate(lines, {})
        self.assertTrue(ok, why)
        # …and the same counter reset by a second Load half-way through still grades 24
        lines2, n = [self.LOAD], 0
        for s in range(14):
            lines2.append(f"loop=60 route=player pos={s}s play=1000pm fps=60")
            if s == 7:
                n = 0
            for _ in range(5):
                n += 5
                lines2.append(f"smp_cb type=47 num={n} str=")
        ok, why = run.a_presented_rate(lines2, {})
        self.assertTrue(ok, why)
        self.assertIn("median 25.0", why)

class DepGates(unittest.TestCase):
    """Restructure spec §15.2: `ci/check-deps.sh` is green, and every allowlist under ci/allow/
    declares the count it actually has — so an allowlist grows only by editing both lines, and a
    review sees the number move."""

    ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

    # Every path `ci/check-deps.sh` reads (it `cd`s to its own `..` first): the Rust sources it
    # greps, its allowlists and helper, and the build configuration its `fpflags` rule scans.
    # A self-test plants or mutates a file in a private copy of exactly these, never in the
    # checkout, so the self-tests are independent of one another (they run in parallel, see
    # `load_tests` at the bottom) and a failing or interrupted one cannot leave the working tree
    # edited. `test_the_private_copy_grades_exactly_like_the_real_tree` (the grep inputs) and
    # `test_the_private_copy_carries_every_fpflags_input` (the build configuration) fail if this
    # list falls behind the script.
    TREE_INPUTS = (
        "ci",
        "rust-modules/src",
        # The layer crates split out of `src`: `ci/check-deps.sh` reads them as `SRC_BASE`,
        # `SRC_MACHINE`, `SRC_NET`, `SRC_PLATFORM`, `SRC_GFX`, `SRC_UI`, `SRC_PLEX`, `SRC_TELEMETRY`, `SRC_DATA`, `SRC_SESSION` and `SRC_MEDIA`, so a copy without them grades a different
        # tree than the checkout.
        "rust-modules/base/src",
        "rust-modules/machine/src",
        "rust-modules/net/src",
        "rust-modules/platform/src",
        "rust-modules/gfx/src",
        "rust-modules/ui/src",
        "rust-modules/telemetry/src",
        "rust-modules/data/src",
        "rust-modules/media/src",
        "rust-modules/appkit/src",
        "rust-modules/screens/src",
        "rust-modules/Cargo.toml",
        "rust-modules/build.rs",
        "rust-modules/net/Cargo.toml",
        "rust-modules/telemetry/Cargo.toml",
        "rust-modules/data/Cargo.toml",
        "rust-modules/media/Cargo.toml",
        "rust-modules/appkit/Cargo.toml",
        "rust-modules/appkit/build.rs",
        "rust-modules/screens/Cargo.toml",
        "rust-modules/screens/build.rs",
        "rust-modules/platform/Cargo.toml",
        "rust-modules/platform/build.rs",
        "rust-modules/gfx/Cargo.toml",
        "rust-modules/gfx/build.rs",
        "rust-modules/ui/Cargo.toml",
        "rust-modules/ui/build.rs",
        "rust-modules/storage/Cargo.toml",
        "rust-modules/storage/build.rs",
        "rust-modules/.cargo",
        "Makefile",
    )

    @functools.cached_property
    def tree(self):
        """The private copy of the gate's inputs; removed when the test ends."""
        root = tempfile.mkdtemp(prefix="_check_deps_tree_")
        self.addCleanup(shutil.rmtree, root, True)
        for rel in self.TREE_INPUTS:
            source = os.path.join(self.ROOT, rel)
            if not os.path.exists(source):
                continue
            target = os.path.join(root, rel)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            if os.path.isdir(source):
                shutil.copytree(source, target, ignore=shutil.ignore_patterns("__pycache__"))
            else:
                shutil.copy2(source, target)
        return root

    def test_check_deps_is_green(self):
        r = subprocess.run([os.path.join(self.ROOT, "ci", "check-deps.sh")], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_self_tests_never_touch_the_real_tree(self):
        """Red-first for the parallel gate self-tests: every planting helper below edits a private
        copy of the gate's inputs, never the checkout. A helper that wrote into the checkout
        bumped the mtime of a file `cargo` watches, so the NEXT cargo invocation of a `make check`
        recompiled the whole crate for nothing -- and made it impossible to run two self-tests at
        once."""
        real = os.path.join(self.ROOT, "rust-modules", "data", "src", "metadata.rs")
        before = os.stat(real).st_mtime_ns
        self.assertEqual(self._prepend("metadata.rs", "\n").returncode, 0)
        self.assertEqual(os.stat(real).st_mtime_ns, before, "a self-test wrote into the real tree")

    def test_the_private_copy_grades_exactly_like_the_real_tree(self):
        """The copy must carry every input `ci/check-deps.sh` reads: a missing one makes a gate see
        nothing and pass (or fail) for a reason the real tree would not share."""
        real = subprocess.run([os.path.join(self.ROOT, "ci", "check-deps.sh")], capture_output=True, text=True)
        copy = subprocess.run([os.path.join(self.tree, "ci", "check-deps.sh")], capture_output=True, text=True)
        self.assertEqual((copy.returncode, copy.stdout), (real.returncode, real.stdout), copy.stderr)

    def test_the_private_copy_carries_every_fpflags_input(self):
        """The untouched copy above cannot see an input that was left out of `TREE_INPUTS` -- a
        gate that scans nothing is green. So plant what the `fpflags` rule looks for in each file
        it scans, inside the copy, and require the rule to go red naming it.

        All of them are planted at once and graded by ONE run: the rule prints one `path:line:text`
        match per offending file, so a file the rule did not scan (or the copy did not carry)
        is missing from the output and that input's subtest fails by name. Run one file at a
        time this was 17 of the 73 `ci/check-deps.sh` runs in this class (about a quarter of
        the module's CPU)."""
        inputs = ("rust-modules/Cargo.toml", "rust-modules/build.rs", "rust-modules/net/Cargo.toml", "rust-modules/telemetry/Cargo.toml", "rust-modules/data/Cargo.toml", "rust-modules/media/Cargo.toml", "rust-modules/appkit/Cargo.toml", "rust-modules/appkit/build.rs", "rust-modules/screens/Cargo.toml", "rust-modules/screens/build.rs", "rust-modules/platform/Cargo.toml",
                  "rust-modules/platform/build.rs", "rust-modules/gfx/Cargo.toml",
                  "rust-modules/gfx/build.rs", "rust-modules/ui/Cargo.toml", "rust-modules/ui/build.rs",
                  "rust-modules/storage/Cargo.toml",
                  "rust-modules/storage/build.rs", "rust-modules/.cargo/config.toml", "Makefile")
        for rel in inputs:
            target = os.path.join(self.tree, rel)
            self.assertTrue(os.path.exists(target), f"{rel} is not in the private copy (TREE_INPUTS)")
            with open(target, "a", encoding="utf-8") as f:
                f.write("\nrustflags = [\"-C\", \"target-feature=+fma\"]\n")
        r = subprocess.run([os.path.join(self.tree, "ci", "check-deps.sh")],
                           capture_output=True, text=True)
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("fpflags:", out)
        reported = {line.split(":", 1)[0] for line in out.splitlines() if "target-feature=+fma" in line}
        for rel in inputs:
            with self.subTest(input=rel):
                self.assertIn(rel, reported, out)

    def _plant(self, name, content, crate_dir=None):
        """Write `content` to a temp file under rust-modules/src that no ci/allow/*.txt names,
        run the private copy's ci/check-deps.sh with it present, and guarantee removal even if
        the assertion that follows fails. An orphan .rs file with no `mod` statement pointing at
        it is invisible to cargo (nothing declares it part of the crate) but not to `find … -name
        '*.rs'`, which is all these gates scan with — so this is the cheapest way to prove a gate
        catches a shape without touching a real, permanent source file."""
        # `player/` and `route/` are the media layer crate's now (docs/module-layers.md, Split 11).
        crate = crate_dir or ("media" if name.split("/")[0] in ("player", "route") else None)
        target = os.path.join(self.tree, "rust-modules", *([crate] if crate else []), "src", name)
        self.assertFalse(os.path.exists(target), f"stale self-test artifact at {target} — remove it by hand")
        try:
            with open(target, "w", encoding="utf-8") as f:
                f.write(content)
            return subprocess.run([os.path.join(self.tree, "ci", "check-deps.sh")],
                                   capture_output=True, text=True)
        finally:
            if os.path.exists(target):
                os.remove(target)

    def _store_file(self, relpath):
        """The private copy's path of a data-layer module: the data layer's crate (`plx_data`,
        Split 9) holds metadata.rs; any other relative path is the application crate's. A path that exists in neither fails the caller's
        open(), loudly."""
        data = os.path.join(self.tree, "rust-modules", "data", "src", relpath)
        return data if os.path.exists(data) else os.path.join(self.tree, "rust-modules", "src", relpath)

    def _prepend(self, relpath, content):
        """Temporarily prepend a valid module-level fixture to a scanned Rust file."""
        target = self._store_file(relpath)
        with open(target, encoding="utf-8") as f:
            original = f.read()
        try:
            with open(target, "w", encoding="utf-8") as f:
                f.write(content + original)
            return subprocess.run([os.path.join(self.tree, "ci", "check-deps.sh")],
                                  capture_output=True, text=True)
        finally:
            with open(target, "w", encoding="utf-8") as f:
                f.write(original)

    def test_test_module_discovery_lexer_and_graph_regressions(self):
        result = subprocess.run([sys.executable, os.path.join(self.ROOT, "ci", "test_rust_test_modules.py")],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def _module_path_fixture(self, production_reference):
        source = os.path.join(self.tree, "rust-modules", "src")
        with tempfile.TemporaryDirectory(prefix="_check_deps_cfg_", dir=source) as directory:
            declaration = '#[cfg(test)]\n#[allow(dead_code)]\n#[path = "support.rs"]\npub(crate) mod checks;\n'
            if production_reference:
                declaration += '#[path = "support.rs"] mod production;\n'
            with open(os.path.join(directory, "entry.rs"), "w", encoding="utf-8") as output:
                output.write(declaration)
            target = os.path.join(directory, "support.rs")
            with open(target, "w", encoding="utf-8") as output:
                output.write('pub fn helper() { std::thread::spawn(|| {}); let _ = 0.5f32.powf(2.4); }\n')
            result = subprocess.run([os.path.join(self.tree, "ci", "check-deps.sh")],
                                    capture_output=True, text=True)
            return result, os.path.relpath(target, self.tree)

    def test_cfg_test_path_modules_are_excluded_from_production_gates(self):
        result, target = self._module_path_fixture(False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn(target + ":", result.stdout)

    def test_production_path_reference_prevents_test_file_exemption(self):
        result, target = self._module_path_fixture(True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("threads:", result.stdout)
        self.assertIn("libm:", result.stdout)
        self.assertIn(target + ":", result.stdout)

    def test_threads_gate_catches_a_bare_thread_spawn_after_use_std_thread(self):
        """The gate used to match only the fully-qualified `std::thread::spawn(` spelling, so a
        file that does `use std::thread;` and then calls the bare `thread::spawn(` — exactly the
        shape the phase-12 gates package's own brief named as invisible — passed silently. RED:
        planting that shape in a file ci/allow/threads.txt does not name must fail `threads`."""
        r = self._plant(
            "_check_deps_selftest_threads.rs",
            "use std::thread;\n\npub fn spawn_worker() {\n    thread::spawn(|| {});\n}\n",
        )
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("threads:", out)
        self.assertIn("_check_deps_selftest_threads.rs", out)

    def test_blocking_gate_catches_a_new_allow_blocking_site(self):
        """A frame-thread blocking exception is a ledger entry in ci/allow/blocking.txt. RED: a new
        production `allow_blocking(` that the list does not name must fail `blocking`, naming the
        file and the enclosing fn."""
        body = ("pub fn sneaky() {\n    let _b = plx_base::task::allow_blocking("
                "const { &plx_base::task::BlockingLabel::new(\"x\") });\n}\n")
        r = self._plant("_check_deps_selftest_blocking.rs", body)
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("blocking:", out)
        self.assertIn("_check_deps_selftest_blocking.rs::sneaky", out)

    def test_blocking_gate_ignores_tests_comments_and_declarations(self):
        """GREEN: the same call inside a `#[cfg(test)] mod`, in a comment, or as an `fn` declaration
        is not a call site."""
        r = self._plant(
            "_check_deps_selftest_blocking_ok.rs",
            "// allow_blocking(label) is documented here\n"
            "pub fn allow_blocking(label: &str) {}\n"
            "#[cfg(test)]\nmod tests {\n    fn t() { let _b = plx_base::task::allow_blocking(L); }\n}\n",
        )
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("ok — blocking", r.stdout)

    def test_blocking_gate_catches_an_aliased_allow_blocking(self):
        """The call-site grep cannot see `use ...::allow_blocking as x; x(..)` or `let f =
        allow_blocking;`. RED: a production mention of the bare identifier that is not a call, the
        `fn` definition or the listed re-export must fail `blocking`."""
        body = ("use plx_base::task::allow_blocking as sneak;\n\npub fn sneaky() {\n"
                "    let _f = sneak;\n}\n")
        r = self._plant("_check_deps_selftest_blocking_alias.rs", body)
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("blocking:", out)
        self.assertIn("_check_deps_selftest_blocking_alias.rs:1", out)

    def test_blocking_gate_catches_a_stale_entry(self):
        """The ledger only shrinks: an entry whose site is gone must fail `blocking`, so a migration
        has to delete its line (and a line cannot be left behind to excuse a later regrowth)."""
        allow = os.path.join(self.tree, "ci", "allow", "blocking.txt")
        with open(allow, encoding="utf-8") as f:
            original = f.read()
        try:
            declared = int(original.splitlines()[0].split(":")[1])
            stale = original.replace(f"# count: {declared}", f"# count: {declared + 1}", 1)
            stale += "rust-modules/media/src/route/decision.rs::no_such_fn\tgone\n"
            with open(allow, "w", encoding="utf-8") as f:
                f.write(stale)
            r = subprocess.run([os.path.join(self.tree, "ci", "check-deps.sh")], capture_output=True, text=True)
        finally:
            with open(allow, "w", encoding="utf-8") as f:
                f.write(original)
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("blocking:", out)
        self.assertIn("decision.rs::no_such_fn", out)

    def test_tmppath_gate_catches_a_path_built_on_one_line_and_opened_on_the_next(self):
        """The gate used to require the literal and a filesystem-open verb on the SAME line, so a
        value built on one line and opened on the next passed silently. RED: planting that split
        in a file dev.rs does not own must fail `tmppath`."""
        r = self._plant(
            "_check_deps_selftest_tmppath_open.rs",
            'pub fn open_it() {\n    let p = "/tmp/dispatcharrtv-selftest";\n'
            "    let _ = std::fs::File::open(p);\n}\n",
        )
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("tmppath:", out)
        self.assertIn("_check_deps_selftest_tmppath_open.rs", out)

    def test_tmppath_gate_exempts_a_log_message_mention(self):
        """D4's own exemption: a `/tmp/dispatcharrtv-` literal that is only message text passed to
        `log`/`crate::eventlog::log`/`log!` must not fail the gate. GREEN: planting one, including a nested
        `format!` the way most real call sites spell it, must leave `tmppath` (and the whole
        script) green."""
        r = self._plant(
            "_check_deps_selftest_tmppath_log.rs",
            'pub fn mention_it(n: u32) {\n    crate::eventlog::log(&format!(\n'
            '        "selftest: see /tmp/dispatcharrtv-selftest ({n})"\n    ));\n}\n',
        )
        out = r.stdout + r.stderr
        self.assertEqual(r.returncode, 0, out)
        self.assertIn("ok — tmppath", out)

    def test_cards_gate_catches_a_screen_assembling_its_own_strip(self):
        """Shared-card-sections L1: a screen draws card sections with `plx_ui::cards::{Shelf, Grid}`.
        `card_row` / `poster_grid` are `pub(crate)`, so the gate polices what `plx_ui::cards`
        re-exports. RED: a file in `screens/` outside ci/allow/cards.txt that calls a tile painter
        through the re-export surface (qualified, or imported by name), names an L0 type, or names
        `AvatarRow` outside the profile picker must fail `cards`."""
        r = self._plant(
            "_check_deps_selftest_cards_strip.rs",
            "pub fn draw_row() {\n    plx_ui::cards::paint_visible(p, rect, 1.0, true);\n}\n"
            "use plx_ui::cards::{paint_visible, RowStyle};\n"
            "pub struct Mine(CardRow);\n"
            "pub struct Pop(GridPop);\n"
            "pub struct Bands(GridBands);\n"
            "pub fn scale() -> f32 { press::scale() }\n"
            "pub struct Avatars(plx_ui::cards::AvatarRow);\n",
            crate_dir="screens",
        )
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("cards:", out)
        for line in (2, 4, 5, 6, 7, 8, 9):
            self.assertIn(f"_check_deps_selftest_cards_strip.rs:{line}", out)

    def test_cards_gate_allows_the_shared_component(self):
        r = self._plant(
            "_check_deps_selftest_cards_ok.rs",
            "pub struct Mine(plx_ui::cards::Shelf);\n",
            crate_dir="screens",
        )
        out = r.stdout + r.stderr
        self.assertEqual(r.returncode, 0, out)
        self.assertIn("ok — cards", out)

    def test_sink_gate_catches_the_video_sink_named_outside_the_player(self):
        """Step L15: the Starfish/ACB verbs are the player's alone. RED: a module outside
        `player/`, `port.rs` and `tv/` that reaches `tv::sink::installed()` must fail `sink`."""
        r = self._plant(
            "_check_deps_selftest_sink_out.rs",
            "pub fn pause_it() {\n    let _ = plx_platform::tv::sink::installed();\n}\n",
        )
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("sink:", out)
        self.assertIn("_check_deps_selftest_sink_out.rs", out)

    def test_sink_gate_allows_the_player_to_name_the_video_sink(self):
        """GREEN: the same call from a file under `player/` leaves `sink` (and the script) green."""
        r = self._plant(
            "player/_check_deps_selftest_sink_in.rs",
            "pub fn pause_it() {\n    let _ = plx_platform::tv::sink::installed();\n}\n",
        )
        out = r.stdout + r.stderr
        self.assertEqual(r.returncode, 0, out)
        self.assertIn("ok — sink", out)

    def test_textmeasure_gate_catches_a_bare_call_outside_the_seam(self):
        """Phase 12, D4: `textmeasure` went from an allowlist to zero-tolerance. RED: a raw
        `crate::text::text_width(` call in a file that is neither one of the three seam files
        (`text.rs`, `ui/text_view.rs`, `ui/text_buffer.rs`) nor inside an `impl … Measure for …`
        block must fail — a screen reaching for the raw function instead of threading a `Measure`
        capability down is exactly the shape D1 spent this phase eliminating."""
        r = self._plant(
            "_check_deps_selftest_textmeasure_bare.rs",
            'pub fn label_width(s: &std::ffi::CStr) -> f32 {\n'
            "    crate::text::text_width(s.as_ptr(), 24, 0)\n}\n",
        )
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("textmeasure:", out)
        self.assertIn("_check_deps_selftest_textmeasure_bare.rs", out)

    def test_textmeasure_gate_exempts_the_body_of_a_measure_impl(self):
        """The exemption is STRUCTURAL, not a path allowlist: any `impl … Measure for …` block,
        anywhere in the tree, may call the raw functions it wraps — `widgets.rs`'s `LegacyMeasure`
        and `login.rs`'s `RawTextMeasure` are today's two, but a third added later must not need
        its file added to a list. GREEN: the identical call from the RED case above, moved inside
        such a block in a brand-new file, must leave `textmeasure` green."""
        r = self._plant(
            "_check_deps_selftest_textmeasure_impl.rs",
            "struct SelftestMeasure;\n\n"
            "impl plx_machine::machine::Measure for SelftestMeasure {\n"
            "    fn width(&self, s: &std::ffi::CStr, sz: i32, bold: bool) -> f32 {\n"
            "        crate::text::text_width(s.as_ptr(), sz, bold as i32)\n"
            "    }\n"
            "}\n",
        )
        out = r.stdout + r.stderr
        self.assertEqual(r.returncode, 0, out)
        self.assertIn("ok — textmeasure", out)

    def test_dt_gate_catches_a_raw_accumulator(self):
        """Phase 12, D4: `dt` went from a 12-file allowlist to zero-tolerance everywhere but
        `machine/src/motion.rs`. RED: a `self.t += dt;`-shaped accumulator in a new file must fail — this
        is the exact pattern (a clock-driven animator summing a raw per-frame delta instead of
        reading `Tick.ms` through `motion::Ramp`/`motion::Phase`) the frozen-animator regression
        class comes from."""
        r = self._plant(
            "_check_deps_selftest_dt_accum.rs",
            "pub struct Ramp { t: f32 }\n\nimpl Ramp {\n"
            "    pub fn tick(&mut self, dt: f32) {\n        self.t += dt;\n    }\n}\n",
        )
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("dt:", out)
        self.assertIn("_check_deps_selftest_dt_accum.rs", out)

    def test_dt_gate_catches_idle_dt_by_name(self):
        """`idle::dt()` is deleted from `machine/src/idle.rs` — the accessor callers used to sum themselves.
        RED: a call spelled `idle::dt()` anywhere must fail even with no `+=`/`-=` beside it, since
        the function no longer exists to call."""
        r = self._plant(
            "_check_deps_selftest_dt_fn.rs",
            "pub fn read_it() -> f32 {\n    plx_machine::idle::dt()\n}\n",
        )
        out = r.stdout + r.stderr
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("dt:", out)
        self.assertIn("_check_deps_selftest_dt_fn.rs", out)

    def test_check_statics_is_green(self):
        """Spec §0 done-criterion 1: `ci/check-statics.sh` — every `static mut` under ui/ and screens/
        is a named render cache (ci/allow/statics.txt) or sits in a legacy module still awaiting its
        phase (ci/allow/statics-migration.txt), and no allowlist entry is stale."""
        r = subprocess.run([os.path.join(self.ROOT, "ci", "check-statics.sh")], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_every_allowlist_declares_its_own_count(self):
        allow = os.path.join(self.ROOT, "ci", "allow")
        seen = 0
        for fn in sorted(os.listdir(allow)):
            with open(os.path.join(allow, fn), encoding="utf-8") as f:
                lines = f.read().splitlines()
            declared = int(lines[0].split(":")[1])
            entries = [l for l in lines if l.strip() and not l.startswith("#")]
            self.assertEqual(declared, len(entries), fn)
            for e in entries:
                # `<path>::<fn>` keys (blocking.txt, store-seams.txt) name a file and a symbol in it
                self.assertTrue(os.path.exists(os.path.join(self.ROOT, e.split("\t")[0].split("::")[0])), e)
            seen += 1
        self.assertGreaterEqual(seen, 3)

    # Phase 12 (P8-H): a per-file PIN, on top of the self-consistency check above. That check
    # only proves a file's own `# count: N` line matches its own entry count — it cannot see an
    # allowlist and its count grow TOGETHER, correctly, in the same edit, past the number this
    # phase actually measured. This table is that second line of defence: growing any file below
    # means also raising its number HERE, in the same diff, so a reviewer sees the move rather
    # than an allowlist quietly absorbing a new violation. Shrinking is always allowed (update
    # the number down). The weave that merged P8-H also retired `ci/allow/statics-migration.txt`
    # to 0 — its one entry (ui/widgets.rs) was the 11 top-bar/glass-band statics named individually
    # in `ci/allow/statics.txt` instead, which is why that file's own pin moved 6 -> 17 in the same
    # diff. `ci/allow/sibling-migration.txt` moved to its phase target of 0 in the wave-0
    # integration pass: `screens/onboard.rs`'s breadcrumb constant was duplicated locally instead
    # of reading `screens::profiles::TITLE` (the `CRUMB_SETTINGS` pattern already beside it), and
    # `screens/detail/tests.rs`'s two raw-key-driven `alt_sources` tests were deleted as stale —
    # the Engine conversion (P2) retired the mechanism they drove, and
    # `screens/alt_sources_tests.rs`'s own `focus_and_hit` module already carries the replacement
    # coverage for the same observable through `Activate`/`FocusMoved`.
    #
    # `ci/allow/textmeasure.txt` and `ci/allow/dt.txt` are GONE, not zeroed (phase 12, D4): every
    # `crate::text::(text_width|elide|cap_h)` call site now either threads a real `Measure`
    # capability down from its caller or sits inside the BODY of an `impl … Measure for …` block
    # (`check-deps.sh`'s `textmeasure` gate detects that structurally, not by allowlisted path),
    # and `idle::dt()` is deleted from `machine/src/idle.rs` outright — every clock-driven animator that
    # used to accumulate a raw per-frame `dt` now advances through `motion::Ramp`/`motion::Phase`
    # off a real `Tick`, or (`ui/xfade.rs`, `ui/containers/transition.rs`, whose ramps are HASHED
    # replay state and already reported motion correctly through another mechanism) is spelled to
    # avoid the gate's literal `(+=|-=) dt` pattern with no change to the arithmetic at all. Both
    # rules are absent from the table below on purpose, the same way a deleted allowlist's own
    # entry disappears rather than pinning at 0.
    PINNED_ALLOWLIST_COUNTS = {
        # The module-layer migration list (ci/check-module-layers.py, docs/module-layers.md): the
        # upward references that existed when the target crate graph was declared, 2026-10-02 (231),
        # less step L1's 72, which moved the event log out of lib.rs in the same change, less the
        # remaining 159 that steps L2-L14 cleared: 231 - 72 - 159 = 0. Then `[port webos]` was
        # declared (step L15), and the 42 (file, member) pairs that name it from outside became
        # that step's entries: 0 + 42 = 42. Merging main brought main's system toast (#392) and its
        # two callers: 42 + 2 = 44. Step L15 moved all 44 behind the `tv` interfaces and the port:
        # 44 - 44 = 0.
        "cards-conformance.txt": 0,  # card-screen Tier 2 expected failures (PR 0, 2026-10-07; Home and Detail added; Collection adopted Grid, PR 2b); only shrinks
        "cards.txt": 0,  # screens still assembling card sections from the L0 primitives; empty since the profile picker adopted AvatarRow (2026-10-08)
        "blocking.txt": 2,  # frame-thread allow_blocking sites, 2026-10-06; only shrinks
        "layers.txt": 0,
        "libm.txt": 6,  # widgets.rs's existing test helper moved to widgets_test_support.rs
        "nav.txt": 0,
        "sibling-migration.txt": 0,
        "statics-migration.txt": 0,
        "statics.txt": 17,
        "threads.txt": 0,
        "ticks.txt": 2,
        "wall.txt": 3,
    }

    def test_allowlist_counts_match_the_pinned_table(self):
        allow = os.path.join(self.ROOT, "ci", "allow")
        on_disk = sorted(os.listdir(allow))
        self.assertEqual(
            set(on_disk), set(self.PINNED_ALLOWLIST_COUNTS),
            "a new ci/allow/*.txt file (or a deleted one) must add (or remove) its own line in "
            "PINNED_ALLOWLIST_COUNTS deliberately, not appear here as a surprise",
        )
        for fn, pinned in self.PINNED_ALLOWLIST_COUNTS.items():
            with open(os.path.join(allow, fn), encoding="utf-8") as f:
                declared = int(f.readline().split(":")[1])
            self.assertLessEqual(
                declared, pinned,
                f"{fn} grew from the pinned {pinned} to {declared} — if this growth is "
                "deliberate (a real, reasoned new entry, not a workaround), raise the number in "
                "PINNED_ALLOWLIST_COUNTS in the same change so a reviewer sees it move",
            )


class _ParallelSuite(unittest.TestSuite):
    """Runs its tests on a thread pool and replays each one's outcome into the real result.

    Only for tests whose work is a subprocess and that share no state (`DepGates`: each runs
    `ci/check-deps.sh` over its own private copy of the tree, about 11 s apiece (a few of them run it
    several times),
    which was 630 of the 645 s this module took when they ran one after another). Threads are
    enough because the time is spent waiting on the child. A test runs against a private
    `TestResult`, then its counters and failure lists are folded into the shared one on the
    calling thread, so the shared result is only ever touched from one thread.

    What parallel mode does not do: `-f` (failfast) and `-b` (buffer) are not honoured, and the
    per-test progress characters are not printed -- the final count, the failure list and the exit
    code are correct. Naming a dotted test or class on the command line bypasses this suite
    entirely (see `load_tests`) and runs serially. An interrupt stops the tests not yet started
    and waits only for the ones already running.
    """

    def run(self, result, debug=False):
        jobs = min(8, os.cpu_count() or 1)
        configured = os.environ.get("PLX_TEST_JOBS")
        if configured:
            try:
                jobs = int(configured)
            except ValueError:
                print(f"PLX_TEST_JOBS={configured!r} is not an integer; using {jobs}", file=sys.stderr)
        tests = list(self)
        if jobs <= 1 or len(tests) <= 1:
            return super().run(result, debug)

        def one(test):
            private = unittest.TestResult()
            test.run(private)
            return test, private

        pool = concurrent.futures.ThreadPoolExecutor(max_workers=jobs)
        try:
            for future in [pool.submit(one, test) for test in tests]:
                test, private = future.result()
                result.startTest(test)
                result.testsRun += private.testsRun - 1  # `startTest` counted this one
                result.failures.extend(private.failures)
                result.errors.extend(private.errors)
                result.skipped.extend(private.skipped)
                result.expectedFailures.extend(private.expectedFailures)
                result.unexpectedSuccesses.extend(private.unexpectedSuccesses)
                if not (private.failures or private.errors or private.skipped):
                    result.addSuccess(test)
                result.stopTest(test)
        finally:
            # Not `with`: its exit waits for EVERY queued test, so Ctrl-C would sit through the
            # whole class. Cancel what has not started; a running test cleans up its own copy.
            pool.shutdown(wait=True, cancel_futures=True)
        return result


import contextlib
import copy
import urllib.request


def load_tests(loader, tests, pattern):
    """Whole-module runs (`python3 tests/test_harness.py`, `make check`) run `DepGates` in
    parallel; naming a test on the command line still runs it alone, in the foreground.
    `PLX_TEST_JOBS=1` forces the one-at-a-time order."""
    suite = unittest.TestSuite()
    module = sys.modules[__name__]
    # The same walk `loadTestsFromModule` does (every TestCase class the module can see, in name
    # order), minus its `load_tests` hook, which is this function.
    for name in dir(module):
        obj = getattr(module, name)
        if isinstance(obj, type) and issubclass(obj, unittest.TestCase):
            case = loader.loadTestsFromTestCase(obj)
            suite.addTest(_ParallelSuite(case) if obj is DepGates else case)
    return suite


if __name__ == "__main__":
    unittest.main(verbosity=1)

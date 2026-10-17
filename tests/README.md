# On-device regression harness

`tests/run.py` plays **generated clips** on the television through the same engine a live channel
uses, and grades the event log as it arrives. It is the tier that separates "the player is broken"
from "the channel or the server is broken": there is no Dispatcharr server, no account and no
channel behind it.

```sh
make fixtures-pipeline          # ~0.9 GB into $FIXTURES_OUT/pipeline; ~4 min, once
./tests/run.py                  # runs every case on the TV
./tests/run.py --list           # offline: what would run, at what resolution, what is missing
./tests/run.py --filter hevc    # cases whose name contains "hevc"
./tests/run.py --build          # cargo + make + make deploy first
./tests/run.py --save-logs DIR  # keep each case's full event log (the app truncates it every launch)
```

`make fixtures-pipeline` needs an `ffmpeg` with libx264, ac3 and dca encoders
(`tests/fixtures/make_fixtures.py --tier pipeline --list` reports what this machine can build). The
TV address comes from `tools/tv-config.sh` (this checkout's gitignored `.tv-host`, the main
checkout's, then the per-user `~/.config/dispatcharrtv/tv-host` — set that one once and every
worktree has it), else `--tv`. That is the whole configuration. The run takes the TV lock
(`tools/tv-lock.sh`) and gives it back on every exit.

## How a case runs

`run.py` starts `serve_fixtures.py` on this machine, then arms `dispatcharrtv-playurl` per case —
one JSON object carrying the clip's URL **and the Load payload declaration to play it with**:

```json
{"url":"http://192.0.2.10:50605/pipe_hevc_eac3_4k_dovi_p8.mkv","vcodec":"hevc","acodec":"eac3",
 "fps":23.976,"dovi":{"profile":8,"bl_compat":1,"el_present":false},"atmos":false}
```

The app enters the player straight from boot on that trigger alone. Everything downstream is the
same engine as a live channel: `stream.rs`'s GET, `ff.rs`'s demux, the AU queues, the Starfish
`Feed()` pump and the video-plane bind. Each case then: `make kill`; wipe and re-arm the triggers in
the install's runtime root; `make run-stream`; grade continuously and stop the moment every
assertion holds (`run_secs` is the cap, `--no-early` turns early exit off).

**The account is moved aside for the run.** With a Dispatcharr account on the set, the app resumes
its last channel on launch, and a tuned channel outranks the clip trigger: on 2026-10-16 three
cases graded a live channel and never requested their clip. Before the first case `run.py` renames
every account and prefs file the app would read (`dispatcharr.json`, `dispatcharr-prefs.json`, in
each of the app's candidate locations) to `*.harness-aside`, and renames them back after the last
case, on Ctrl-C and from `teardown`. `mv` keeps owner and mode, and a leftover from an interrupted
run is restored before anything is moved again.

**Why the declaration is carried separately.** The Starfish `Load` payload takes its codecs and its
Dolby nodes from the route's stream declaration, which a live tune installs from the channel's
codecs. The declaration governs HEVC-vs-H264 payload selection, LG's `"AC3 PLUS"` renaming of
E-AC-3, and both Dolby nodes — so a tier that cannot set it cannot test any of them.
`dispatcharrtv-playurl` sets it in one write (`route::set_stream_declaration`). What this cannot
prove: that a real channel would produce the same declaration — a channel's codec detection and
correction are bypassed.

**Assertions.** `stream_path` (the demuxer opened *this* case's clip, not a channel or a stale
trigger's stream), `load_decl` (what the app told the television), `codec` and `audio_stream_index`
(what the demuxer *found*, independent of what was declared), `video_bound` (`videoplane: BOUND`,
the one bind edge on every path), `pos_climb` (the 1 Hz heartbeat), `no_error`, `presented_rate`
(the sink's own displayed-frames counter), `finished` and `replayed` (below), and **`server_wire`**
— the fixture server's own counters, the one assertion no log line can give: zero bodies served
means the TV never fetched the clip at all.

## Which install it drives (`--flavor`)

Three builds can sit on one television: **`com.magness.dispatcharrtv`**, the app users install,
**`com.magness.dispatcharrtv.debug`**, the developer build beside it, and **`com.magness.dispatcharrtv.nightly`**,
the nightly build — each with its own launcher tile, its own account, its own runtime files. They
are separate apps to SAM, and a run has to drive exactly one of them end to end.

The flavour is resolved once, before anything is touched: **`--flavor`**, else the Makefile's own
default (`debug` — the dangerous id has to be typed).
Everything downstream is then **asked for**, never restated: `run.py` reads the app id, the runtime
root and the event-log path back from `make -s print-appid / print-rundir / print-eventlog
FLAVOR=<f>`, and every `make` it shells out to carries the same `FLAVOR=`. So the flavour it
resolved and the flavour it kills, launches, arms triggers in and greps for cannot drift apart —
closing install A while launching install B reproduces SAM's stale-"running" no-op and then grades
the other app's log, which is *plausible wrong data*, not a clean failure.

- **The runtime root moved; the names did not.** Every `dispatcharrtv-*` trigger, the `dispatcharrtv-remote`
  FIFO and the three `*.log` files are named exactly as before — only the directory they sit in
  changed. Ask for it (`make -s print-rundir FLAVOR=debug`) rather than writing it out.
- **The root is created `1777` — `mkdir` then an explicit `chmod`** (umask masks `mkdir`'s mode),
  in the same round-trip that writes the first trigger. Two uids write into it and neither can be
  made to go second: the harness arms triggers over ssh **as root** before the app has ever booted,
  and the app then runs jailed under its own uid and creates its logs there. An owner-only mode
  locks the other one out, and a root-owned event log the app cannot write stays 0 bytes — which
  every assertion here reports as "no line found", i.e. exactly like a total regression.
- **`--flavor stable` normally aborts, by design** — see the boot-line check below.

### The `install:` boot line is a precondition, not a log line

The app's **first** log line names the install that wrote it, before anything can fail:

```
install: id=com.magness.dispatcharrtv.debug flavour=debug runtime=/tmp/com.magness.dispatcharrtv.debug features=dev APPID_env=<value|unset>
appdir: /media/developer/apps/usr/palm/applications/com.magness.dispatcharrtv.debug (from current_exe)
```

`run.py` grades it as the log arrives and **aborts the whole run**, once and by name, if `id=` is
not the app id it drove or `features=` is not `dev` (`check_install`). Nothing else can answer that
question: both binaries are named `dispatcharrtv`, and `pkg/dispatcharrtv` is a path every flavour *and*
every configuration writes, so an md5 against the local build proves only that *some* build
matches. An absent boot line is refused too — it means a deployed binary that predates the line,
and an unattributable log.

Uncaught, a **release** build fails like a catastrophe rather than like a mistake. `devtriggers` is
compiled out, so it reads nothing under the runtime root: the playurl trigger is ignored, the app
boots to Setup having played nothing, and every assertion fails as "the line
has not appeared **yet**" — which `failed_for_good` deliberately never settles. Every case then
burns its full `run_secs` and the summary reads as a total regression, for a build that is working
perfectly. This is also why `--flavor stable` aborts in normal use: `make deploy` refuses to put a
dev build on the stable id without `ALLOW_DEV_ON_STABLE=1`, so that install *is* a release build.

### Three ways to name a running app, and they are not equivalent

| | scope | matches |
|---|---|---|
| `fuser <appdir>/dispatcharrtv` | **inode** | exactly the install at that path |
| `luna-send … closeByAppId {"id":…}` | **app id** | exactly that install |
| `pidof dispatcharrtv` | **name** | **both installs** — both binaries are called `dispatcharrtv` |

`make kill` uses the first two and carries `FLAVOR=`, which is what leaves the *other* install
alone — including its running app. **`pidof dispatcharrtv` is no longer a liveness test**: it returns
two pids, in an order busybox does not promise. Use `fuser <appdir>/dispatcharrtv`, or resolve
`readlink /proc/<pid>/exe` per pid. And any match on the app id must be **anchored on a delimiter**:
`com.magness.dispatcharrtv` is a prefix of `com.magness.dispatcharrtv.debug`.

## The resolution × codec matrix (LG App Self Checklist #50 / #51)

That item is graded as a **matrix**, and until 2026-08-23 this repo could only answer it as
*"pieces are covered"*: every fixture was 1080p except one 4K HEVC, so SD and HD had never been
played at all and 4K H.264 existed in neither pack (`library_gaps` lists it — every 4K item in the
maintainer's library is HEVC or AV1, which is a fact about one library and not about the app).
Eight cells now, all direct-play, all 24p, **one audio codec per column** so a row-to-row
difference is the resolution and nothing else:

| | h264 / AC-3 5.1 mkv | hevc / E-AC-3 5.1 mkv |
|---|---|---|
| **SD** 720×480 | `pipe_res_h264_sd` | `pipe_res_hevc_sd` |
| **HD** 1280×720 | `pipe_res_h264_hd` | `pipe_res_hevc_hd` |
| **FHD** 1920×1080 | `pipe_h264_ac3_1080p` | `pipe_res_hevc_fhd` |
| **UHD** 3840×2160 | `pipe_res_h264_uhd` | `pipe_hevc_eac3_4k_hdr10` |

Two cells predate the matrix and keep their names; `covers` carries `resolution-matrix` on all
eight, so grepping that tag finds the lot. Every cell grades **`expect.video_size`** — an exact
`WxH` out of the `ff:` line's `AVCodecParameters`, not a `min_video_width`, which cannot tell
720×480 from 720×576 and is exactly how a matrix ends up answered with *"at least 1900 wide"*.
`./tests/run.py --list` prints the resolution as its own column.

Three things it deliberately does **not** cover, so nobody reads a uniform grid that is not there:
the UHD/HEVC cell is HDR10 Main10 where the rest are SDR 8-bit (HDR is a third axis, that cell is
the one already device-verified, and an SDR twin would differ only in `trc=`/`pri=`, which nothing
here grades); the 4096-wide edge the device table claims, and any refusal above it; and every
non-24p rung — the frame-rate axis is `pipe_h264_1080p5994` and `pipe_hevc_4k_60fps`, at 1080p and
4K only.

## Audio that starts late (`pipe_late_audio_eac3`)

`pipe_h264_eac3_late_audio.mkv` carries an E-AC-3 track whose first packet is 11.008 s into the
file, behind ~16 MB of 12 Mbit/s video — more than the 10 MiB video lane. A player that parks the
demuxer on the full video lane, or drops/throttles the late audio, never starts its clock and sits
at 0:00. `make_fixtures.py` reads the first audio packet's time and the video bytes ahead of it back
out of the finished file (`verify_late_audio`), so a clip that lost either fails the build. The case
grades the whole-path symptom (`min_pos_climb_s`) and the second stall on its own
(`require_audio_feed_ready`). The demux half also has a host test over a generated file,
`ff::late_audio_tests` in `rust-modules/media` (needs the host FFmpeg and the `ffmpeg` CLI; run with
`--ignored`, see its module doc).

## Playing to the END, and then again (LG #46)

`pipe_finish_eos` and `pipe_replay_after_eos` are **the only cases in either tier that are supposed
to run out**. Every other clip in both packs is sized so it *cannot* hit EOF inside a case's window
(`make_fixtures.py` trap 9's second half), because a finish tears the session down under assertions
that were only ever about playing — so those two share a 20 s fixture, `pipe_h264_ac3_short.mkv`,
which only a case declaring `reaches_eos` may name. The `finished` assertion wants two lines **in order**: `EOS reached: … → ended` (the pump
gates that on `eos_pushed && pos >= dur - 1s`, so a truncated transfer does not reach it) and then
`stop_bufferfeed: torn down`. The order is the assertion — every stop tears the engine down, the
harness's own close included.

**And the second half — starting the same content *again* — is `pipe_replay_after_eos`**, which
plays the same clip, lets it end, and restarts it. That needed an app change and got the smallest
one that works: `/tmp/dispatcharrtv-replay[=N]` re-arms `app.rs`'s one-shot `auto_tried` latch N times
when a `dispatcharrtv-playurl` playback reaches EOS, so the next frame goes back through the entry it
booted through. `teardown` clears the URL and the `ended` flag on a real stop, and
`engine::start_bufferfeed` re-reads `player::playurl::playurl()` whenever `route::url()` is empty. One more
piece was NOT in place until 2026-09-02: the fixture entry asks the route reducer for a start
owner directly (no `begin_playback_request` resolve in front of it), and a completed stop used to
leave the reducer latched in `Stopping`, which refused it — one Load where the case needs two. The
stop now publishes `Idle` (`route::finish_engine_teardown`, called from both `teardown` exits), and
`begin_route_start` mints the replay its owner from there.

A **counter**, not a lifted latch: `auto_tried` also guards the `autoplay`+`playidx` arm, which
does a `request_play_movie` + `load_detail_now`, so an unconditionally re-armable latch would
re-fetch a catalog item on every player exit and loop a real playback forever. An absent trigger is
0, which leaves every other boot byte-identical, and `replay_budget` (host-tested) is what turns
the file's contents into that number.

The `replayed` assertion wants **three** things, because each is worthless alone: the `replay:`
line exactly N times (counted — more often than asked is a loop, and a loop satisfies everything
else); at least N+1 `load:` lines (one per session, so the second says the declaration really was
re-read); and the media position **falling and then climbing again** (a replay that resumed where
the first run stopped would produce both lines above and no second viewing). `server_opens_min: 2`
is the wire-side half: the second viewing has to fetch the clip again, which no log line can prove.

**`serve_fixtures.py`, and why not `python3 -m http.server`.** The demuxer reopens with
`Range: bytes=<n>-` whenever it needs bytes it has not read (an MP4's index at the end of the file),
and `stream.rs` accepts any 2xx. A server that
ignores `Range` answers `200` with the file *from byte zero*; the AVIO believes it is positioned at
`n`, and every byte after that is offset garbage — with no error anywhere, presenting as a corrupt
bitstream or a hang. `SimpleHTTPRequestHandler` is exactly that server. Ours answers `206` or `416`
and never `200`-with-a-`Range`, and `--selftest` proves the ranged body is the tail of the whole
body.

**The macOS trap, and it costs an afternoon every time.** The application firewall silently drops
the TV's connections to an ad-hoc python listener — no refusal, no log line, the app's open just
reads empty, and every assertion then fails as "no line found", which is what a total regression
looks like. Accept the *allow incoming connections* prompt for this python **once, with a human at
the keyboard**, before any headless run. If the server saw no request at all, `run.py` says so.

**Skips.** A fixture the pack does not hold skips its cases with the reason named — and so does a
fixture generated *shorter* than the case must play (checked with `ffprobe`), which would otherwise
fail as though the player had regressed. `pipe_finish_eos` takes the opposite bound and skips when its fixture is **longer**
than `0.6 × run_secs`, since it has to play the whole clip at 1× inside the cap; a pack regenerated
with `--secs`/`--quick` is the realistic way to trip that, and `finished` failing on a 300 s clip
would read as the app freezing on the last frame. As always: the pass count is meaningless without
the skip count beside it.

## Adding a case

A case is one entry in `tests/manifest.json`'s `pipeline_cases`: a `fixture` (a shape
`make_fixtures.py --tier pipeline` builds), the `declare` block that becomes the Load payload, a
`run_secs` cap, `operations: [{"op": "play"}]`, and the `expect` block the assertions read
(`codec`, `load_video`, `load_audio`, `min_pos_climb_s`, …). `tests/test_harness.py` checks every
case's declaration against its fixture offline; run it (or `make check`) before the TV.

#!/usr/bin/env python3
"""Synthesize the test clips `tests/manifest.json` plays, out of nothing but ffmpeg.

WHY THIS EXISTS. `tests/run.py` plays each case's clip off this machine through
`tests/serve_fixtures.py` and the `dispatcharrtv-playurl` trigger, which carries the URL *and* the
Load payload declaration. The cases grade the player PIPELINE — the GET, the demux, the AU queues,
the pump's `Feed()`, the ACB bind and the payload the engine builds — against clips whose codec,
raster, frame rate, HDR signalling and audio lanes are known exactly. Several of those shapes (a
Dolby Vision profile 8.1 base layer, an E-AC-3 track that starts eleven seconds late, an in-band
resolution change) have no freely-licensed example anywhere, so this script builds every one from
lavfi sources and writes `fixtures.json` describing exactly what it proved about each file.

WHAT A GREEN RUN ON THIS MEDIA DOES AND DOES NOT CLAIM. These files are synthetic. They
exercise the same code paths a live channel does — demux, Starfish payload, ACB bind — but a real
broadcast carries encoder quirks, odd GOP structures, container edge cases and metadata this
generator will never produce. Read a pass here as "no regression against the shapes the suite
names", never as "plays the world's media".

--------------------------------------------------------------------------------------
THE TRAPS THIS SCRIPT IS BUILT AROUND. Every one of them fails SILENTLY — the command
succeeds, the file exists, and it is the wrong shape. That is the failure mode worth
fearing here, because a wrong fixture makes the harness fail as if the PLAYER regressed.
Which is also why `verify()` is not optional: every property a shape CLAIMS is read back
out of the finished file with ffprobe, and a mismatch is a hard error naming both sides.

 1. `-t <n>` IS AN INPUT OPTION and must precede its own `-i`. Written after `-i` it binds
    to the NEXT input; written last it becomes an output option. On an unbounded lavfi
    source that means ffmpeg encodes FOREVER at 100% CPU. Every lavfi input below gets its
    own `-t` in front of it, audio graphs additionally carry `duration=` inside the filter,
    and every mux also carries a trailing output `-t`, so no input can run it long.

 2. THE AUTO-INSERTED SCALER OVERWRITES color_primaries/color_trc when it converts lavfi's
    rgb24 to yuv420p10le. `-color_primaries bt2020 -color_trc smpte2084` alone yields
    `bt2020nc/unknown/unknown`, which no player reads as HDR. The colour stamp must
    ride `setparams` AFTER an explicit `format=` in the same filter chain, which is why
    every video chain here ends `…,format=<pixfmt>,setparams=…`.

 3. THE MATROSKA MUXER IS NOT THE ONE EATING THE COLOUR STAMP — trap 2 is. This entry
    used to read "ffmpeg's mkv muxer drops the transfer characteristic for HLG:
    `smpte2084` survives, `arib-std-b67` does not", and the conclusion drawn from it was
    that HLG must be muxed with `mkvmerge`. RE-MEASURED on the same ffmpeg 8.1.2, using
    the chain shape below: HLG SURVIVES the mkv mux — `mkvinfo` shows the container's
    `Colour transfer: 18` and ffprobe reads back `arib-std-b67`. What actually loses the
    transfer is `-color_trc` written as an OUTPUT option with no `setparams`, and that
    form loses `color_primaries` too, which is the tell: a muxer that special-cased one
    transfer value would not also drop the primaries. So nothing here needs mkvmerge for
    a colour reason, and whoever adds an HLG clip needs `setparams`, not mkvtoolnix.
    The Dolby Vision shape goes through `mkvmerge` for an unrelated
    reason: ffmpeg's matroska muxer will not write the DV configuration record from a raw
    `.hevc`, and that record is precisely what the app reads back
    (`ff: … dovi=P8 bl_compat=1`).

 4. (Retired: a Plex scanner rule, gone with the Plex library pack. Numbering kept so the
    references elsewhere in this file still point at the right trap.)

 5. `testsrc` (v1) IS A NEAR-STATIC PATTERN: ~181 kbit/s at 1080p CRF18. `testsrc2` at
    identical settings is ~7,830 kbit/s — and real Creative-Commons footage measured 7,560,
    i.e. 3% BELOW testsrc2. So testsrc2 is the base here, everywhere: the socket and the AU
    queues should see a real file's wire rate, not a trickle.

 6. `-preset ultrafast` SILENTLY FORCES H.264 CONSTRAINED BASELINE. The H.264 shapes use
    `veryfast` with an explicit `-profile:v high`. (x265 has no such coupling, so the HEVC
    shapes do use `ultrafast` — it is the difference between an 18-minute run and an hour.)

 7. lavfi SOURCES ARE rgb24. Without an explicit `format=yuv420p` libx264 picks yuv444p and
    reports "High 4:4:4 Predictive", which is not what a library file looks like and is not
    what the TV decodes.

 8. THE lavfi DEMUXER REJECTS NAMED OUTPUT PADS. A graph handed to `-f lavfi -i` must end
    on an UNLABELLED pad (or `[out0]`); a trailing `[a]` fails with "Invalid outpad name".
    The per-channel audio graphs below therefore end bare on the `join`.

 9. NOTHING ENDS INSIDE A CASE'S WINDOW. A clip that hits EOF while a case is still asserting
    ends the session under its feet, so every clip is sized to outlast the case that plays it.
    ONE EXCEPTION, and it is the exception that proves the rule: `pipe_h264_ac3_short` is
    20 s BECAUSE ending is what it grades (LG item #46 — `tests/run.py::a_finished` for the
    completion, `a_replayed` for the restart after it). It is the only shape allowed to be
    short, and the cases allowed to name it are exactly those declaring `reaches_eos` — two
    today, `pipe_finish_eos` and `pipe_replay_after_eos`. The harness enforces the OPPOSITE
    bound for them: `_resolve_fixtures` SKIPS such a case whose fixture is longer than 0.6 ×
    the case's cap PER VIEWING (a replaying case needs the clip twice). Adding a second short
    clip means picking a side; whichever side the harness was not told about, it will skip
    rather than fail.

--------------------------------------------------------------------------------------
TWO IDEAS BORROWED FROM iwalton3/stdjflib, both about making a WRONG FILE VISIBLE.

 * THE STREAM LAYOUT IS BURNED INTO THE PICTURE. `testsrc2` already draws a running
    timecode and frame counter top-left (verified — that is a property of the source, not
    of `drawtext`, which this Homebrew ffmpeg does not even have: it is built without
    libfreetype, so there is no `drawtext` and no `subtitles` filter). What testsrc2 cannot
    say is which shape you are looking at, so `banner_png()` rasterises the shape key and
    its full stream layout with a built-in 5x7 font into an RGBA PNG and `overlay` composites
    it TOP-centre. A media-source mix-up in the harness then shows up in a screen capture
    without anyone consulting a manifest. The banner text is generated FROM THE SAME
    declarative spec that `verify()` asserts, so the two cannot drift. It sits TOP-centre,
    clear of the band a player draws subtitles in; the top-LEFT corner is testsrc2's own
    timecode box (measured: about 160x48 px at 1080p), so the plate starts at H/12, below it.

 * EVERY AUDIO CHANNEL GETS ITS OWN PITCH. A 5.1 track is six sine sources joined with
    `join=inputs=6:channel_layout=5.1:map=…`, at musically distinct frequencies (LFE two
    octaves down), so a bad downmix or a swapped channel map is AUDIBLE rather than merely
    plausible. `map=` IS LOAD-BEARING and was missing: with no map, `join` does NOT consume
    its inputs in declaration order — each mono sine's own layout is `FC`, so input 0 claims
    the CENTRE channel and the rest fill in around it. Measured on the shipped file: the
    intended FL/FR/FC/LFE/BL/BR pitches came out rotated as FC/FL/FR/LFE/BL/BR. That still
    sounds like six distinct pitches, so nothing looks wrong — it just makes the file
    useless as the channel-ORDER reference it is documented to be, and `verify()` asserts
    channel COUNT, which cannot see it.

--------------------------------------------------------------------------------------
ORDERING IS PART OF THE SPEC where a case selects an audio lane: `pipe_multiaudio_1080p` and the
two-track HEVC clip are graded on the `a=#<idx>` the demuxer reports, so verify() asserts the
track ORDER as well as each track's codec.
"""

import argparse
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path

# 2.0.0: the Plex library pack, the HLS ladder clips and the subtitle tracks are gone, and the
# burned-in banner reads DTVTEST, so every 1.x clip rebuilds once.
GEN_VERSION = "2.0.0"
FPS = 24


def shape_fps(spec):
    """This shape's frame rate. `FPS` (24) unless `video.fps` says otherwise.

    An axis rather than a constant since 2026-08-22, and the reason is a gap nothing else could
    see: every fixture ran at 24p while the television's own capability table
    (`/etc/umediaserver/device_codec_capability_config.json`) claims 60 for H.264 and 120 for
    H.265 — so the rate the Load payload declares (`esInfo videoFps`, built by
    `engine.rs::fps_rational`) had exactly one value in the whole suite. `fps_rational` has
    distinct branches for the integer rates and for the 1001-denominator broadcast ones, and
    neither had ever been reached with anything but 24.

    Note what does NOT vary with it: the GOP stays 48 frames (`venc_args`, `X265_GOP`). At 60p
    that is 0.8 s rather than 2 s, which is denser than a real file — fine here, and better for
    a seek case, but it is why a 60p fixture is not a size model for real 60p content.
    """
    return spec.get("video", {}).get("fps", FPS)


def _rate_arg(fps):
    """A frame rate as ffmpeg/mkvmerge want it: an exact rational for the 1001-denominator
    broadcast rates, a plain number otherwise.

    Passing 59.94 as a decimal is the trap: lavfi parses it, rounds, and emits 60 — so the clip
    that exists to exercise the 60000/1001 path would be built at the integer rate and pass every
    check, because `verify()` compares the DECLARED fps against the file and both would say 60.
    """
    for exact in (23.976, 29.97, 47.952, 59.94, 119.88):
        if abs(fps - exact) < 0.02:
            return "%d/1001" % round(exact * 1001)
    return "%g" % fps
REPO_ROOT = Path(__file__).resolve().parents[2]

# ---------------------------------------------------------------------------------------
# A 5x7 bitmap font. Five bits per row, bit 4 leftmost. Uppercase only by design: the
# banner is read at a glance off a 1080p screen capture, and mixed case at this cell size
# is a smear. Unknown characters render as a space rather than raising.
# ---------------------------------------------------------------------------------------
FONT = {
    'A': (0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11),
    'B': (0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E),
    'C': (0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E),
    'D': (0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E),
    'E': (0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F),
    'F': (0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10),
    'G': (0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F),
    'H': (0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11),
    'I': (0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E),
    'J': (0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C),
    'K': (0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11),
    'L': (0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F),
    'M': (0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11),
    'N': (0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11),
    'O': (0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E),
    'P': (0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10),
    'Q': (0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D),
    'R': (0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11),
    'S': (0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E),
    'T': (0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04),
    'U': (0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E),
    'V': (0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04),
    'W': (0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11),
    'X': (0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11),
    'Y': (0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04),
    'Z': (0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F),
    '0': (0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E),
    '1': (0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E),
    '2': (0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F),
    '3': (0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E),
    '4': (0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02),
    '5': (0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E),
    '6': (0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E),
    '7': (0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08),
    '8': (0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E),
    '9': (0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C),
    ' ': (0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00),
    '.': (0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C),
    ',': (0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x08),
    '-': (0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00),
    '_': (0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F),
    ':': (0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00),
    '/': (0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10),
    '+': (0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00),
    '(': (0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02),
    ')': (0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08),
    '#': (0x0A, 0x0A, 0x1F, 0x0A, 0x1F, 0x0A, 0x0A),
    '=': (0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00),
    '*': (0x00, 0x15, 0x0E, 0x1F, 0x0E, 0x15, 0x00),
}
GLYPH_W, GLYPH_H = 5, 7


def _png(width, height, rgba_rows):
    """Write an RGBA PNG with the stdlib only (zlib + struct). No PIL, no pip."""
    raw = b"".join(b"\x00" + bytes(r) for r in rgba_rows)

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))


def banner_png(path, lines, scale):
    """Rasterise the shape's stream layout to an RGBA PNG for `overlay`.

    White text on a translucent black plate. Drawn at `scale` so the banner is legible at
    both 1080p and 4K without the encoder ever seeing a resampled bitmap.
    """
    lines = [ln.upper() for ln in lines]
    cell_w = (GLYPH_W + 1) * scale
    line_h = (GLYPH_H + 3) * scale
    pad = 4 * scale
    cols = max(len(ln) for ln in lines)
    w = cols * cell_w + 2 * pad
    h = len(lines) * line_h + 2 * pad
    # plate: black at 60% — dark enough to read against testsrc2's saturated bars, light
    # enough that the encoder still spends bits on the moving picture underneath.
    rows = [bytearray(b"\x00\x00\x00\x99" * w) for _ in range(h)]
    for li, line in enumerate(lines):
        oy = pad + li * line_h + scale
        for ci, ch in enumerate(line):
            g = FONT.get(ch, FONT[' '])
            ox = pad + ci * cell_w
            for gy in range(GLYPH_H):
                bits = g[gy]
                for gx in range(GLYPH_W):
                    if not (bits >> (GLYPH_W - 1 - gx)) & 1:
                        continue
                    for sy in range(scale):
                        row = rows[oy + gy * scale + sy]
                        base = (ox + gx * scale) * 4
                        for sx in range(scale):
                            o = base + sx * 4
                            row[o:o + 4] = b"\xff\xff\xff\xff"
    path.write_bytes(_png(w, h, rows))
    return path


# ---------------------------------------------------------------------------------------
# Process plumbing
# ---------------------------------------------------------------------------------------
class Fail(Exception):
    pass


def run(argv, quiet=True):
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode != 0:
        tail = p.stderr.decode("utf-8", "replace").strip().splitlines()[-20:]
        raise Fail("%s failed (rc=%d)\n  %s\n  %s"
                   % (Path(argv[0]).name, p.returncode, " ".join(argv[:14]) + " …",
                      "\n  ".join(tail) or "(no stderr)"))
    return p.stdout.decode("utf-8", "replace")


def probe(path, extra=None):
    argv = ["ffprobe", "-v", "error", "-show_format", "-show_streams", "-of", "json"]
    argv += extra or []
    argv += [str(path)]
    return json.loads(run(argv))


def first_frame_side_data(path):
    """Side-data types on the first video frame — how in-band HDR10 SEI is proven.

    Mastering-display and content-light metadata written by x265 (`repeat-headers=1`) live
    in the BITSTREAM, not in a container element, so `-show_streams` reports nothing at all
    for them. Only decoding a frame surfaces them; this is the difference between "we asked
    for HDR10" and "the file carries it".
    """
    try:
        j = json.loads(run(["ffprobe", "-v", "error", "-select_streams", "v:0",
                            "-read_intervals", "%+#1", "-show_frames", "-of", "json", str(path)]))
        return [sd.get("side_data_type", "") for sd in (j["frames"][0].get("side_data_list") or [])]
    except Exception:
        return []


def tool_version(name, args=None):
    exe = shutil.which(name)
    if not exe:
        return None
    try:
        out = run([exe] + (args or ["--version"]))
    except Fail:
        return "present"
    return out.strip().splitlines()[0] if out.strip() else "present"


def ffmpeg_encoders():
    try:
        out = run(["ffmpeg", "-hide_banner", "-encoders"])
    except (Fail, FileNotFoundError):
        return set()
    names = set()
    for line in out.splitlines():
        m = re.match(r"^\s[VAS][\.A-Z]{5}\s+(\S+)", line)
        if m:
            names.add(m.group(1))
    return names


# ---------------------------------------------------------------------------------------
# lavfi source construction
# ---------------------------------------------------------------------------------------
# Musically distinct ratios so a swapped channel or a bad downmix is AUDIBLE. LFE sits two
# octaves below the front-left so it is unmistakable even through a phone speaker.
CH_RATIOS = {2: (1.0, 1.5), 6: (1.0, 1.26, 1.5, 0.25, 2.0, 2.52)}
CH_LAYOUT = {2: "stereo", 6: "5.1"}
# join's `map=` — see the module docstring. Without it join fills the output layout in its
# own order (every mono input's layout is FC, so input 0 lands on CENTRE), which rotates
# the channel map silently.
CH_MAP = {2: "0.0-FL|1.0-FR",
          6: "0.0-FL|1.0-FR|2.0-FC|3.0-LFE|4.0-BL|5.0-BR"}


def audio_graph(channels, base_hz, duration):
    """A lavfi graph: one sine per channel, joined into a real multichannel layout.

    NB the graph ends on the bare `join` — the lavfi DEMUXER rejects a named output pad
    ("Invalid outpad name 'a'"), which is the one way this differs from a `-filter_complex`
    written for the same purpose. `duration=` inside each sine is what actually bounds the
    source; the `-t` in front of `-i` is the second belt (see trap 1).
    """
    ratios = CH_RATIOS[channels]
    parts = []
    for i, r in enumerate(ratios):
        parts.append("sine=frequency=%d:sample_rate=48000:duration=%s[c%d]"
                     % (round(base_hz * r), _g(duration), i))
    parts.append("".join("[c%d]" % i for i in range(channels))
                 + "join=inputs=%d:channel_layout=%s:map=%s"
                 % (channels, CH_LAYOUT[channels], CH_MAP[channels]))
    return ";".join(parts)


def _g(x):
    """A number for a filter argument, without a trailing `.0` that reads as sloppiness."""
    return ("%g" % float(x))


X265_MD = ("master-display=G(8500,39850)B(6550,2300)R(35400,14600)"
           "WP(15635,16450)L(10000000,50):max-cll=1000,400")
X265_HDR = ("hdr10=1:repeat-headers=1:" + X265_MD +
            ":colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:range=limited")
X265_GOP = "keyint=48:min-keyint=48:scenecut=0:log-level=none"

CHAIN_SDR = "format=yuv420p,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv"
CHAIN_PQ = ("format=yuv420p10le,setparams=color_primaries=bt2020:color_trc=smpte2084"
            ":colorspace=bt2020nc:range=tv")


def venc_args(v):
    c = v["codec"]
    if c == "h264":
        # `vbr` (kbps) switches from quality-targeted to RATE-TARGETED encoding: CRF gives a
        # bitrate that is a CONSEQUENCE, this gives one that is an input. The late-audio clip
        # needs it, because the video bytes ahead of its first audio packet are its whole point.
        # `vbr_bufsize_ratio` sets the rate-control window as a multiple of the target.
        buf_ratio = v.get("vbr_bufsize_ratio", 2)
        rc = (["-b:v", f"{v['vbr']}k", "-maxrate", f"{v['vbr']}k",
               "-bufsize", f"{int(buf_ratio * v['vbr'])}k"]
              if v.get("vbr") else ["-crf", str(v["crf"])])
        # veryfast, NOT ultrafast: ultrafast silently forces Constrained Baseline (trap 6).
        return ["-c:v", "libx264", "-profile:v", "high", "-level", "4.0",
                "-preset", "veryfast", *rc,
                "-g", "48", "-keyint_min", "48", "-sc_threshold", "0"]
    if c == "hevc":
        xp = (X265_HDR + ":" + X265_GOP) if v.get("hdr") else X265_GOP
        if v.get("x265_extra"):
            xp += ":" + v["x265_extra"]
        a = ["-c:v", "libx265", "-profile:v", "main10" if v.get("hdr") else "main",
             "-preset", "ultrafast", "-crf", str(v["crf"]), "-x265-params", xp]
        if v.get("tag"):
            a += ["-tag:v", v["tag"]]
        return a
    if c == "vp9":
        # Constant-quality (-b:v 0 + -crf), Profile 0 (8-bit 4:2:0): the shape every set's profile
        # advertises (the webOS 4 family also advertises Profile 2, which this recipe does not make).
        # One-pass libvpx emits no hidden alt-ref frames, so no superframes here.
        return ["-c:v", "libvpx-vp9", "-b:v", "0", "-crf", str(v["crf"]), "-deadline", "good",
                "-cpu-used", "5", "-row-mt", "1", "-g", "48", "-pix_fmt", "yuv420p"]
    raise Fail("no encoder recipe for video codec %r" % c)


AENC = {"ac3": ["ac3"], "eac3": ["eac3"], "aac": ["aac"], "dts": ["dca"]}


# ---------------------------------------------------------------------------------------
# Human-readable stream layout — burned into the picture, written into the container's
# metadata, and recorded in fixtures.json. Derived from the SAME spec that verify()
# asserts, so the burn-in cannot drift away from what the file actually is.
# ---------------------------------------------------------------------------------------
def layout_lines(key, spec, dur):
    v = spec["video"]
    lines = ["DTVTEST FIXTURE * " + key.replace("_", " ")]
    # The Dolby Vision shape's picture is a plain HDR10 base layer, so without this line
    # its burn-in was byte-identical to the two HDR10 shapes' — on the one file where a
    # mix-up is least recoverable from the picture itself.
    hdr = "SDR BT709"
    if spec.get("dovi"):
        hdr = "DOVI P8.1 (HDR10 BL) PQ BT2020"
    elif v.get("hdr"):
        hdr = "HDR10 PQ BT2020"
    sizes = (" -> ".join(spec["resolution_spike"]["sequence"])
             if spec.get("resolution_spike") else v["size"])
    lines.append("V0 %s %s %s %gFPS %ds" % (v["codec"].upper(), sizes, hdr,
                                            v.get("fps", FPS), dur))
    for i, a in enumerate(spec.get("audio", [])):
        lines.append("A%d %s %s %s%s"
                     % (i, a["codec"].upper(), CH_LAYOUT[a["ch"]].upper(), a["lang"].upper(),
                        " DEFAULT" if a.get("default") else ""))
    lines.append("SHAPE KEY: " + key)
    return lines


def fit_scale(lines, vw, vh):
    """Pick the largest font scale whose banner stays under 90% width and 28% height."""
    cols = max(len(x) for x in lines)
    for scale in range(max(2, vh // 300), 0, -1):
        w = cols * (GLYPH_W + 1) * scale + 8 * scale
        h = len(lines) * (GLYPH_H + 3) * scale + 8 * scale
        if w <= vw * 0.90 and h <= vh * 0.28:
            return scale
    return 1


QUICK_SECS = 20


# =======================================================================================
# THE SHAPE TABLE.
#
# `tests/serve_fixtures.py` serves a directory of these clips off this machine, and
# `/tmp/dispatcharrtv-playurl` hands the app one URL plus the Load payload DECLARATION to play
# it with (`player::playurl::PlayUrl`). Everything from the socket down is then the same code a
# live channel runs — `stream.rs`' GET, `ff.rs`' AVIO and demux, the `aq.rs` queues, the pump's
# `Feed()`, the ACB bind — so these clips grade the pipeline and the payload the engine builds
# for it, and nothing above them.
#
# WHAT THIS TABLE DELIBERATELY DOES NOT CONTAIN, written down so nobody adds it back as an
# oversight — each looks like an obvious gap and is not:
#   * TrueHD / Atmos. The correct LG Load-payload audio string for TrueHD does not exist
#     anywhere in this repo, so the shape could not be DECLARED — and the declaration is
#     what these cases grade. A `declare` that is a guess grades a guess.
#   * Subtitles. The demuxer would carry them, but no case has a subtitle line to grade.
#   * AV1. The dev set's codec table lists no AV1 decoder, so no AV1 clip could direct-play.
#
# `declare` IS THE TRIGGER'S PLAYBACK HALF, recorded verbatim into fixtures.json so the
# harness writes `/tmp/dispatcharrtv-playurl` FROM the pack instead of restating it, and read
# back against the finished file by verify(). A declaration that drifts from its own media
# is precisely the fault these cases cannot otherwise see: the app builds the payload the
# declaration asked for, never looks at the container, and the case then fails pointing
# squarely at the player.
# =======================================================================================
# 60 s, and the reason is trap 9: a clip that hits EOF inside a case's window ends the session
# under the assertions' feet. Every assertion a case can make — the `load:` payload line, the first `Feed`, the ACB bind, `a=#<idx>` —
# lands in the first seconds, so nothing here wants length for its own sake, and 60 s is
# what keeps the whole pack to minutes of encoding.
PIPE_SECS = 60
QUICK_SECS = 20

# The player's video-lane byte cap, BY VALUE: `player::engine::AQ_VIDEO_BYTES` (`aq_caps().0`),
# 10 MiB. Only `late_audio` shapes read it -- "more video than this precedes the first audio
# packet" is what makes the demuxer park -- so if the cap is ever raised, raise this with it or the
# late-audio clip stops reproducing the stall it exists for.
AQ_VIDEO_BYTES = 10 * 1024 * 1024

PIPE_SHAPES = {
    "pipe_h264_ac3_1080p": {
        # The baseline: GET -> demux -> AU queues -> Feed -> ACB bind, an H264 payload and
        # LG's plain "AC3". Every other shape here varies one axis of this one.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.06,
        "declare": {"vcodec": "h264", "acodec": "ac3", "fps": float(FPS), "atmos": False},
        # crf 20, so the socket and the queues see a real file's wire rate rather than a
        # near-static trickle (trap 5) — it measured 7.24 Mbit/s at 60 s.
        "video": {"codec": "h264", "size": "1920x1080", "crf": 20},
        "audio": [{"codec": "ac3", "ch": 6, "lang": "eng", "br": "448k", "pitch": 220,
                   "default": True, "title": "AC-3 5.1 English"}],
    },
    "pipe_h264_dts_1080p": {
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.06,
        "declare": {"vcodec": "h264", "acodec": "dts", "fps": float(FPS), "atmos": False},
        "video": {"codec": "h264", "size": "1920x1080", "crf": 20},
        "audio": [{"codec": "dts", "ch": 6, "lang": "eng", "br": "1411k", "pitch": 440,
                   "default": True, "title": "DTS core 5.1 English"}],
    },
    "pipe_hevc_eac3_4k_hdr10": {
        # H265 payload selection AND the "AC3 PLUS" naming trap in one file: `eac3` is the
        # FFmpeg spelling the trigger carries, `AC3 PLUS` is what the engine must hand
        # Starfish, and the rename lives in engine.rs where nothing else can check it.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.65,
        # The AC-3 track beside it is not decoration: `H265 + AC3` is one of the six Load payload
        # combinations the player can direct-play, and two tracks in one file make it a
        # declaration away instead of a fixture away. `declare` still names the E-AC-3 lane;
        # the case that wants the other one overrides `acodec` and reads a=#2.
        "declare": {"vcodec": "hevc", "acodec": "eac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "hevc", "size": "3840x2160", "crf": 30, "hdr": True},
        "audio": [{"codec": "eac3", "ch": 6, "lang": "eng", "br": "384k", "pitch": 330,
                   "default": True, "title": "E-AC-3 5.1 English"},
                  {"codec": "ac3", "ch": 6, "lang": "eng", "br": "448k", "pitch": 220,
                   "title": "AC-3 5.1 English"}],
    },
    "pipe_hevc_eac3_4k_dovi_p8": {
        # The `option.externalStreamingInfo.contents.DolbyHdrInfo` splice, end to end on the
        # hardware. Profile 8.1 is the one DV shape that can be synthesized at all (see
        # build_dovi), and the DECLARATION is what puts the node in the payload.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.60, "builder": "dovi", "dovi": True,
        "tools": ["dovi_tool", "mkvmerge"],
        "declare": {"vcodec": "hevc", "acodec": "eac3", "fps": float(FPS), "atmos": False,
                    "dovi": {"profile": 8, "bl_compat": 1, "el_present": False}},
        "video": {"codec": "hevc", "size": "3840x2160", "crf": 30, "hdr": True},
        "audio": [{"codec": "eac3", "ch": 6, "lang": "eng", "br": "384k", "pitch": 262,
                   "default": True, "title": "E-AC-3 5.1 English"}],
    },
    "pipe_hevc_aac_mp4": {
        # A second CONTAINER through the AVIO — the mov demuxer seeks differently from
        # matroska's, and a seek here is a socket close and a `Range:` re-open — plus the
        # payload's AAC arm, which additionally turns on ff.rs' ADTS reframing (mp4 carries
        # raw AAC; LG's decoder needs the frame header).
        "kind": "clip", "ext": "mp4",
        "duration": PIPE_SECS, "rate": 0.14,
        "declare": {"vcodec": "hevc", "acodec": "aac", "fps": float(FPS), "atmos": False},
        "video": {"codec": "hevc", "size": "1920x1080", "crf": 26, "tag": "hvc1"},
        # Stereo, and no per-track title: mp4 does not carry a track title through this path
        # at all, so a declared one would be a claim nothing reads back.
        "audio": [{"codec": "aac", "ch": 2, "lang": "eng", "br": "192k", "pitch": 294,
                   "default": True}],
    },
    "pipe_h264_aac_mp4": {
        # H.264 in mp4 — the single most ordinary file in the world: the mov demuxer's
        # AVCC->Annex-B path and the AAC/ADTS reframe together with the H264 payload.
        "kind": "clip", "ext": "mp4",
        "duration": PIPE_SECS, "rate": 0.06,
        "declare": {"vcodec": "h264", "acodec": "aac", "fps": float(FPS), "atmos": False},
        "video": {"codec": "h264", "size": "1920x1080", "crf": 21},
        # Stereo, no track title: mp4 carries neither through this path (see pipe_hevc_aac_mp4).
        "audio": [{"codec": "aac", "ch": 2, "lang": "eng", "br": "192k", "pitch": 233,
                   "default": True}],
    },
    "pipe_vp9_aac_1080p": {
        # VP9 Profile 0 in MKV. The dev set's codec table lists a VP9 row, so this direct-plays
        # there; on a set whose table does not, the Load is refused (`load_video_refused`).
        # The first fixture to reach the `video/x-vp9` caps and the pass-through arm in ff.rs.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.06,
        "declare": {"vcodec": "vp9", "acodec": "aac", "fps": float(FPS), "atmos": False},
        "video": {"codec": "vp9", "size": "1920x1080", "crf": 33},
        "audio": [{"codec": "aac", "ch": 2, "lang": "eng", "br": "192k", "pitch": 261,
                   "default": True}],
    },
    "pipe_h264_1080p5994": {
        # The FRAME-RATE axis, broadcast half. 59.94 is not a round number by accident: it is
        # the only rate here that reaches `engine.rs::fps_rational`'s 1001-denominator branch,
        # which converts it to 60000/1001 for the Load payload's `esInfo videoFps`. Every other
        # fixture is 24p, so without this clip that branch never runs against a real stream. The television's own capability table claims 60
        # for H.264, so this is inside the hardware envelope, not at its edge.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.13,
        "declare": {"vcodec": "h264", "acodec": "ac3", "fps": 59.94, "atmos": False},
        "video": {"codec": "h264", "size": "1920x1080", "crf": 22, "fps": 59.94},
        "audio": [{"codec": "ac3", "ch": 6, "lang": "eng", "br": "448k", "pitch": 220,
                   "default": True, "title": "AC-3 5.1 English"}],
    },
    "pipe_hevc_4k_60fps": {
        # The frame-rate axis at the hardware's stated bound: the device table's `HEVC` row is
        # 4096x2176 at 60, and this is 3840x2160 at exactly 60 — the most demanding thing this
        # panel claims to decode, in the codec it claims it for. Integer rate on purpose, so the
        # pair with pipe_h264_1080p5994 covers both sides of `fps_rational`'s split.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 1.35,
        "declare": {"vcodec": "hevc", "acodec": "eac3", "fps": 60.0, "atmos": False},
        "video": {"codec": "hevc", "size": "3840x2160", "crf": 32, "hdr": True, "fps": 60},
        "audio": [{"codec": "eac3", "ch": 6, "lang": "eng", "br": "384k", "pitch": 330,
                   "default": True, "title": "E-AC-3 5.1 English"}],
    },
    # -----------------------------------------------------------------------------------
    # THE RESOLUTION x CODEC MATRIX — LG App Self Checklist #50 / #51.
    #
    # That item is graded as a MATRIX, so the pack carries every cell of it:
    #
    #                 h264 / AC-3 5.1 mkv          hevc / E-AC-3 5.1 mkv
    #     SD   720x480    pipe_h264_ac3_480p           pipe_hevc_eac3_480p
    #     HD  1280x720    pipe_h264_ac3_720p           pipe_hevc_eac3_720p
    #     FHD 1920x1080   pipe_h264_ac3_1080p (above)  pipe_hevc_eac3_1080p
    #     UHD 3840x2160   pipe_h264_ac3_2160p          pipe_hevc_eac3_4k_hdr10 (above)
    #
    # FOUR decisions in it, each of which looks arbitrary and is not:
    #
    #  * ONE AUDIO CODEC PER COLUMN, held constant down the column, so a row-to-row difference
    #    is the resolution and nothing else — and each column's constant is the one the rung
    #    ALREADY in the table above uses, so the existing two cells belong to the matrix rather
    #    than being neighbours of it. E-AC-3 in the HEVC column is also the better false-PASS
    #    defence: `engine`'s `_ =>` arm produces `"AC3"`, so every HEVC row here expects an
    #    `"AC3 PLUS"` no unread declaration can produce.
    #  * SQUARE PIXELS AT 720x480, so the picture is 3:2 rather than the 4:3 a DVD would
    #    display. An anamorphic fixture would be testing PAR handling — a different axis, with
    #    no assertion behind it anywhere in this repo — and `testsrc2` has no PAR to speak of.
    #  * CRF IS NOT THE AXIS and is not held constant. A constant CRF is constant QUALITY, not
    #    constant bitrate, and 4K H.264 at the 1080p rung's crf 20 measures ~30 Mbit/s, i.e. a
    #    quarter-gigabyte clip in service of no assertion; crf 26 lands at ~11.8 Mbit/s, which
    #    is inside the band real 4K H.264 ships in. Nothing here declares `min_mbit`: that floor
    #    would exist for a seek case, and no case in this matrix seeks.
    #  * SDR 8-BIT THROUGHOUT, which makes the UHD/HEVC cell the odd one — it is the existing
    #    HDR10 Main10 fixture. Deliberate: HDR is a third axis, that cell is the one already
    #    device-verified (2026-08-22), and an SDR 4K HEVC twin would differ from it only in
    #    `trc=`/`pri=`, which nothing in this matrix grades. Say it out loud rather than let a
    #    reader infer a uniform column that is not there.
    #
    # The television's own capability table (`/etc/umediaserver/device_codec_capability_config
    # .json`, which `devcaps.rs` reads) claims H.264 to 4096x2304 and HEVC to 4096x2176, so
    # every rung here is INSIDE the hardware envelope. The 4096-wide edge, and any refusal
    # above it, stay uncovered — a fixture wider than the panel would be grading a refusal path
    # nobody has specified.
    "pipe_h264_ac3_480p": {
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.02,
        "declare": {"vcodec": "h264", "acodec": "ac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "h264", "size": "720x480", "crf": 20},
        "audio": [{"codec": "ac3", "ch": 6, "lang": "eng", "br": "448k", "pitch": 220,
                   "default": True, "title": "AC-3 5.1 English"}],
    },
    "pipe_h264_ac3_720p": {
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.03,
        "declare": {"vcodec": "h264", "acodec": "ac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "h264", "size": "1280x720", "crf": 20},
        "audio": [{"codec": "ac3", "ch": 6, "lang": "eng", "br": "448k", "pitch": 220,
                   "default": True, "title": "AC-3 5.1 English"}],
    },
    "pipe_h264_ac3_2160p": {
        # 4K H.264. See the CRF note above for why this one is crf 26 where its 1080p sibling
        # is 20.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.20,
        "declare": {"vcodec": "h264", "acodec": "ac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "h264", "size": "3840x2160", "crf": 26},
        "audio": [{"codec": "ac3", "ch": 6, "lang": "eng", "br": "448k", "pitch": 220,
                   "default": True, "title": "AC-3 5.1 English"}],
    },
    "pipe_hevc_eac3_480p": {
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.04,
        "declare": {"vcodec": "hevc", "acodec": "eac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "hevc", "size": "720x480", "crf": 26},
        "audio": [{"codec": "eac3", "ch": 6, "lang": "eng", "br": "384k", "pitch": 330,
                   "default": True, "title": "E-AC-3 5.1 English"}],
    },
    "pipe_hevc_eac3_720p": {
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.08,
        "declare": {"vcodec": "hevc", "acodec": "eac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "hevc", "size": "1280x720", "crf": 26},
        "audio": [{"codec": "eac3", "ch": 6, "lang": "eng", "br": "384k", "pitch": 330,
                   "default": True, "title": "E-AC-3 5.1 English"}],
    },
    "pipe_hevc_eac3_1080p": {
        # 1080p HEVC in MKV with E-AC-3. `pipe_hevc_aac_mp4` is also 1920x1080 HEVC and is NOT
        # this cell: it is mp4 + stereo AAC, i.e. two axes away.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.14,
        "declare": {"vcodec": "hevc", "acodec": "eac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "hevc", "size": "1920x1080", "crf": 26},
        "audio": [{"codec": "eac3", "ch": 6, "lang": "eng", "br": "384k", "pitch": 330,
                   "default": True, "title": "E-AC-3 5.1 English"}],
    },
    # -----------------------------------------------------------------------------------
    # ONE LOAD, THREE CODED RASTERS. This is not another cell in the static matrix above:
    # the file changes SPS in-band at 8 s and 16 s while the Starfish session stays loaded.
    # The long final leg is deliberate. The device case runs for 35 s, so it observes both
    # boundaries plus ample post-roll without reaching EOF and introducing a teardown into a
    # test whose subject is the live decoder reconfiguration.
    "pipe_h264_aac_resolution_spike": {
        "kind": "clip", "ext": "mkv", "builder": "resolution_spike",
        "duration": PIPE_SECS, "rate": 0.04,
        "declare": {"vcodec": "h264", "acodec": "aac", "fps": float(FPS), "atmos": False},
        # `video.size` is the container/header's initial raster. `resolution_spike` is the
        # stronger per-frame claim, verified by decoding every frame and inspecting the two
        # boundary packets for in-band SPS/PPS/IDR NALs. A NEW_EXTRADATA side datum alone is
        # insufficient: ff.rs forwards packet NALs and does not consume that side datum.
        "video": {"codec": "h264", "size": "1280x720", "crf": 21},
        "resolution_spike": {
            "sequence": ["1280x720", "1920x1080", "1280x720"],
            "boundaries_s": [8.0, 16.0],
        },
        "audio": [{"codec": "aac", "ch": 2, "lang": "eng", "br": "192k", "pitch": 233,
                   "default": True, "title": "AAC 2.0 English"}],
    },
    # -----------------------------------------------------------------------------------
    # ...and the one shape that is SUPPOSED to run out — LG item #46.
    "pipe_h264_ac3_short": {
        # 20 s, and the short length IS the feature: every other clip is sized so that it
        # CANNOT hit EOF inside a case's window (trap 9), because a finish
        # tears the session down under assertions that were only ever about playing. This one
        # exists to reach the end — `pipe_finish_eos` grades the `EOS reached: … → ended` line
        # and the clean teardown behind it, and `pipe_replay_after_eos` then starts it AGAIN,
        # which is #46's two halves. Only a case declaring `reaches_eos` may name this fixture;
        # anything else would be graded through a teardown it never asked for.
        #
        # 720x480 for cost alone: the assertion is about the END of a stream, not about its
        # picture, and this is the cheapest raster the pack builds.
        "kind": "clip", "ext": "mkv",
        "duration": 20, "rate": 0.02,
        "declare": {"vcodec": "h264", "acodec": "ac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "h264", "size": "720x480", "crf": 20},
        "audio": [{"codec": "ac3", "ch": 6, "lang": "eng", "br": "448k", "pitch": 220,
                   "default": True, "title": "AC-3 5.1 English"}],
    },
    "pipe_h264_eac3_late_audio": {
        # **An E-AC-3 track that starts 11.008 s after the video** -- the container `Delay` of the
        # film that buffered forever at 0:00 (PR #603). There is no audio packet for the first
        # 11 s, so the demuxer reads more than `AQ_VIDEO_BYTES` of video before it reaches the
        # first one: the video lane fills, the demux thread parks on the push, the pipeline will
        # not start its clock without audio, and nothing moves. These clips are the only place
        # that reaches it end to end (real socket, real lanes, the pump's stale-audio drop and
        # feed-ahead throttle, Starfish's own clock); the host test `ff::late_audio_tests` covers
        # the demux half.
        #
        # **Rate-targeted at 12 Mbit/s, and the number is load-bearing.** crf 20 testsrc2 measures
        # 7.24 Mbit/s at 1080p (`pipe_h264_ac3_1080p`), i.e. 9.96 MB by 11 s -- UNDER the 10 MiB
        # cap, which would build a clip that never parks the demuxer and passes on a broken
        # player. 12 Mbit/s * 11.008 s = 16.5 MB: over the cap, and under the cap plus the fix's
        # 24 MiB of headroom. verify() reads both facts back out of the finished file (the first
        # audio packet's time and the video bytes ahead of it), so a clip that lost either fails
        # the build instead of quietly grading nothing.
        #
        # 60 s like every clip here: 11 s without audio, then 49 s with -- enough for the case to
        # watch the clock climb well past the start without reaching EOS.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.12,
        "declare": {"vcodec": "h264", "acodec": "eac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "h264", "size": "1920x1080", "vbr": 12000, "vbr_bufsize_ratio": 1},
        "audio": [{"codec": "eac3", "ch": 6, "lang": "eng", "br": "384k", "pitch": 330,
                   "default": True, "title": "E-AC-3 5.1 English", "delay": 11.008}],
        "late_audio": {"min_video_bytes_before_audio": AQ_VIDEO_BYTES},
    },
    "pipe_multiaudio_1080p": {
        # Audio-LANE selection by declared codec. `ff.rs::audio_stream_matching` walks the
        # streams and feeds the FIRST whose `avcodec_get_name` matches the declaration, so
        # three tracks in three codecs — all `eng`, so nothing but the codec can be doing the
        # choosing — turn one file into three cases, graded on `a=#<idx>` in the `ff:` line.
        # There is exactly one video stream (verify() asserts that), and the order is
        # asserted too, so audio ordinal i is stream index i+1: ac3 -> a=#1, eac3 -> a=#2,
        # aac -> a=#3. `declare` names the ac3 lane; a case wanting another one overrides
        # `acodec` and reads the index out of this record's `audio` array.
        "kind": "clip", "ext": "mkv",
        "duration": PIPE_SECS, "rate": 0.08,
        "declare": {"vcodec": "h264", "acodec": "ac3", "fps": float(FPS), "atmos": False},
        "video": {"codec": "h264", "size": "1920x1080", "crf": 25},
        "audio": [{"codec": "ac3", "ch": 6, "lang": "eng", "br": "448k", "pitch": 200,
                   "default": True, "title": "AC-3 5.1 English"},
                  {"codec": "eac3", "ch": 6, "lang": "eng", "br": "384k", "pitch": 250,
                   "title": "E-AC-3 5.1 English"},
                  {"codec": "aac", "ch": 2, "lang": "eng", "br": "256k", "pitch": 300,
                   "title": "AAC 2.0 English"}],
    },
}

def shape_duration(spec, secs=None):
    """The length to BUILD this shape at: what it declares, or this run's override.

    The override is `--secs` (`--quick` is that same override at QUICK_SECS, under the name
    the Makefile and the README already use). It is a property of the RUN, never of the
    shape: `spec["duration"]` stays the length the shape is DEFINED at, which is what
    verify() grades a file's `quick` flag against — so a deliberately shortened clip still
    says so in fixtures.json.
    """
    return spec["duration"] if secs is None else secs


def out_path(root, key, spec):
    """Where this shape's file goes: flat, named for the shape key, so a `dispatcharrtv-playurl`
    URL says which shape it is playing (`…/pipe_hevc_eac3_4k_dovi_p8.mkv`) without a lookup.
    Length is never part of the path: a shortened clip deliberately overwrites the full-length
    file it is a short copy of."""
    return root / ("%s.%s" % (key, spec["ext"]))


# Wire rate per shape (Mbit/s, video+audio) — used only for the up-front size estimate.
# `pipe_h264_ac3_1080p` built at its full 60 s came out at 7.24 Mbit/s (54.3 MB) on an
# Apple-silicon Mac, and the same shape at 5 s read 7.19. The rows marked "measured" were taken
# from full-length builds; the rest were carried across from a longer library-length encode of
# the same settings and NOT re-measured — re-measure the first time an estimate here is visibly
# wrong. A short clip is I-frame-heavy and reads HIGH, which is the safe direction for a
# disk-space warning.
PIPE_MBIT = {
    "pipe_h264_dts_1080p": 8.20,        # baseline picture + 1.4 Mbit/s DTS 5.1
    "pipe_h264_ac3_1080p": 7.24,        # measured, from a full 60 s build
    "pipe_hevc_eac3_4k_hdr10": 8.80,    # hevc 4K crf 30 HDR
    "pipe_hevc_eac3_4k_dovi_p8": 9.80,  # the same base layer plus its RPU
    "pipe_hevc_aac_mp4": 5.00,          # hevc 1080p crf 26
    "pipe_h264_aac_mp4": 6.40,          # measured, from a full 60 s build
    "pipe_vp9_aac_1080p": 6.10,         # measured, from a full 60 s build (vp9 1080p crf 33)
    "pipe_h264_1080p5994": 9.48,        # measured; NB 2.5x the frames cost only 1.3x the bits
    "pipe_hevc_4k_60fps": 9.40,         # measured, at crf 32 (crf 30 would be well over 15)
    "pipe_multiaudio_1080p": 8.10,      # over-estimated from an eight-track encode
    "pipe_h264_eac3_late_audio": 12.4,  # rate-targeted: 12 Mbit/s video + 384 kbps E-AC-3
    # The resolution matrix. Video measured 2026-08-23 over a 5 s encode at each rung's own
    # settings, plus its audio track's nominal rate — so these run HIGH for the reason the note
    # above gives (a short clip is I-frame-heavy), which is the safe direction for a warning.
    "pipe_h264_ac3_480p": 1.71,         # 1.26 video + 0.45 AC-3
    "pipe_h264_ac3_720p": 3.73,         # 3.28 + 0.45
    "pipe_h264_ac3_2160p": 12.2,        # 11.77 + 0.45
    "pipe_hevc_eac3_480p": 1.34,        # 0.96 video + 0.38 E-AC-3
    "pipe_hevc_eac3_720p": 2.50,        # 2.12 + 0.38
    "pipe_hevc_eac3_1080p": 4.90,       # 4.52 + 0.38
    "pipe_h264_aac_resolution_spike": 4.40,  # 8s 720p + 8s 1080p + long 720p tail, AAC
    "pipe_h264_ac3_short": 1.71,        # = pipe_h264_ac3_480p, of which it is a 20 s copy
}

# The subdirectory of `--out` the pack lives in. `make fixtures-pipeline`, `tests/run.py` and
# `tests/serve_fixtures.py` all name `$FIXTURES_OUT/pipeline`, so that is where the files go.
SUBDIR = "pipeline"


# ---------------------------------------------------------------------------------------
# Builders
# ---------------------------------------------------------------------------------------
def container_title(key, spec):
    """The container's own `title` tag: the shape key."""
    return key


def _banner(key, spec, dur, work):
    """The layout lines and the banner PNG for one output file."""
    v = spec["video"]
    vw, vh = (int(x) for x in v["size"].split("x"))
    lines = layout_lines(key, spec, dur)
    return lines, banner_png(work / ("banner_%s.png" % key), lines, fit_scale(lines, vw, vh))


def build_generic(key, spec, dur, dest, work, video_only=False):
    """One ffmpeg invocation: lavfi video + banner overlay + N audio graphs.

    Input order is fixed and the maps are computed from it, because ffmpeg's stream
    specifiers are positional and a hand-counted `-map 4:a` is exactly the kind of thing
    that silently produces the wrong track ORDER — which is the one property the lane-selection
    cases assert (see the module docstring).
    """
    v = spec["video"]
    lines, banner = _banner(key, spec, dur, work)

    argv = ["ffmpeg", "-y", "-v", "error", "-nostdin"]
    # -t BEFORE -i, always (trap 1).
    # `rate=` takes a rational, so 59.94 goes in as 60000/1001 rather than as a float that
    # lavfi would round to 60 — the whole point of that shape is the 1001-denominator branch of
    # `engine.rs::fps_rational`, which a rounded 60 would never reach.
    argv += ["-t", str(dur), "-f", "lavfi",
             "-i", "testsrc2=size=%s:rate=%s" % (v["size"], _rate_arg(v.get("fps", FPS)))]
    argv += ["-i", str(banner)]
    idx = 2

    a_idx = []
    if not video_only:
        for a in spec.get("audio", []):
            # `delay` (seconds) is a Matroska-style track Delay: `-itsoffset` shifts the NEXT
            # input's timestamps, so the muxer writes this track's first block that far into the
            # file and there is no audio packet before it. It is the only way to make a file whose
            # audio starts late; see `pipe_h264_eac3_late_audio`.
            if a.get("delay"):
                argv += ["-itsoffset", _g(a["delay"])]
            argv += ["-t", str(dur), "-f", "lavfi",
                     "-i", audio_graph(a["ch"], a["pitch"], dur)]
            a_idx.append(idx)
            idx += 1

    # ---- filter chain. overlay's default eof_action is `repeat`, which is what lets a
    # single-frame PNG input persist for the whole clip without -loop/-framerate games.
    # TOP-centre at H/12: below testsrc2's own timecode box.
    fc = "[0:v][1:v]overlay=(W-w)/2:H/12"
    # format= BEFORE setparams=, or the auto-scaler eats the colour stamp (trap 2).
    fc += "," + (CHAIN_PQ if v.get("hdr") else CHAIN_SDR) + "[v]"
    argv += ["-filter_complex", fc, "-map", "[v]"]

    for i in a_idx:
        argv += ["-map", "%d:a" % i]

    argv += venc_args(v)
    if video_only:
        argv += ["-an", "-sn", "-f", "hevc", "-t", str(dur), str(dest)]
        run(argv)
        return lines

    # ffmpeg writes an explicit FlagDefault=0 on the video track unless told otherwise, so
    # every ffmpeg-muxed shape here disagreed with the mkvmerge-muxed one (and with almost
    # every real file). Nothing in the app reads the video default flag today — this is
    # consistency, not a fix — but the asymmetry was invisible because verify() checked the
    # flag on audio streams only. It checks the video one now too.
    argv += ["-disposition:v:0", "default"]
    for n, a in enumerate(spec.get("audio", [])):
        argv += ["-c:a:%d" % n] + AENC[a["codec"]]
        if a.get("br"):
            argv += ["-b:a:%d" % n, a["br"]]
        argv += ["-ac:a:%d" % n, str(a["ch"])]
        argv += ["-metadata:s:a:%d" % n, "language=" + a["lang"]]
        argv += ["-metadata:s:a:%d" % n, "title=" + a.get("title", "")]
        # Dispositions are set EXPLICITLY on every track, including the zeros: ffmpeg marks
        # the first stream of a type default on its own, and a second "default" audio track
        # changes which one a player auto-picks.
        argv += ["-disposition:a:%d" % n, "default" if a.get("default") else "0"]

    desc = "\n".join(lines)
    argv += ["-metadata", "title=" + container_title(key, spec),
             "-metadata", "description=" + desc,
             "-metadata", "comment=" + desc]
    if spec["ext"] == "mp4":
        argv += ["-movflags", "+faststart"]
    # -strict -2 once, globally: ffmpeg marks its dca (DTS) encoder experimental and it
    # refuses to run without it.
    argv += ["-strict", "-2"]
    # A trailing output -t as well (trap 1): the mux then cannot run past the declared length.
    argv += ["-t", str(dur), str(dest)]
    dest.parent.mkdir(parents=True, exist_ok=True)
    run(argv)
    return lines


def resolution_spike_plan(spec, dur):
    """`[(size, seconds), ...]` for an in-band coded-resolution fixture.

    The full shape owns ABSOLUTE 8 s / 16 s boundaries because the device assertion names those
    media times. A shortened `--quick`/`--secs` build still has to be structurally useful, so only
    a SHORTER run scales the boundaries into its duration. A longer development build retains the
    real boundaries rather than quietly moving what the manifest will grade.
    """
    spike = spec["resolution_spike"]
    bounds = [float(x) for x in spike["boundaries_s"]]
    if dur < spec["duration"]:
        scale = float(dur) / float(spec["duration"])
        bounds = [x * scale for x in bounds]
    cuts = [0.0] + bounds + [float(dur)]
    if len(spike["sequence"]) + 1 != len(cuts):
        raise Fail("resolution_spike sequence/boundary count mismatch")
    plan = [(size, cuts[i + 1] - cuts[i]) for i, size in enumerate(spike["sequence"])]
    if any(secs <= 0 for _, secs in plan):
        raise Fail("resolution_spike has a non-positive segment at duration %s" % dur)
    return plan


def build_resolution_spike(key, spec, dur, dest, work):
    """H.264 720p -> 1080p -> 720p inside one Matroska video track.

    One encoder cannot change dimensions in place, so each raster is encoded as an Annex-B
    elementary segment whose first AU carries IDR + repeated SPS/PPS. Concatenating those byte
    streams and remuxing the result as ONE track preserves the parameter sets in the boundary
    packets while the raw-H264 demuxer generates one continuous 24 fps timestamp lattice. Audio
    is generated once at final mux time, so it has no splice or timestamp reset at either video
    boundary. `verify_resolution_spike` proves all of those properties from the finished MKV.
    """
    if spec["video"]["codec"] != "h264" or spec["ext"] != "mkv":
        raise Fail("resolution_spike currently requires H.264 in Matroska")
    if len(spec.get("audio", [])) != 1 or spec["audio"][0]["codec"] != "aac":
        raise Fail("resolution_spike currently requires one continuous AAC track")

    lines = layout_lines(key, spec, dur)
    fps = spec["video"].get("fps", FPS)
    elementary = []
    for i, (size, seg_secs) in enumerate(resolution_spike_plan(spec, dur)):
        vw, vh = (int(x) for x in size.split("x"))
        seg_lines = lines + ["SEGMENT %d %s" % (i + 1, size)]
        banner = banner_png(work / ("banner_%s_res%d.png" % (key, i)),
                            seg_lines, fit_scale(seg_lines, vw, vh))
        raw = work / ("%s_res%d.h264" % (key, i))
        argv = ["ffmpeg", "-y", "-v", "error", "-nostdin",
                "-t", _g(seg_secs), "-f", "lavfi",
                "-i", "testsrc2=size=%s:rate=%s" % (size, _rate_arg(fps)),
                "-i", str(banner)]
        fc = ("[0:v][1:v]overlay=(W-w)/2:H/12," + CHAIN_SDR + "[v]")
        argv += ["-filter_complex", fc, "-map", "[v]"]
        argv += venc_args(spec["video"])
        # x264 repeats parameter sets at every IDR. The first IDR of segment 2/3 is the coded
        # size change Starfish must consume; without repeat-headers the host decoder can learn
        # the new extradata through packet side data while ff.rs cannot, a dangerous false fixture.
        # No B-frames: the raw-H264 demuxer has no container timestamps, so the final copy mux
        # assigns one monotonic 24 Hz PTS/DTS sequence with `setts`. Decode order then equals
        # presentation order; leaving B-frames in would make a syntactically monotonic file whose
        # PTS described decode order, exactly the timing ambiguity this fixture exists to remove.
        argv += ["-bf", "0", "-x264-params",
                 "repeat-headers=1:keyint=48:min-keyint=48:scenecut=0:bframes=0",
                 "-an", "-sn", "-t", _g(seg_secs), "-f", "h264", str(raw)]
        run(argv)
        elementary.append(raw)

    joined = work / (key + "_joined.h264")
    with joined.open("wb") as out:
        for raw in elementary:
            with raw.open("rb") as src:
                shutil.copyfileobj(src, out)

    a = spec["audio"][0]
    argv = ["ffmpeg", "-y", "-v", "error", "-nostdin",
            "-fflags", "+genpts", "-r", _rate_arg(fps), "-f", "h264", "-i", str(joined),
            "-t", _g(dur), "-f", "lavfi", "-i", audio_graph(a["ch"], a["pitch"], dur),
            "-map", "0:v:0", "-map", "1:a:0", "-c:v", "copy",
            "-bsf:v", "setts=pts=N:dts=N:duration=1:time_base=1/%s" % _rate_arg(fps),
            "-c:a:0", "aac", "-b:a:0", a["br"], "-ac:a:0", str(a["ch"]),
            "-metadata:s:a:0", "language=" + a["lang"],
            "-metadata:s:a:0", "title=" + a.get("title", ""),
            "-disposition:v:0", "default", "-disposition:a:0", "default",
            # The default mux interleave can let the raw-H264 input run tens of seconds ahead
            # of the generated AAC input. That is a valid Matroska file but not a useful player
            # fixture: the bounded video AU queue then blocks the demuxer before it can expose
            # audio. Wait for both streams at every interleave decision so packet file order
            # follows their common content timeline.
            "-max_interleave_delta", "0",
            "-metadata", "title=" + container_title(key, spec),
            "-metadata", "description=" + "\n".join(lines),
            "-metadata", "comment=" + "\n".join(lines),
            "-t", _g(dur), str(dest)]
    dest.parent.mkdir(parents=True, exist_ok=True)
    run(argv)
    return lines


def build_dovi(key, spec, dur, dest, work):
    """Dolby Vision profile 8.1, synthesized end to end — no donor file involved.

    8.1 means the base layer IS plain HDR10, which is why this can be built at all: encode
    the HDR10 base, have `dovi_tool generate` author an RPU from a JSON description, inject
    it, and let mkvmerge write the container's DV configuration record. ffmpeg's matroska
    muxer will NOT write that record from a raw .hevc, and it is exactly what the app reads
    back (`ff: … dovi=P8 bl_compat=1`). x265 needs aud/hrd/vbv here so the injected RPU NALs land in a conformant
    access-unit structure.
    """
    v = dict(spec["video"], x265_extra="aud=1:hrd=1:vbv-maxrate=50000:vbv-bufsize=50000")
    bl = work / ("%s_bl.hevc" % key)
    lines = build_generic(key, dict(spec, video=v), dur, bl, work, video_only=True)

    cfg = work / ("%s_dovi.json" % key)
    cfg.write_text(json.dumps({
        "cm_version": "V40", "profile": "8.1", "length": int(dur * shape_fps(spec)),
        "level5": {"active_area_left_offset": 0, "active_area_right_offset": 0,
                   "active_area_top_offset": 0, "active_area_bottom_offset": 0},
        "level6": {"max_display_mastering_luminance": 1000,
                   "min_display_mastering_luminance": 1,
                   "max_content_light_level": 1000,
                   "max_frame_average_light_level": 400},
    }))
    rpu = work / ("%s_rpu.bin" % key)
    run(["dovi_tool", "generate", "-j", str(cfg), "-o", str(rpu)])
    bl_rpu = work / ("%s_bl_rpu.hevc" % key)
    run(["dovi_tool", "inject-rpu", "-i", str(bl), "--rpu-in", str(rpu), "-o", str(bl_rpu)])

    a = spec["audio"][0]
    mka = work / ("%s_audio.mka" % key)
    run(["ffmpeg", "-y", "-v", "error", "-nostdin",
         "-t", str(dur), "-f", "lavfi", "-i", audio_graph(a["ch"], a["pitch"], dur)]
        + ["-c:a"] + AENC[a["codec"]] + (["-b:a", a["br"]] if a.get("br") else [])
        + ["-ac", str(a["ch"]), "-strict", "-2", "-t", str(dur), str(mka)])

    # The description tag, which this builder used to skip entirely: build_generic writes
    # it, but the DV path returns from build_generic at the video_only branch long before
    # that code runs, so the ONE file whose picture is hardest to tell apart from its
    # neighbours was also the one with no machine-readable layout in it. mkvmerge writes
    # global tags from an XML document; ffprobe reads them back as format.tags.DESCRIPTION,
    # which is what verify() now compares against layout_lines().
    tags = work / ("%s_tags.xml" % key)
    desc = "\n".join(lines)
    tags.write_text('<?xml version="1.0" encoding="UTF-8"?>\n<Tags><Tag>\n'
                    + "".join("<Simple><Name>%s</Name><String>%s</String></Simple>\n"
                              % (n, _xml_escape(desc)) for n in ("DESCRIPTION", "COMMENT"))
                    + "</Tag></Tags>\n", encoding="utf-8")

    dest.parent.mkdir(parents=True, exist_ok=True)
    argv = ["mkvmerge", "-q", "-o", str(dest),
            "--title", container_title(key, spec), "--global-tags", str(tags),
            "--default-duration", "0:%sfps" % _rate_arg(shape_fps(spec)), "--language", "0:und",
            "--default-track", "0:yes", str(bl_rpu),
            "--language", "0:" + a["lang"], "--track-name", "0:" + a.get("title", ""),
            "--default-track", "0:yes", str(mka)]
    run(argv)
    for p in (bl, bl_rpu, mka, rpu, cfg, tags):
        p.unlink(missing_ok=True)
    return lines


def _xml_escape(s):
    return (s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
             .replace('"', "&quot;"))


# ---------------------------------------------------------------------------------------
# Self-verification. NOT optional, and not a formality: a generator that silently emits the
# wrong shape is worse than no generator at all, because the harness then fails as if the
# PLAYER regressed and the evidence all points at the app. Everything a shape CLAIMS is
# read back out of the finished file; a mismatch names both sides and is fatal.
# ---------------------------------------------------------------------------------------
VCODEC_NAME = {"h264": "h264", "hevc": "hevc", "vp9": "vp9"}
ACODEC_NAME = {"ac3": "ac3", "eac3": "eac3", "aac": "aac", "dts": "dts"}


def _rate(s):
    """ffprobe's rational frame rate ("24/1") as a float; 0.0 when it says nothing."""
    try:
        n, d = (s or "0/0").split("/")
        return float(n) / float(d) if float(d) else 0.0
    except (ValueError, ZeroDivisionError):
        return 0.0


def _frame_pts(frame):
    """Best available presentation timestamp from one ffprobe `-show_frames` object."""
    for key in ("best_effort_timestamp_time", "pts_time", "pkt_pts_time"):
        try:
            return float(frame[key])
        except (KeyError, TypeError, ValueError):
            pass
    return None


def collapse_resolution_frames(frames):
    """Ordered `(width, height, first_pts, key_frame)` runs from decoded video frames."""
    runs = []
    for frame in frames:
        try:
            size = int(frame["width"]), int(frame["height"])
        except (KeyError, TypeError, ValueError):
            continue
        pts = _frame_pts(frame)
        if pts is None:
            continue
        if not runs or runs[-1][:2] != size:
            runs.append((size[0], size[1], pts, bool(int(frame.get("key_frame") or 0))))
    return runs


def _ffprobe_data_bytes(text):
    """Turn ffprobe's offset/hex/ASCII dump for one packet back into bytes."""
    out = bytearray()
    for line in (text or "").splitlines():
        if ":" not in line:
            continue
        # `00000000: 0000 0004 ....  ....` — the double space separates hex from ASCII.
        hex_side = line.split(":", 1)[1].strip().split("  ", 1)[0]
        compact = "".join(hex_side.split())
        if compact and len(compact) % 2 == 0 and re.fullmatch(r"[0-9a-fA-F]+", compact):
            out.extend(bytes.fromhex(compact))
    return bytes(out)


def h264_nal_types(data):
    """NAL types from either Annex-B or the generated Matroska's 4-byte AVCC packets."""
    types = []
    if b"\x00\x00\x01" in data[:8]:
        starts = []
        i = 0
        while i + 3 <= len(data):
            if data[i:i + 4] == b"\x00\x00\x00\x01":
                starts.append(i + 4)
                i += 4
            elif data[i:i + 3] == b"\x00\x00\x01":
                starts.append(i + 3)
                i += 3
            else:
                i += 1
        return [data[p] & 0x1f for p in starts if p < len(data)]
    i = 0
    while i + 4 <= len(data):
        size = int.from_bytes(data[i:i + 4], "big")
        i += 4
        if size <= 0 or i + size > len(data):
            break
        types.append(data[i] & 0x1f)
        i += size
    return types


def boundary_packet_nals(path, boundary_s):
    """NAL types in the key packet nearest one expected resolution boundary."""
    start = max(0.0, boundary_s - 0.20)
    try:
        doc = json.loads(run([
            "ffprobe", "-v", "error", "-select_streams", "v:0",
            "-read_intervals", "%s%%%s" % (_g(start), _g(boundary_s + 0.20)),
            "-show_packets", "-show_data",
            "-show_entries", "packet=pts_time,flags,data", "-of", "json", str(path),
        ]))
    except (Fail, ValueError):
        return []
    candidates = []
    for packet in doc.get("packets", []):
        try:
            pts = float(packet["pts_time"])
        except (KeyError, TypeError, ValueError):
            continue
        if "K" in (packet.get("flags") or ""):
            candidates.append((abs(pts - boundary_s), packet))
    if not candidates:
        return []
    return h264_nal_types(_ffprobe_data_bytes(min(candidates, key=lambda x: x[0])[1].get("data")))


def physical_av_interleave(packets, video_index, audio_index):
    """Maximum PTS separation while walking packets in physical file order.

    Per-track timestamps can be immaculate in a catastrophically ordered Matroska file. The
    player has one bounded queue per lane and a single demux thread, so ten seconds of video blocks
    that thread before it reaches the audio stored later. `packet.pos` is the evidence that matters
    here: sort by byte position explicitly rather than depending on how a particular ffprobe build
    chooses to print packets.
    """
    wanted = {int(video_index), int(audio_index)}
    rows = []
    counts = {int(video_index): 0, int(audio_index): 0}
    for ordinal, packet in enumerate(packets):
        try:
            stream = int(packet["stream_index"])
        except (KeyError, TypeError, ValueError):
            continue
        if stream not in wanted:
            continue
        try:
            pos = int(packet["pos"])
            pts = float(packet["pts_time"])
        except (KeyError, TypeError, ValueError):
            raise ValueError("an A/V packet lacks physical position or presentation timestamp")
        rows.append((pos, ordinal, stream, pts))
        counts[stream] += 1
    if any(counts[index] < 2 for index in wanted):
        raise ValueError("physical packet walk did not find both A/V streams")

    latest = {}
    max_skew = 0.0
    for _pos, _ordinal, stream, pts in sorted(rows):
        latest[stream] = pts
        if wanted <= latest.keys():
            max_skew = max(max_skew, abs(latest[video_index] - latest[audio_index]))
    return max_skew, counts


def late_audio_lead(packets, video_index, audio_index):
    """`(first audio packet's time in s, video bytes in packets that precede it)`, from ffprobe
    packets. Both are properties of the FILE as the demuxer meets it: it reads in file order and
    pushes video until the video lane is full, so what matters is how much video sits ahead of the
    first audio packet, not what the container's metadata says the delay is.

    "Precede" is by presentation time, which for the muxers here equals file order to within one
    interleave step; an audio packet with no timestamp cannot be placed, so it is an error rather
    than a silent zero.
    """
    first_audio = None
    video = []
    for packet in packets:
        try:
            stream = int(packet["stream_index"])
        except (KeyError, TypeError, ValueError):
            continue
        if stream not in (int(video_index), int(audio_index)):
            continue
        try:
            pts = float(packet["pts_time"])
            size = int(packet["size"])
        except (KeyError, TypeError, ValueError):
            raise ValueError("a packet lacks a presentation time or size")
        if stream == int(audio_index):
            first_audio = pts if first_audio is None else min(first_audio, pts)
        else:
            video.append((pts, size))
    if first_audio is None:
        raise ValueError("the file has no audio packets")
    return first_audio, sum(size for pts, size in video if pts < first_audio)


def verify_late_audio(path, spec, streams, rec, problems):
    """Prove the clip really starts its audio late AND queues enough video ahead of it.

    A `delay` the muxer silently dropped, or a bitrate too low to fill the video lane before the
    first audio packet, builds a file that verifies clean on every other axis and then passes on a
    player that has the bug: it never parks the demuxer.
    """
    want = spec["late_audio"]
    vs = [s for s in streams if s["codec_type"] == "video"]
    aus = [s for s in streams if s["codec_type"] == "audio"]
    delays = [a.get("delay") for a in spec.get("audio", [])]
    if len(vs) != 1 or len(aus) != 1 or len(delays) != 1 or not delays[0]:
        problems.append("late_audio shape wants exactly one video stream and one delayed audio "
                        "stream; found %d video, %d audio" % (len(vs), len(aus)))
        return
    try:
        doc = json.loads(run(["ffprobe", "-v", "error", "-show_entries",
                              "packet=stream_index,pts_time,size", "-of", "json", str(path)]))
        first_audio, ahead = late_audio_lead(
            doc.get("packets", []), vs[0]["index"], aus[0]["index"])
    except (Fail, ValueError) as e:
        problems.append("could not read the first audio packet back: %s" % e)
        return
    rec["late_audio"] = {"first_audio_s": round(first_audio, 3), "video_bytes_before_audio": ahead}
    # +-0.1 s: ffmpeg's -itsoffset lands within a packet of the request (an E-AC-3 frame is 32 ms),
    # and a codec priming offset can shift the first timestamp slightly negative.
    if abs(first_audio - delays[0]) > 0.1:
        problems.append("first audio packet at %.3fs, wanted %.3fs (+-0.1) -- the container delay "
                        "did not land" % (first_audio, delays[0]))
    if ahead < want["min_video_bytes_before_audio"]:
        problems.append("%d bytes of video before the first audio packet, wanted at least %d (the "
                        "video lane's cap) -- this clip would not park the demuxer, so it grades "
                        "nothing" % (ahead, want["min_video_bytes_before_audio"]))


def verify_resolution_spike(path, spec, dur, rec, problems):
    """Prove the dynamic raster, boundary AUs and continuous A/V timestamps from the file."""
    try:
        doc = json.loads(run([
            "ffprobe", "-v", "error", "-select_streams", "v:0", "-show_frames",
            "-show_entries", "frame=best_effort_timestamp_time,pts_time,width,height,key_frame",
            "-of", "json", str(path),
        ]))
        frames = doc.get("frames", [])
    except (Fail, ValueError):
        frames = []
    runs = collapse_resolution_frames(frames)
    got_sequence = ["%dx%d" % (w, h) for w, h, _, _ in runs]
    want_sequence = list(spec["resolution_spike"]["sequence"])
    expected_plan = resolution_spike_plan(spec, dur)
    expected_bounds = []
    at = 0.0
    for _, seconds in expected_plan[:-1]:
        at += seconds
        expected_bounds.append(at)
    rec["resolution_spike"] = {
        "sequence": got_sequence,
        "boundaries_s": [round(x[2], 3) for x in runs[1:]],
        "boundary_keyframes": [x[3] for x in runs[1:]],
    }
    if got_sequence != want_sequence:
        problems.append("decoded resolution sequence %s, wanted %s"
                        % (" -> ".join(got_sequence) or "none", " -> ".join(want_sequence)))
    if len(runs) == len(want_sequence):
        fps = float(spec["declare"]["fps"])
        tolerance = max(0.010, 2.0 / fps)
        for i, (run_info, want_at) in enumerate(zip(runs[1:], expected_bounds), 1):
            if abs(run_info[2] - want_at) > tolerance:
                problems.append("resolution boundary %d at %.3fs, wanted %.3fs (+-%.3fs)"
                                % (i, run_info[2], want_at, tolerance))
            if not run_info[3]:
                problems.append("resolution boundary %d does not begin on a keyframe" % i)

        pts = [_frame_pts(f) for f in frames]
        pts = [x for x in pts if x is not None]
        steps = [b - a for a, b in zip(pts, pts[1:])]
        want_step = 1.0 / fps
        rec["resolution_spike"]["decoded_frames"] = len(pts)
        rec["resolution_spike"]["video_max_step_ms"] = round(max(steps, default=0) * 1000, 3)
        if not steps or any(x <= 0 for x in steps):
            problems.append("video presentation timestamps are absent, duplicate or decreasing")
        elif max(abs(x - want_step) for x in steps) > 0.003:
            problems.append("video timestamp step departs from %.3fms (range %.3f..%.3fms)"
                            % (want_step * 1000, min(steps) * 1000, max(steps) * 1000))

        for i, boundary in enumerate(expected_bounds, 1):
            nals = boundary_packet_nals(path, boundary)
            rec["resolution_spike"]["boundary_%d_nals" % i] = nals
            missing = {5, 7, 8} - set(nals)  # IDR, SPS, PPS
            if missing:
                problems.append("resolution boundary %d packet NALs %s; missing %s — SPS/PPS must "
                                "be in-band, not only NEW_EXTRADATA"
                                % (i, nals, sorted(missing)))

    try:
        adoc = json.loads(run([
            "ffprobe", "-v", "error", "-select_streams", "a:0", "-show_packets",
            "-show_entries", "packet=pts_time,duration_time", "-of", "json", str(path),
        ]))
        audio = [(float(p["pts_time"]), float(p.get("duration_time") or 0))
                 for p in adoc.get("packets", []) if p.get("pts_time") not in (None, "N/A")]
    except (Fail, ValueError, KeyError):
        audio = []
    gaps = [b[0] - a[0] for a, b in zip(audio, audio[1:])]
    rec["resolution_spike"]["audio_packets"] = len(audio)
    rec["resolution_spike"]["audio_max_step_ms"] = round(max(gaps, default=0) * 1000, 3)
    if len(audio) < 2 or any(x <= 0 for x in gaps):
        problems.append("AAC timestamps are absent, duplicate or decreasing")
    elif max(gaps) > 0.050:
        problems.append("AAC timestamp discontinuity: maximum packet step %.3fms" % (max(gaps) * 1000))
    elif audio[0][0] > 0.100 or audio[-1][0] + audio[-1][1] < dur - 0.100:
        problems.append("AAC does not span the clip continuously (%.3fs..%.3fs, wanted ~0..%ss)"
                        % (audio[0][0], audio[-1][0] + audio[-1][1], dur))

    # Timestamp continuity alone does not prove that a bounded streaming demuxer can reach both
    # tracks. Check physical packet order as well: a badly muxed file may store most video first
    # and most audio afterwards while every per-track PTS remains perfect.
    try:
        pdoc = json.loads(run([
            "ffprobe", "-v", "error", "-show_packets", "-show_streams",
            "-show_entries", "stream=index,codec_type:packet=stream_index,pts_time,pos",
            "-of", "json", str(path),
        ]))
        by_type = {s.get("codec_type"): int(s["index"]) for s in pdoc.get("streams", [])
                   if s.get("codec_type") in ("video", "audio")}
        max_interleave, packet_counts = physical_av_interleave(
            pdoc.get("packets", []), by_type["video"], by_type["audio"])
    except (Fail, ValueError, KeyError) as e:
        max_interleave, packet_counts = None, {}
        problems.append("cannot verify physical A/V packet interleave: %s" % e)
    rec["resolution_spike"]["physical_packet_counts"] = {
        str(index): count for index, count in sorted(packet_counts.items())
    }
    rec["resolution_spike"]["packet_interleave_max_ms"] = (
        round(max_interleave * 1000, 3) if max_interleave is not None else None)
    if max_interleave is not None and max_interleave > 0.250:
        problems.append("physical A/V packet interleave drifts by %.3fms (maximum 250ms)"
                        % (max_interleave * 1000))


def verify(key, spec, dur, path):
    problems, rec = [], {}
    if not path.exists():
        return {}, ["missing file %s" % path]
    j = probe(path)
    fmt = j.get("format", {})
    rec["size_bytes"] = int(fmt.get("size") or path.stat().st_size)

    measured = float(fmt.get("duration") or 0.0)
    rec["duration_s"] = round(measured, 3)
    # A property of the FILE, not of the invocation that happened to look at it — which is
    # what makes the document's top-level flag derivable rather than a per-run stamp.
    rec["quick"] = measured < spec["duration"] * 0.9
    tol = max(2.0, dur * 0.03)
    if abs(measured - dur) > tol:
        # DUR_LONGER_MARK: main() reads this exact prefix to tell "the file on disk is
        # LONGER than this run asked for" from every other duration mismatch, because the
        # two want opposite handling — a short file is wrong, a long one is a full-length
        # build a later `--quick` run must not silently shorten.
        problems.append("duration %.2fs, wanted %ds (+-%.1f)" % (measured, dur, tol))

    if measured > 0:
        rec["bitrate_mbit"] = round(rec["size_bytes"] * 8 / measured / 1e6, 2)
        # A RATE-TARGETED shape has to land on its target, and this is the only place that can be
        # checked against the file rather than against the ffmpeg arguments. It exists because
        # nothing else notices a `crf` -> `vbr` change: the duration, raster, codec and track
        # layout are all identical either way, so a stale quality-targeted clip would be skipped
        # as "already correct". Measured 2026-08-26: rate-targeted clips land at 0.90-1.14x of
        # target while quality-targeted ones sat at 1.58-1.90x, so the band below separates them.
        want = spec["video"].get("vbr")
        if want:
            want_mbit = (want + sum(int(str(a.get("br", "0k")).rstrip("k") or 0)
                                    for a in spec.get("audio", []))) / 1000.0
            ratio = rec["bitrate_mbit"] / want_mbit if want_mbit else 0
            if not 0.70 <= ratio <= 1.30:
                problems.append("bitrate %.2f Mbit against a %.2f Mbit target (%.2fx) — this "
                                "clip is not encoded to its rate"
                                % (rec["bitrate_mbit"], want_mbit, ratio))

    # The burned-in layout, read back out of the container rather than restated from the
    # spec. fixtures.json used to record `layout` from layout_lines() alone, so it asserted
    # a description the file did not necessarily have — and one builder did not write one
    # at all. Tag case differs by container (matroska uppercases), hence the fold.
    tags = {k.lower(): v for k, v in (fmt.get("tags") or {}).items()}
    # Against the MEASURED length, not the requested one: the layout's duration field is
    # whatever the file was built at, and grading a full-length file during a --quick run
    # must produce exactly ONE complaint (the duration), which is what main() keys the
    # "longer than asked, keep it" rule on.
    want_desc = "\n".join(layout_lines(key, spec, int(round(measured)) or dur))
    got_desc = tags.get("description") or tags.get("comment")
    rec["description_ok"] = (got_desc or "").strip() == want_desc.strip()
    if not rec["description_ok"]:
        problems.append("container description does not match the burned-in layout "
                        "(got %r)" % ((got_desc or "")[:60]))

    streams = j.get("streams", [])
    vs = [s for s in streams if s["codec_type"] == "video"]
    aus = [s for s in streams if s["codec_type"] == "audio"]

    v = spec["video"]
    want_w, want_h = (int(x) for x in v["size"].split("x"))
    if len(vs) != 1:
        problems.append("expected 1 video stream, found %d" % len(vs))
    else:
        s = vs[0]
        rec["video"] = {"codec": s.get("codec_name"), "width": s.get("width"),
                        "height": s.get("height"), "pix_fmt": s.get("pix_fmt"),
                        "color_primaries": s.get("color_primaries"),
                        "color_transfer": s.get("color_transfer"),
                        "color_space": s.get("color_space"),
                        "codec_tag": s.get("codec_tag_string"),
                        "default": (s.get("disposition") or {}).get("default"),
                        "profile": s.get("profile")}
        if v.get("tag") and s.get("codec_tag_string") != v["tag"]:
            # `hvc1` vs `hev1` decides whether some players will touch the file at all, and
            # it is one `-tag:v` away from wrong. A hev1 copy of this shape verified clean.
            problems.append("codec tag %r, wanted %r" % (s.get("codec_tag_string"), v["tag"]))
        if not (s.get("disposition") or {}).get("default"):
            problems.append("video track is not flagged default")
        want_codec = VCODEC_NAME[v["codec"]]
        if s.get("codec_name") != want_codec:
            problems.append("video codec %r, wanted %r" % (s.get("codec_name"), want_codec))
        if (s.get("width"), s.get("height")) != (want_w, want_h):
            problems.append("video %sx%s, wanted %s" % (s.get("width"), s.get("height"), v["size"]))
        want_pix = "yuv420p10le" if v.get("hdr") else "yuv420p"
        if s.get("pix_fmt") != want_pix:
            hint = (" (trap 7: lavfi is rgb24 and libx264 picks yuv444p when nothing in the "
                    "chain says format=)" if "444" in (s.get("pix_fmt") or "") else "")
            problems.append("pix_fmt %r, wanted %r%s" % (s.get("pix_fmt"), want_pix, hint))
        if v["codec"] == "h264" and "Baseline" in (s.get("profile") or ""):
            problems.append("H.264 profile %r — -preset ultrafast forced Constrained "
                            "Baseline (trap 6)" % s.get("profile"))
        if v.get("hdr"):
            for field, want in (("color_transfer", "smpte2084"),
                                ("color_primaries", "bt2020"),
                                ("color_space", "bt2020nc")):
                if s.get(field) != want:
                    problems.append("%s %r, wanted %r (trap 2: the auto-scaler overwrites "
                                    "these unless setparams runs after format=)"
                                    % (field, s.get(field), want))
            sd = first_frame_side_data(path)
            rec["video"]["frame_side_data"] = sd
            for want in ("Mastering display metadata", "Content light level metadata"):
                if want not in sd:
                    problems.append("no in-band %s (x265 repeat-headers=1 did not land)" % want)
        if spec.get("dovi"):
            dv = [x for x in (s.get("side_data_list") or [])
                  if "DOVI" in (x.get("side_data_type") or "")]
            if not dv:
                problems.append("no DOVI configuration record in the container "
                                "(ffmpeg's matroska muxer will not write one — mux with mkvmerge)")
            else:
                rec["video"]["dovi"] = {"profile": dv[0].get("dv_profile"),
                                        "bl_compat": dv[0].get("dv_bl_signal_compatibility_id"),
                                        "el_present": dv[0].get("el_present_flag"),
                                        "bl_present": dv[0].get("bl_present_flag"),
                                        "rpu_present": dv[0].get("rpu_present_flag")}
                if dv[0].get("dv_profile") != 8:
                    problems.append("dv_profile %r, wanted 8" % dv[0].get("dv_profile"))
                if dv[0].get("dv_bl_signal_compatibility_id") != 1:
                    problems.append("dv_bl_signal_compatibility_id %r, wanted 1 (8.1 means "
                                    "an HDR10-compatible base layer)"
                                    % dv[0].get("dv_bl_signal_compatibility_id"))
                # A dual-layer file is not profile 8.1 and the declaration says el_present 0.
                if dv[0].get("el_present_flag"):
                    problems.append("el_present_flag=1 — a dual-layer file, not profile 8.1")
                if not dv[0].get("bl_present_flag"):
                    problems.append("bl_present_flag=0 — no base layer in the record")
                if not dv[0].get("rpu_present_flag"):
                    problems.append("rpu_present_flag=0 — the configuration record claims "
                                    "no RPU, so this is not Dolby Vision")
            # ...and the record is a CLAIM the container makes. The RPU NALs are what the
            # decoder sees: assert they are in the bitstream too.
            sd = rec.get("video", {}).get("frame_side_data") or first_frame_side_data(path)
            rec["video"]["frame_side_data"] = sd
            if "Dolby Vision RPU Data" not in sd:
                problems.append("no in-band Dolby Vision RPU Data on the first frame "
                                "(dovi_tool inject-rpu did not land)")
        if spec.get("resolution_spike"):
            verify_resolution_spike(path, spec, dur, rec, problems)

    want_audio = spec.get("audio", [])
    rec["audio"] = [{"codec": s.get("codec_name"), "channels": s.get("channels"),
                     "language": (s.get("tags") or {}).get("language"),
                     "title": (s.get("tags") or {}).get("title"),
                     "default": (s.get("disposition") or {}).get("default")} for s in aus]
    if len(aus) != len(want_audio):
        problems.append("%d audio streams, wanted %d" % (len(aus), len(want_audio)))
    else:
        for i, (got, w) in enumerate(zip(aus, want_audio)):
            want_name = ACODEC_NAME[w["codec"]]
            if got.get("codec_name") != want_name:
                problems.append("audio[%d] codec %r, wanted %r — ORDER is asserted by the "
                                "lane-selection cases" % (i, got.get("codec_name"), want_name))
            if got.get("channels") != w["ch"]:
                problems.append("audio[%d] %r channels, wanted %d" % (i, got.get("channels"), w["ch"]))
            lang = (got.get("tags") or {}).get("language")
            if lang != w["lang"]:
                problems.append("audio[%d] language %r, wanted %r" % (i, lang, w["lang"]))
            dflt = bool((got.get("disposition") or {}).get("default"))
            if dflt != bool(w.get("default")):
                problems.append("audio[%d] default=%s, wanted %s" % (i, dflt, bool(w.get("default"))))
            # Track titles are checked for MATROSKA only: mp4 does not carry a per-track
            # title through this path, which is why the mp4 shape declares none. A spec
            # field nothing reads back is a claim, and this file had three of them.
            if spec["ext"] != "mp4" and w.get("title"):
                title = (got.get("tags") or {}).get("title")
                if title != w["title"]:
                    problems.append("audio[%d] title %r, wanted %r" % (i, title, w["title"]))

    if spec.get("late_audio"):
        verify_late_audio(path, spec, streams, rec, problems)

    if ss := [s for s in streams if s["codec_type"] == "subtitle"]:
        problems.append("%d subtitle stream(s), and no shape carries one" % len(ss))

    # ---- THE DECLARATION. What `/tmp/dispatcharrtv-playurl` will tell the
    # television this stream IS, read back against what it actually is. The app builds its
    # Load payload from the declaration and never consults the container, so a declaration
    # that has drifted from its own media is invisible everywhere but the panel — an H264
    # payload over an HEVC elementary stream, or an audio lane that matches nothing — and
    # the case then fails pointing squarely at the player.
    dec = spec.get("declare")
    if dec:
        rec["declare"] = dict(dec)
        if vs:
            if vs[0].get("codec_name") != dec["vcodec"]:
                problems.append("declares vcodec %r, the file is %r"
                                % (dec["vcodec"], vs[0].get("codec_name")))
            # The payload's esInfo frame rate — the engine prints it as `fps=` on the `load:`
            # line to three decimals, so a clip built at another rate reads there as a
            # payload bug rather than as a fixture that was never 24p.
            got_fps = _rate(vs[0].get("avg_frame_rate") or vs[0].get("r_frame_rate"))
            if dec.get("fps") and abs(got_fps - dec["fps"]) > 0.01:
                problems.append("declares fps %.3f, the file runs at %.3f"
                                % (dec["fps"], got_fps))
        # `ff.rs::audio_stream_matching` feeds the FIRST audio stream whose
        # `avcodec_get_name` equals the declared codec, case-insensitively — so the
        # declaration is also the lane SELECTOR, and the stream index it lands on is exactly
        # what `a=#<idx>` in the `ff:` line reports. Recorded, because on the multi-audio
        # shape that index is the assertion, and deriving it from the same probe that just
        # graded the track order is how the harness avoids counting streams by hand.
        want_a = (dec.get("acodec") or "").lower()
        lane = next((i for i, s in enumerate(streams)
                     if s.get("codec_type") == "audio"
                     and (s.get("codec_name") or "").lower() == want_a), None)
        if want_a and lane is None:
            problems.append("declares acodec %r and no audio stream is that codec (have %s) "
                            "— the app would silently fall back to av_find_best_stream"
                            % (dec["acodec"], ", ".join(str(s.get("codec_name")) for s in aus)))
        rec["declare_audio_stream"] = lane
        # All three DV fields the trigger carries, against the container's own record. A
        # declared profile the media does not have is the one way the DolbyHdrInfo splice can
        # be exercised with nothing behind it — the payload node would be built and the
        # television would be told about a base layer that is not there.
        want_dv, got_dv = dec.get("dovi") or {}, (rec.get("video") or {}).get("dovi") or {}
        for f in ("profile", "bl_compat", "el_present"):
            if int(want_dv.get(f) or 0) != int(got_dv.get(f) or 0):
                problems.append("declares dovi %s=%d, the container's DV record says %d"
                                % (f, int(want_dv.get(f) or 0), int(got_dv.get(f) or 0)))
        if dec.get("atmos"):
            # No shape can carry Atmos — PIPE_SHAPES says why TrueHD is absent — so this is a
            # declaration with no media under it.
            problems.append("declares atmos, and no shape carries it")
    return rec, problems


# ---------------------------------------------------------------------------------------
# Preflight. Missing OPTIONAL tooling SKIPS its shapes with a printed reason and never
# aborts — the same skip channel `tests/run.py` uses for a missing fixture, and the house
# rule: a partial set that says what it is beats an all-or-nothing failure.
# ---------------------------------------------------------------------------------------
EXPERIMENTAL_ENC = ("this ffmpeg has no %s encoder (experimental; a build can omit it).\n"
                    "            ffmpeg -hide_banner -encoders | grep ' dca'\n"
                    "            Homebrew's ffmpeg normally has it.")

BREW = {
    "ffmpeg": "brew install ffmpeg",
    "libx264": "brew install ffmpeg          # libx264 ships with it",
    "libx265": "brew install ffmpeg          # libx265 ships with it",
    "libvpx-vp9": "brew install ffmpeg          # libvpx ships with it",
    "ac3": "brew install ffmpeg", "eac3": "brew install ffmpeg",
    "aac": "brew install ffmpeg",
    # dca is an EXPERIMENTAL encoder a build can be configured without, and anyone reading
    # this line already has ffmpeg — "brew install ffmpeg" was the thing that just failed
    # them. Name the check instead.
    "dca": EXPERIMENTAL_ENC % "dca",
    "mkvmerge": "brew install mkvtoolnix",
    "dovi_tool": "brew install dovi_tool",
}


def shape_requirements(key, spec):
    encs = set()
    encs.add({"h264": "libx264", "hevc": "libx265", "vp9": "libvpx-vp9"}[spec["video"]["codec"]])
    for a in spec.get("audio", []):
        encs.add(AENC[a["codec"]][0])
    return sorted(encs), list(spec.get("tools", []))


def preflight(keys, shapes):
    print("== preflight")
    have_ffmpeg = shutil.which("ffmpeg") and shutil.which("ffprobe")
    if not have_ffmpeg:
        print("  ffmpeg/ffprobe: MISSING — nothing can be built.\n     %s" % BREW["ffmpeg"])
        raise SystemExit(2)
    encoders = ffmpeg_encoders()
    versions = {
        "ffmpeg": (tool_version("ffmpeg", ["-version"]) or "").replace("ffmpeg version ", ""),
        "mkvmerge": tool_version("mkvmerge"),
        "dovi_tool": tool_version("dovi_tool"),
    }
    print("  ffmpeg      %s" % versions["ffmpeg"])
    for t in ("mkvmerge", "dovi_tool"):
        print("  %-11s %s" % (t, versions[t] or "MISSING"))

    plan, skipped = [], []
    for k in keys:
        spec = shapes[k]
        need_enc, need_tool = shape_requirements(k, spec)
        miss_e = [e for e in need_enc if e not in encoders]
        miss_t = [t for t in need_tool if not shutil.which(t)]
        if miss_e or miss_t:
            why = ", ".join(miss_e + miss_t)
            fix = sorted({BREW.get(x, "install " + x) for x in (miss_e + miss_t)})
            skipped.append((k, why, fix))
        else:
            plan.append(k)
    if skipped:
        print("\n  SKIPPING %d shape(s) for missing tooling:" % len(skipped))
        for k, why, fix in skipped:
            print("    %-32s needs %s" % (k, why))
            for f in fix:
                print("        %s" % f)
    return plan, skipped, versions


def fmt_secs(s):
    return "%d:%02d" % (int(s) // 60, int(s) % 60)


def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Generate the test clips tests/manifest.json plays, from lavfi.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="Serve <out>/%s with tests/serve_fixtures.py; tests/run.py plays it. "
               "See README.md." % SUBDIR)
    ap.add_argument("--out", default=os.environ.get("FIXTURES_OUT")
                    or str(Path.home() / "dispatcharrtv-fixtures"),
                    help="output root (default: $FIXTURES_OUT, else ~/dispatcharrtv-fixtures). "
                         "Never inside the repo. The clips land in <out>/%s." % SUBDIR)
    # `--quick` and `--secs` are one knob under two names, hence the exclusive group: given
    # both, the loser would silently decide the length of the build.
    length = ap.add_mutually_exclusive_group()
    length.add_argument("--quick", action="store_true",
                        help="build every shape at ~%ds so the run takes a couple of minutes. "
                             "Shapes are structurally correct but SHORTER than a case's "
                             "window — development only." % QUICK_SECS)
    length.add_argument("--secs", type=int, metavar="N",
                        help="build every shape at N seconds instead of the length it "
                             "declares. The general form of --quick, and it carries the same "
                             "warning: anything shorter than the shape declares is recorded "
                             "`quick` in fixtures.json and is not suite-valid.")
    # `append`, not a bare string: `--only A --only B` is a natural thing to type and the
    # single-value form silently kept only B. Each value is still a comma list.
    ap.add_argument("--only", action="append", default=[],
                    help="shape keys, comma-separated and/or repeated (see --list)")
    ap.add_argument("--list", action="store_true", help="list the shapes and exit")
    ap.add_argument("--force", action="store_true",
                    help="rebuild even if the output verifies (also the only way to SHORTEN "
                         "an existing full-length file with --quick/--secs)")
    ap.add_argument("--keep-work", action="store_true", help="keep intermediates in <out>/.work")
    args = ap.parse_args(argv)
    if args.secs is not None and args.secs < 1:
        print("--secs must be at least 1", file=sys.stderr)
        return 2

    shapes = PIPE_SHAPES
    # One name for "the length this run builds at", None meaning "whatever each shape says".
    secs = QUICK_SECS if args.quick else args.secs

    if args.list:
        label = Path("<out>") / SUBDIR
        print("%-34s %-9s %s" % ("shape", "duration", "output"))
        for k, spec in shapes.items():
            print("%-34s %8ds  %s" % (k, shape_duration(spec, secs), out_path(label, k, spec)))
        # ...and the tooling report, because `--list` is what the README sends a newcomer to FIRST
        # and "which shapes can this machine actually build" is the only question they have at that
        # point. Without this the table above reads as happy rows to somebody with no
        # `dovi_tool`, who then learns otherwise partway into a build.
        print()
        preflight(list(shapes), shapes)
        return 0

    root = Path(args.out).expanduser()
    if root.name != SUBDIR:
        # The `root.name != SUBDIR` test makes the append IDEMPOTENT, and that is not defensive
        # habit — this seam has two sides. `make fixtures-pipeline` passes
        # `$(FIXTURES_OUT)/pipeline`, while `tests/run.py` and `tests/serve_fixtures.py` both
        # default their root to `$FIXTURES_OUT/pipeline`. Appending blind once wrote the pack to
        # `<out>/pipeline/pipeline`, and every case then skipped with "no fixture … run `make
        # fixtures-pipeline`" — the suite telling you to re-run the command that had just
        # succeeded. Accepting BOTH spellings of the same directory keeps that from recurring.
        root = root / SUBDIR
    root = root.resolve()
    try:
        root.relative_to(REPO_ROOT)
    except ValueError:
        pass
    else:
        print("refusing to write media inside the repository (%s).\n"
              "Pick an --out outside %s." % (root, REPO_ROOT), file=sys.stderr)
        return 2

    wanted = list(shapes)
    if args.only:
        wanted = []
        for chunk in args.only:
            for name in [x.strip() for x in chunk.split(",") if x.strip()]:
                if name not in shapes:
                    print("unknown shape %r (see --list)" % name, file=sys.stderr)
                    return 2
                if name not in wanted:
                    wanted.append(name)

    plan, skipped, versions = preflight(wanted, shapes)
    if not plan and not skipped:
        print("\nnothing to build.")
        return 2

    total_secs = sum(shape_duration(shapes[k], secs) * shapes[k]["rate"] for k in plan)
    total_bytes = sum(shape_duration(shapes[k], secs) * PIPE_MBIT[k] / 8 * 1e6 for k in plan)
    tag = " [QUICK]" if args.quick else (" [%ds]" % secs if secs is not None else "")
    print("\n== plan: %d shape(s)%s -> %s" % (len(plan), tag, root))
    print("   estimated encode time ~%s, estimated size ~%.2f GB"
          % (fmt_secs(total_secs), total_bytes / 1e9))
    # An EMPTY plan is not an error and does not return early: everything asked for was
    # skipped for missing optional tooling, which is the documented skip contract ("it
    # never aborts") — and the run still has work to do, because the records those shapes
    # left in an earlier fixtures.json have to be re-checked and dropped if they no longer
    # describe what is on disk. Returning here left a document claiming `dovi: {profile: 8}`
    # over a file somebody had since replaced, on the machine least able to notice.
    if not plan:
        print("   (every shape asked for was skipped — re-checking the existing records)")
    if secs is not None and any(secs < shapes[k]["duration"] for k in plan):
        print("   %ds clips: shorter than the length these shapes declare, so every one of\n"
              "   them is recorded `quick` in fixtures.json. Development media — a clip that\n"
              "   hits EOF inside a case's window ends the session under it. Do not read a\n"
              "   run against it as a player verdict." % secs)

    work = root / ".work"
    work.mkdir(parents=True, exist_ok=True)
    manifest_path = root / "fixtures.json"
    doc = {}
    if manifest_path.exists():
        try:
            doc = json.loads(manifest_path.read_text())
        except ValueError:
            doc = {}
    shapes_doc = doc.get("shapes", {})

    built, reused, failed = [], [], []
    t0 = time.time()
    for k in plan:
        spec = shapes[k]
        dur = shape_duration(spec, secs)
        dest = out_path(root, k, spec)
        label = "%-34s %5ds" % (k, dur)
        if dest.exists() and not args.force:
            rec, problems = verify(k, spec, dur, dest)
            # LONGER than this run asked for is not a defect: it is a full-length file and a
            # later `--quick` run must not silently shorten it. `--force` still shortens.
            longer = ((rec.get("duration_s") or 0) > dur
                      and problems and all(p.startswith("duration ") for p in problems))
            if not problems or longer:
                print("  skip  %s  (%s)"
                      % (label, "already correct" if not problems else
                         "%.0fs on disk, longer than asked — kept" % rec["duration_s"]))
                rec["path"] = str(dest.relative_to(root))
                rec["layout"] = layout_lines(k, spec, int(round(rec["duration_s"])))
                shapes_doc[k] = rec
                reused.append(k)
                continue
            print("  redo  %s  (%s)" % (label, problems[0]))
        print("  build %s ..." % label, end="", flush=True)
        ts = time.time()
        try:
            if spec.get("builder") == "dovi":
                lines = build_dovi(k, spec, dur, dest, work)
            elif spec.get("builder") == "resolution_spike":
                lines = build_resolution_spike(k, spec, dur, dest, work)
            else:
                lines = build_generic(k, spec, dur, dest, work)
        except Fail as e:
            print(" FAILED")
            print("        %s" % str(e).replace("\n", "\n        "))
            # Drop any record a PREVIOUS run left for this key: fixtures.json is read as ground
            # truth by the harness, and a stale entry describing a file that is now half-written
            # is worse than no entry.
            shapes_doc.pop(k, None)
            failed.append((k, str(e).splitlines()[0]))
            continue
        rec, problems = verify(k, spec, dur, dest)
        if problems:
            print(" WRONG SHAPE")
            for p in problems:
                print("        - %s" % p)
            shapes_doc.pop(k, None)
            failed.append((k, problems[0]))
            continue
        print(" ok  %s  %.0f MB" % (fmt_secs(time.time() - ts), rec["size_bytes"] / 1e6))
        rec["path"] = str(dest.relative_to(root))
        rec["layout"] = lines
        shapes_doc[k] = rec
        built.append(k)

    if not args.keep_work:
        shutil.rmtree(work, ignore_errors=True)

    # ---- Records this run did not touch. fixtures.json accumulates ACROSS runs — a `--only`
    # run, or a run on a machine without dovi_tool, leaves every other key in the document. A
    # carried-forward record pointing at a deleted or replaced file describes media that is not
    # that shape, and the harness then fails as if the player had regressed. Re-verify each one
    # (~50 ms) and drop the ones that no longer hold — which also covers the tool-SKIPPED
    # shapes, whose whole point is that their file may be stale from an earlier build.
    touched = set(built) | set(reused) | {x for x, _ in failed}
    for k in sorted(shapes_doc):
        if k in touched:
            continue
        spec2 = shapes.get(k)
        if spec2 is None:
            print("  drop  %-34s (no such shape any more)" % k)
            shapes_doc.pop(k, None)
            continue
        dest2 = root / shapes_doc[k].get("path", "")
        if not shapes_doc[k].get("path") or not dest2.exists():
            print("  drop  %-34s (file is gone)" % k)
            shapes_doc.pop(k, None)
            continue
        # Graded at ITS OWN length — the length the RECORD says this file was built at, so a
        # shortened clip carried through a full-length run is still correct and vice versa, and
        # a file somebody has since REPLACED still fails on the duration it no longer has.
        dur2 = shapes_doc[k].get("duration_s")
        if not dur2:
            # A record from before duration_s existed, or one hand-edited: ask the file.
            try:
                dur2 = float(probe(dest2)["format"]["duration"])
            except (Fail, KeyError, ValueError):
                dur2 = spec2["duration"]
        rec2, probs2 = verify(k, spec2, dur2, dest2)
        if probs2:
            print("  drop  %-34s (%s)" % (k, probs2[0]))
            shapes_doc.pop(k, None)
            continue
        rec2["path"] = shapes_doc[k]["path"]
        rec2["layout"] = layout_lines(k, spec2, round(rec2["duration_s"]))
        shapes_doc[k] = rec2

    # `quick` is a property of each FILE (verify() derives it from the measured duration),
    # never of the invocation. Stamped per-run over a cross-run document it could say `false`
    # over a set that was still 20 s clips.
    quicks = {bool(r.get("quick")) for r in shapes_doc.values()}
    quick_flag = "mixed" if len(quicks) > 1 else (quicks.pop() if quicks else bool(args.quick))

    doc = {
        "generator": "tests/fixtures/make_fixtures.py",
        "generator_version": GEN_VERSION,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "quick": quick_flag,
        "skipped": {k: why for k, why, _ in skipped},
        "tool_versions": versions,
        "synthetic": True,
        "_comment": ("Shape key -> the file that shape was built as, plus every property "
                     "verify() read back out of it. Each file is served over HTTP by "
                     "tests/serve_fixtures.py, and its `declare` block is the playback half of "
                     "the /tmp/dispatcharrtv-playurl trigger, verified here against the media "
                     "it describes. Synthetic media: a green run against it is a no-regression "
                     "claim about the shapes the suite names, not a claim about real-world "
                     "media. Every record here was verified against the file on disk during "
                     "the run that wrote this document, including the ones the run did not "
                     "rebuild. `quick` per shape means that file is SHORTER than its shape "
                     "declares (a --quick or --secs development clip) and is NOT suite-valid; "
                     "the top-level `quick` is true/false/\"mixed\" over the whole set."),
        "shapes": shapes_doc,
    }
    manifest_path.write_text(json.dumps(doc, indent=2, sort_keys=False) + "\n")

    print("\n== done in %s" % fmt_secs(time.time() - t0))
    print("   built %d, reused %d, failed %d, skipped %d"
          % (len(built), len(reused), len(failed), len(skipped)))
    for k, why in failed:
        print("   FAILED %-32s %s" % (k, why))
    for k, why, _ in skipped:
        print("   SKIP   %-32s needs %s" % (k, why))
    if quick_flag == "mixed":
        print("   NB this set MIXES quick and full-length files (see the per-shape `quick`\n"
              "      flag in fixtures.json). It is not suite-valid as it stands.")
    print("   manifest: %s" % manifest_path)
    if built or reused:
        # The next thing that touches these files is a static file server on this machine. The
        # port is the one serve_fixtures.py documents as its own default; the harness picks its own.
        print("\n   Next: serve this directory to the television —")
        print("     ./tests/serve_fixtures.py --root %s --port 8020" % root)
        print("   Each case then arms /tmp/dispatcharrtv-playurl with a URL into that server")
        print("   plus the `declare` block recorded beside the file in fixtures.json.")
        print("   Start it ONCE with a human at the keyboard: the macOS application")
        print("   firewall drops the TV's connections to a new python listener silently.")
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)

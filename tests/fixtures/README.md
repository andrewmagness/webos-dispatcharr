# `tests/fixtures` — synthesizing the clips the harness plays

`tests/run.py` plays short **generated clips** on the television (see `tests/README.md`), and
`make_fixtures.py` builds them from `lavfi` sources with nothing but ffmpeg (plus
`mkvmerge` and `dovi_tool` for the Dolby Vision clip). It **reads every finished file back and
checks the properties the cases grade** — codecs, resolution, bit depth, colour transfer and
primaries, audio track order and language tags, Dolby Vision RPU presence, duration — and writes a
`fixtures.json` describing what it proved.

```
make fixtures-pipeline
# the same thing:  python3 tests/fixtures/make_fixtures.py --out ~/dispatcharrtv-fixtures/pipeline
```

About 0.9 GB into `$FIXTURES_OUT/pipeline` (default `~/dispatcharrtv-fixtures/pipeline`) and a few
minutes. The output root may not be inside the repository: this repository is public, and media
must never be committed. Generating media is a deliberate act — no `make` target that builds or
checks the app runs it, and no television is involved.

---

## The honest caveat, up front

**A green run against synthetic media is not the same claim as a green run against real channels.**
These clips exercise the same code paths — demux, the Starfish payload, the video-plane bind, the
position climb — but a real broadcast carries encoder quirks, odd GOP structures and stream changes
this generator will never produce. Read a pass here as *"no regression against the shapes the cases
name"*, never as *"plays every channel"*.

---

## Install the tooling

```sh
brew install ffmpeg          # or your distribution's ffmpeg — needs libx264, libx265, libvpx-vp9 and the ac3/eac3/aac/dca encoders
brew install mkvtoolnix      # optional — only the Dolby Vision clip
brew install dovi_tool       # optional — only the Dolby Vision clip
```

The preflight names every missing encoder, the shapes each one costs, and how to fix it:

```sh
python3 tests/fixtures/make_fixtures.py --list
```

**Missing optional tooling skips its shapes and prints why — it never aborts**, and it exits 0 even
when every shape asked for was skipped. That is the same skip channel `tests/run.py` uses for a clip
the pack does not hold: a partial set that says what it is beats an all-or-nothing failure.

**`dca` (the DTS encoder) is EXPERIMENTAL**, and a build can be configured without it; the skip
message then does not say "install ffmpeg". Check with
`ffmpeg -hide_banner -encoders | grep ' dca'`.

## Flags

| flag | |
|---|---|
| `--out DIR` | output root; default `$FIXTURES_OUT`, else `~/dispatcharrtv-fixtures` — the same variable the Makefile and `tests/run.py` read. Refuses any path inside the repo. The pack goes in a `pipeline/` subdirectory, and naming that subdirectory yourself is idempotent. |
| `--secs N` | override every shape's duration. A case that needs to climb further than the clip is **skipped with the reason named**, not failed — the failure it would otherwise produce reads exactly like a player regression. |
| `--quick` | every shape at ~20 s. **Development only**; every record it writes is stamped `quick: true`. |
| `--only K[,K]` | build a subset; comma-separated **and/or repeated**. |
| `--list` | shapes, durations, output paths, and the tooling report. |
| `--force` | rebuild even when the existing output verifies. |
| `--keep-work` | keep `<out>/.work` (banners and DV intermediates). |

Exit code is nonzero if any shape failed to build or failed its own verification. Missing optional
tooling is **not** a failure at any count.

---

## For maintainers

**Self-verification is the load-bearing part.** After each build, ffprobe reads back everything the
shape claims — duration, codec, resolution, pixel format, codec tag (`hvc1` vs `hev1`), colour
primaries/transfer/matrix, in-band mastering-display and content-light SEI, the DV configuration
record plus the in-band **RPU NALs**, and the audio track list *in order* with channels, language
and default flags. **A shape that fails its own check is a hard error naming both sides**, and the
file is not recorded in `fixtures.json`. A generator that silently emits the wrong shape is worse
than no generator: the harness then fails as if the *player* regressed, and every piece of evidence
points at the app.

**`fixtures.json` is re-verified end to end on every run**, including the records the run did not
rebuild: every untouched key is re-verified at its own length and dropped (with the reason printed)
if it no longer holds.

The module docstring carries the full trap list the code is built around — each one a silent
failure — including why `-t` goes before `-i`, why `setparams` must follow `format=`, and why
`testsrc2` and not `testsrc`.

### DTS core packet regression artifact

`dts-core-frame.dts` is the first 1,884-byte audio packet of the generated `pipe_h264_dts_1080p`
fixture (48 kHz, DTS core 5.1, synthetic tone). Generate the fixture with
`make_fixtures.py --only pipe_h264_dts_1080p`, then extract with
`ffmpeg -i pipe_h264_dts_1080p.mkv -map 0:a:0 -c:a copy -frames:a 1 -f dts dts-core-frame.dts`.
The host regression grades exact preservation and extension stripping. Many 2020+ LG sets cannot
decode DTS at all — the dev C1 (2021) refuses the clip's Load with `Resource Allocation Error` —
so on such a set `pipe_h264_dts_1080p` fails on the television's verdict, not the harness's.

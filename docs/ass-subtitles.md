# Native ASS and SSA subtitles

An ASS/SSA track embedded in the stream the app demuxes is drawn by the app with libass, preserving
the authored script: styles, positioned signs, overlapping dialogue, drawings, font attachments,
movement and karaoke. Plain-text captions follow the simple caption size and wrapping rules; ASS/SSA
retains its authored layout.

*History:* this pipeline was built for the PlxNative code this project grew from, where films carried
ASS tracks routinely. A live channel rarely does; the code stays because the demuxer hands it any
ASS track a stream carries, and the clip harness (`tests/run.py`) can still exercise it.

`ci/build-libass.py` builds a pinned libass with private FreeType, FriBidi and HarfBuzz. The
application loads its own `libass-plx` by absolute path. Only the versioned `plx_ass_*` facade is
exported; dependency symbols cannot collide with the firmware's font stack. `src/ass.c` reads
libass structures through the same headers used to compile that library. No firmware libass or
new Starfish API is required. Pins, checksums, license notices and corresponding source archives
are part of the normal package/source-bundle workflow.

The demuxer copies ASS headers, complete timed packets and font attachments out of FFmpeg.
`Shared` owns the ASS source store and renderer mailbox alongside the other playback transport
state. `player::ass_source` retains immutable snapshots and a bounded event window, including the
history needed for subtitle delay. It keeps all embedded ASS tracks so selecting another language
can use the already-read portion of the file. A seek changes source identities while retaining known
events, headers and fonts: Matroska does not resend earlier signs spanning the seek target.
Reread packets are deduplicated, and pruning follows a backward seek before the native clock
rebases; a new playback discards the previous media's sources.
Reload-based seeks and demux reopens also retain known events, after revalidating the exact
delivery identity and complete ASS header inventory. A different file or a new playback cannot
inherit them. This matters when an in-place seek falls back to a fresh native Load.
Reaching demux EOF does not discard subtitles while queued video still plays.

`player::ass` owns the native objects on one worker. A single latest-request slot coalesces clock
updates; a completed frame is accepted only for the current source/selection epoch. The UI never
waits for font parsing or rasterization. Native change detection avoids work for unchanged output. Overlapping libass images are composed
in order into disjoint regions, with a maximum of 64 regions and one bounded pixel arena.
Consecutive libass image identities also retain native region pixels, so a moving or karaoke
cue does not repeatedly blend unrelated static signs. A bounded image-key table tracks that reuse;
changed geometry, image order, clipping or content invalidates the affected regions.
Unchanged region pixels retain their shared allocation, including across translations, so the
player screen’s `ass_subtitles` cache reuses their GL uploads. Changed regions recycle the
remaining textures; gaps, Off and unmount release them. The cache reports all retained texture
bytes to the shared render accounting.

The subtitle clock interpolates between the pipeline's sparse position callbacks, with at most
250 ms of extrapolation. Pause, seek and discontinuities re-anchor it. Output dimensions and the
original coded video dimensions are passed separately to libass. The output canvas fits the
picture's display aspect, including `sourceInfo`'s pixel aspect ratio, and is offset inside the
video window to account for pillarboxing or letterboxing. Authored subtitles are not moved when
the transport HUD appears.

Verification commands:

```sh
make check
make check-ass
CARGO_INCREMENTAL=0 cargo +nightly check --manifest-path rust-modules/Cargo.toml \
  --lib --no-default-features
make
```

The native host test checks actual libass pixels; ordinary host tests check source preservation,
selection/seek fencing, overlap and bounds. Neither proves TV performance. On the television, a
styled-subtitle clip is played through the debug build's `dispatcharrtv-playurl` trigger, which is
how the clip harness (`tests/run.py`, `tests/README.md`) plays every synthetic case.

Device checks must hold the TV lease and keep both the panel and sound off. Release the lease
while building or reviewing captures. Compare the unchanged
build and the candidate on the same media: styles and overlap, moving/karaoke cues, track changes,
Off, pause, backward/forward seek, external ASS and plain subtitles. Capture and inspect the
output, and compare the UI heartbeat and hardware displayed-frame counters with capture disabled
during performance measurement.

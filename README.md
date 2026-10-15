# Dispatcharr TV

A native live-TV client for LG webOS televisions, for [Dispatcharr](https://github.com/Dispatcharr/Dispatcharr)
servers. Sign in to your Dispatcharr server from the TV, browse a programme guide of your channels,
and watch them on the TV's own hardware decoder — no web view, no browser, no transcoding unless you
ask your server for it.

*An independent project, not affiliated with Dispatcharr or LG.*

## The goal

webOS has no good way to watch a self-hosted IPTV lineup. Dispatcharr already does the hard work on
the server — channels, EPG, stream proxying — so this app aims to be the living-room end of it:

- **Setup on the TV.** The TV finds your Dispatcharr on the network and fills in its address; you
  enter your username and password once, and the TV keeps an API key, never your password.
- **A real guide.** Every channel against time, with what's on now and next, logos, favourites,
  your Dispatcharr channel groups and the coloured remote keys.
- **Live playback that feels like a TV.** Channel up/down, number entry, last-channel recall and a
  now/next banner, played through LG's native media pipeline.

## Using it

On the **Guide**, OK on any programme plays its channel; **hold OK** on a channel to make it a
favourite (a star marks it), and the **Favourites** switch at the top shows only those. The
**All groups** switch beside it steps through your Dispatcharr channel groups, one per press, then
back to all of them; the two combine (your favourites in one group). While watching:

| Remote | What it does |
|---|---|
| CH▲ / CH▼ | the next or previous channel in the list you came from |
| number keys (the Magic Remote's 123) | that channel, as soon as the number can only mean one |
| OK, or UP / DOWN | show the banner: what's on now and its progress, with what's up next on the right (OK again hides it) |
| hold OK | back to the channel you watched before |
| BACK | the Guide |

The app reopens on the last channel you watched; *Account* → *Start on the last channel* turns that
off.

## Status

Early and under active development. Nothing here has been released yet; the first release will be
0.1.0.

| Milestone | State |
|---|---|
| M0 — fork, rebrand, x86_64 Linux build environment | done |
| M1 — Dispatcharr client and the Setup screen | done |
| M2 — live MPEG-TS playback on the television | done — H.264/AAC channels play on the TV's hardware decoder |
| M3 — the programme guide | done — grid, logos, OK to tune |
| M4 — player overlay, channel up/down, favourites, recall | done — channel up/down, number entry, now/next banner, hold-OK recall, favourites, start on the last channel |
| M5 — codec coverage, error handling, packaging and release | in progress — dropped or frozen streams reconnect, a busy server is retried, a channel's real codecs are detected and declared (H.264/HEVC video; AAC, AC-3, E-AC-3, DTS and MP2/MP3 audio); an MPEG-2 video channel says how to fix it in Dispatcharr |

Development targets a webOS 6 set. Features are verified in the desktop simulator against a mock
Dispatcharr server (`tests/mock_dispatcharr.py`), and on the television against a real Dispatcharr
server.

## Building

The app is Rust (with a small C shim) cross-compiled to 32-bit ARM. [`docs/building.md`](docs/building.md)
covers the toolchain and the test loop; it is inherited from PlxNative and still being adapted.
`make check` runs the host test suite and lint gates, and `make sim-linux` builds the desktop
simulator.

## Based on PlxNative

This project is a standalone derivative of [PlxNative](https://github.com/GLinnik21/plx-native), a native
Plex client for webOS by Gleb Linnik. Its rendering, UI system, focus engine and native player are
what this app is built on. The Plex-specific screens are still in the tree and are being removed as
their Dispatcharr replacements land.

## Licence

[GPL-3.0-or-later](LICENSE), as PlxNative is; PlxNative's copyright and notices are retained. The
PlxNative name and brand artwork are not part of this project ([`TRADEMARKS.md`](TRADEMARKS.md)).
Third-party components and their licences are in [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md)
and `licenses/`, including the bundled LGPL build of FFmpeg.

Security problems: report them privately as [`SECURITY.md`](SECURITY.md) describes.

"Dispatcharr", "LG" and "webOS" are the names of their respective owners and are used here only to
say what this app works with.

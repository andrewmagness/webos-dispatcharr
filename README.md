# Dispatcharr TV

A native live-TV client for LG webOS televisions, for [Dispatcharr](https://github.com/Dispatcharr/Dispatcharr)
servers. Sign in to your Dispatcharr server from the TV, browse a programme guide of your channels,
and watch them on the TV's own hardware decoder — no web view, no browser, no transcoding unless you
ask your server for it.

*An independent project, not affiliated with Dispatcharr or LG.*

## What it does

webOS has no good way to watch a self-hosted IPTV lineup. Dispatcharr already does the hard work on
the server — channels, EPG, stream proxying — so this app is the living-room end of it:

- **Setup on the TV.** The TV finds your Dispatcharr on the network and fills in its address; you
  enter your username and password once, and the TV keeps an API key, never your password.
- **A real guide.** Every channel against time, with what's on now and next, logos, favourites and
  your Dispatcharr channel groups.
- **Live playback that feels like a TV.** Channel up/down, number entry, last-channel recall, a
  now/next banner you can browse other channels in, played through LG's native media pipeline.

## Installing

You need an LG webOS TV with **Developer Mode** turned on (no root needed), a Dispatcharr server the
TV can reach on your network, and a computer on the same network for the one-time install.

1. Turn on Developer Mode on the TV, and connect to it from your computer with
   [webOS Dev Manager](https://github.com/webosbrew/dev-manager-desktop/releases/latest).
2. Download `com.magness.dispatcharrtv_X.Y.Z_arm.ipk` from the
   [latest release](https://github.com/andrewmagness/webos-dispatcharr/releases/latest) and check
   it against the release's `ipk.sha256`.
3. In Dev Manager, open **Apps** → **Install** and choose the `.ipk`. (On a rooted TV with the
   Homebrew Channel, connect Dev Manager over its SSH option instead; the install is the same.)
4. Open **Dispatcharr TV** on the TV. Setup finds your server on the network (or type its address,
   such as `http://192.168.1.10:9191`); sign in with your Dispatcharr username and password.

To update, install the newer `.ipk` over the old one; your sign-in and favourites are kept.

**Developer Mode expires** unless you extend it in the Developer Mode app, and when it lapses LG
removes the apps installed through it. [Installing and checking a download](docs/install-and-verify.md)
walks through every step with the details, the hash check on each platform and how to keep Developer
Mode alive; [troubleshooting](docs/troubleshooting.md) covers sign-in, guide and playback problems.

## Using it

On the **Guide**, OK on any programme plays its channel; **hold OK** on a channel to make it a
favourite (a star marks it), and the **Favourites** switch at the top shows only those. The
**All groups** switch beside it steps through your Dispatcharr channel groups, one per press, then
back to all of them; the two combine (your favourites in one group). While watching:

| Remote | What it does |
|---|---|
| CH▲ / CH▼ | the next or previous channel in the list you came from |
| number keys (the Magic Remote's 123) | that channel, as soon as the number can only mean one |
| OK | show the banner: what's on now and its progress, with what's up next on the right (OK again hides it) |
| UP / DOWN | look through the other channels in the banner, each with its logo and what's on, without leaving the one playing; UP goes toward higher numbers. OK changes to the channel shown, BACK returns to the one playing, and if you leave it alone the banner goes away and you keep watching |
| hold OK | back to the channel you watched before |
| BACK | the Guide, with the channel still playing in its top-right corner; OK on it (focus lands there) goes back to it full screen, OK on any other programme changes to that channel, and BACK on the Guide stops it |
| STOP | stop watching and go to the Guide |

The app reopens on the last channel you watched; *Account* → *Start on the last channel* turns that
off. Switch to another app and back and the channel you were watching tunes again, live.

## Compatibility

0.1.0 is the first release. [What's in each release](https://github.com/andrewmagness/webos-dispatcharr/releases),
including its known issues, is in its release notes.

| Television | webOS | Notes |
|---|---|---|
| LG OLED C1 (2021, OLED65C1PUB) | 6.5.3 | the development set: every feature is verified here against a real Dispatcharr server |
| LG OLED B8 (2018, OLED55B8PUA) | 4.4.3 | Setup found the Dispatcharr server on the network on its own; the guide, live playback and the guide's corner picture all work and perform well (owner-tested on 0.2.0, 2026-10-31) |

The app starts on every firmware from webOS 3.4.0 up (checked against each firmware image's
libraries, which says nothing about video), and does not start on webOS 2 or older. Video has been
watched only on the two sets above: if you try another, [open an issue](https://github.com/andrewmagness/webos-dispatcharr/issues)
with your model and webOS version, whether it works or not. Channels with MPEG-2 video do not play,
and many 2020-and-later sets cannot decode DTS audio; a Dispatcharr stream profile with H.264 video
and AAC or AC-3 audio fixes either.

Both test sets happen to be rooted; the app itself does not need root. Features are also verified in
the desktop simulator against a mock Dispatcharr server (`tests/mock_dispatcharr.py`).

### Privacy

The app has no crash reporting or analytics and sends nothing to its developer: it talks only to
your Dispatcharr server. [`PRIVACY.md`](PRIVACY.md) lists what is stored on the television.

## Building

The app is Rust (with a small C shim) cross-compiled to 32-bit ARM. [`docs/building.md`](docs/building.md)
covers the toolchain, the two installs (the stable app and a developer build beside it) and the test
loop. `make check` runs the host test suite and lint gates, `make` builds the television's binary,
and `make sim-linux` builds the desktop simulator.

## Based on PlxNative

This project is a standalone derivative of [PlxNative](https://github.com/GLinnik21/plx-native), a native
Plex client for webOS by Gleb Linnik. Its rendering, UI system, focus engine and native player are
what this app is built on. The Plex pages, plex.tv sign-in, the Settings pages, the Plex
documentation tooling, the on-demand player's transport and panels, the trailer preview engine,
the Plex transcode machinery (seek and quality flights, Original trials, the film-resume machine),
the Plex adaptive-bitrate and HLS playback engine, the Plex play resolve, timeline reporting
and subtitle routing (side reader, sidecars, server burns), the Plex library data stores
(Home, Library, Detail, Search, Person, Collection, watch state, subtitle search), the Plex player
settings, the crash and analytics reporting, and the Plex client library itself (the typed Plex
API, its sign-in and server registry, and the poster art path) have been removed. Engineering
records under `docs/` that came from PlxNative — measured on its development television, about
mechanisms this app still uses — say so at the top and name what still applies.

## Licence

[GPL-3.0-or-later](LICENSE), as PlxNative is; PlxNative's copyright and notices are retained. The
PlxNative name and brand artwork are not part of this project ([`TRADEMARKS.md`](TRADEMARKS.md)).
Third-party components and their licences are in [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md)
and `licenses/`, including the bundled LGPL build of FFmpeg.

Security problems: report them privately as [`SECURITY.md`](SECURITY.md) describes.

"Dispatcharr", "LG" and "webOS" are the names of their respective owners and are used here only to
say what this app works with.

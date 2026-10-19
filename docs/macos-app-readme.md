# Dispatcharr TV for macOS

A native live-TV client for Dispatcharr servers, built for LG webOS televisions, running here as a
real Mac app. Everything it
needs is inside the bundle — there is nothing to install.

## Open it (the first time only)

macOS will refuse to open it on the first try: the app is signed, but not by a registered Apple
developer, and Apple charges for that. This is normal and you only do it once.

1. Drag **DispatcharrTV.app** to your Applications folder (or anywhere you like).
2. **Right-click it → Open**, then click **Open** in the dialog.
   *If macOS says the app "is damaged and can't be opened", that is the download quarantine, not
   the app. Open Terminal and run:*
   `xattr -dr com.apple.quarantine /Applications/DispatcharrTV.app`
3. Say **Allow** when macOS asks about finding devices on your local network — the app talks to
   your Dispatcharr server directly, and cannot work without it.

After that it opens by double-click like anything else.

## Sign in

Setup looks for a Dispatcharr server on your network and fills in its address; you can also type
it. Sign in with your Dispatcharr username and password. The app keeps the API key the server
issues, never your password, and stays signed in.

## Getting around

It is a TV interface, so it is driven like one — with arrow keys, not the mouse.

| Key | Does |
|---|---|
| ↑ ↓ ← → | Move focus |
| Return | Open / select |
| Esc or Delete | Back |
| Cmd-Q | Quit |

Clicking works too, on most screens.

## What is not here: video

**Playback does not work in this build, and pressing Play will tell you so** with a full-screen
message. That is honest rather than broken: the decoding is done by the television's own hardware
video pipeline (LG's StarfishMediaAPIs, bound to the TV's video plane), which does not exist on a
Mac. Everything else is the real thing — the same interface, the same code, talking to your real
server: Setup, the programme guide with its channel logos, groups and favourites.

So: this is the app to *look at* and click around. It is not a way to watch anything.

## Small print

Apple Silicon Macs only (M1 and later), macOS 11 or newer.

Unofficial, and not affiliated with, endorsed by, or sponsored by the Dispatcharr project or LG
Electronics. Licensed under the GPL, version 3 or later; the licence and the third-party notices are
inside the app bundle, under `Contents/Resources`.

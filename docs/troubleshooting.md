# Troubleshooting

Haven't installed Dispatcharr TV yet? Start with the [installation guide](install-and-verify.md).
This page is about what to do when something doesn't work.

## The app tile does nothing

Dispatcharr TV starts on every firmware from webOS 3.4 up that it has been checked against, and on
nothing older: on webOS 2 and earlier the television stops it before anything appears, so the tile
just does nothing when you select it. See the [README's compatibility section](../README.md#compatibility) for
which televisions have actually been watched playing.

## Developer Mode removed the app

LG Developer Mode expires and removes every app it installed unless you renew the session before
it lapses. [Renew it in the Developer Mode app](install-and-verify.md#important-developer-mode-expires),
or reinstall Dispatcharr TV if the session already expired.

## Setup can't sign in

Setup looks for a Dispatcharr server on your network and fills in the address it finds; if it finds
none, type the address (for example `192.168.1.10:9191`). Setup then says what stopped it:

- **"Nothing answered at that address."** Check the address, that the server is running, and that
  the TV can reach it — a server reachable only through a VPN on your computer is not reachable from
  the TV.
- **"Something answered at that address, but it is not Dispatcharr."** The address or port points at
  another service. Dispatcharr listens on port 9191 unless you changed it.
- **"Wrong username or password."** Use the account you sign in to Dispatcharr's web interface with.
- **"The server refused this TV."** Dispatcharr's network access settings do not allow the TV's
  address. Allow it in Dispatcharr, then try again.
- **"Too many attempts."** Wait a minute before trying again.
- **"Signed in, but this TV could not save the sign-in."**, or **"Saved only until this TV
  restarts."** The TV had nowhere permanent to keep the sign-in. [Collect the event
  log](#collecting-logs) and open an issue.

The address field notes **"Not encrypted on your network"** for an `http://` address: your username
and password cross your network unencrypted once, at sign-in. Use an `https://` address if your
server has one.

## The Guide says "Couldn't load the guide" or shows no programmes

- **"The server no longer accepts this TV's key."** The API key the TV kept was revoked or the
  account changed. Choose **Sign in again**.
- **"No channels."** The account you signed in with can watch no channels. Add channels to it in
  Dispatcharr.
- **Channels but "No information" for every programme.** Your server has no guide data (EPG) for
  those channels, or none for the time shown. The app also compares the TV's clock with the server's
  and uses the server's when the TV's is far off, so a TV whose clock is wrong still shows the right
  programmes; setting the TV's date and time to update automatically is still worth doing.

## A channel won't play

The failure screen says why:

- **"This channel's video format can't play on this TV"**, naming MPEG-2 for example. The TV's
  playback pipeline does not take that video codec. In Dispatcharr, give the channel a stream
  profile that converts to H.264.
- **A channel with DTS audio fails to start.** Many 2020-and-later LG sets cannot decode DTS; use a
  stream profile with AC-3 or AAC audio for that channel.
- **"The media stream could not be opened"**, or a stream that keeps reconnecting. The server did
  not deliver the stream: check the channel plays in Dispatcharr itself, and that your provider's
  stream limit is not already used by another viewer.

**Try again** on the failure screen tunes the channel afresh, as does changing channel and back. A
stream that drops or freezes while you watch is tuned again automatically.

## Playback reports `jail_missing_rtkmem`

This is a specific problem on Realtek **k5lp/k3lp** sets: LG's Developer Mode sandbox on some of
these sets withholds a device (`/dev/rtkmem`) that native video decoding needs. Dispatcharr TV detects
this and reports it instead of crashing.

On a **rooted TV with Homebrew Channel**, the failure screen offers **Repair**, which asks
Homebrew Channel's elevated service to patch the sandbox. Read the [full repair procedure and its
limits](native-video-sandbox.md) before using it: it only runs once per app session, and a timeout
leaves the outcome unknown until you fully close and reopen Dispatcharr TV. **Without root, there is
currently no fix.**

Don't use Repair for a plain black screen, an unsupported codec, or any failure other than
`jail_missing_rtkmem` — it addresses that one sandbox problem and nothing else.

## Collecting logs

Dispatcharr TV keeps a few small log files on the TV. When you report a problem, they are what lets
the problem be found. You can fetch them yourself with [webOS Dev
Manager](install-and-verify.md#2-connect-your-computer-to-the-tv). On a television that is not
rooted, Dev Manager should be able to download them, because they are readable by the app's group.

### Which log to attach

| What happened | Attach |
| --- | --- |
| A channel did not start, froze or stopped (for example "Playback stopped after it had started") | the event log |
| The app closed by itself, vanished, or the TV returned to the launcher | the crash log **and** the event log |
| The app does not start, or shows a black screen and exits | the stderr log **and** the crash log |

If you are not sure, attach all three. They are small.

### Where they are

Where the files are depends on which Dispatcharr TV you run. The regular release keeps its logs
directly in `/tmp/`. Dispatcharr TV Nightly keeps its own copies in a folder of its own, so it never
overwrites the regular app's logs.

| Install | Where the log files are |
| --- | --- |
| Dispatcharr TV (release) | directly in `/tmp/`, for example `/tmp/dispatcharrtv-events.log` |
| Dispatcharr TV Nightly | in the folder `/tmp/com.magness.dispatcharrtv.nightly/` |

The three files have the same names in both places:

- `dispatcharrtv-events.log` — the event log: what the app did, line by line.
- `dispatcharrtv-crash.log` — the crash log: a record each time the app was stopped by a fault.
- `dispatcharrtv-stderr.log` — what the app and the TV's own libraries printed to the error stream.

### Fetch them before you reopen the app

The event log and the stderr log start empty **every time Dispatcharr TV launches**. If you reproduce
the problem, close Dispatcharr TV and open it again, the log of the failure is gone. So:

1. Reproduce the problem.
2. Leave the app as it is, or just go back to the launcher. Do not launch Dispatcharr TV again yet.
3. Fetch the logs, as below.

The crash log is different: it only grows, so earlier crashes stay in it across relaunches. It is
cleared when the TV restarts, because `/tmp` is.

### Fetching them with Dev Manager

1. Open **webOS Dev Manager** on your computer and select your TV. It must be connected the same
   way you installed Dispatcharr TV.
2. Open the **Files** view. It lists the TV's folders, and its breadcrumb bar at the top starts
   at `/`.
3. Click `/`, then double-click **tmp**. For Dispatcharr TV Nightly, then double-click
   **com.magness.dispatcharrtv.nightly**.
4. Click a log file to select it, then click the **Download** button in the toolbar and choose
   where to save it on your computer.
5. Attach the downloaded files to your [GitHub issue](https://github.com/andrewmagness/webos-dispatcharr/issues/new/choose).
   If a file is large, compress it first.

If a file is missing, the app has not run since the TV last restarted, or you are looking in the
wrong folder for your install. If Dev Manager shows the file but cannot download it, say so in
your report instead of giving up; the project wants to know.

### What to include in the report

Logs help most with a few facts beside them:

- your TV model (from the sticker on the back, or Settings, then About This TV) and webOS
  platform release;
- the Dispatcharr TV version, and whether it is a release or a nightly;
- your Dispatcharr version, and the stream profile the channel uses if you know it;
- what you did and what happened.

On a playback failure screen, the small line at the bottom adds facts the project needs. For
example:

```
Dispatcharr TV 0.1.0 · webOS 9.2.4 · k24 · K24_DVB · BOARD_DV_1ST · playback_interrupted
```

It reads: the Dispatcharr TV version, the webOS release, then what the TV reports about itself — on
many sets the first of these is the platform codename (`k24` here) rather than the model on the
sticker, followed by the chip or board and the hardware revision — and last the failure code. Some
parts can be missing on some TVs. It does not replace your TV model from the sticker; send both.
Copy the line exactly into your report, or photograph the screen.

### What the logs contain

The event log replaces your username before each line is written, and never contains your password
(the app does not keep it). It does contain your server's channel identifiers and the technical
detail of each stream, so think before you post a log in a public issue. The crash and stderr logs
are not cleaned line by line the same way; they hold fault addresses and whatever the TV's libraries
print. [The privacy policy](../PRIVACY.md) and [what the app
writes](install-and-verify.md#what-it-writes) spell this out. Never post your server's API key or
the Developer Mode passphrase, even if you see one in a file.

The app sends nothing on its own: it has no crash reporting and no analytics, so a log you attach is
the only way the project sees what happened.

## Other problems

Try the current release first, then photograph any failure screen and [collect the
logs](#collecting-logs) before you reopen the app, and report it in [GitHub
Issues](https://github.com/andrewmagness/webos-dispatcharr/issues/new/choose) with the facts listed
in [What to include in the report](#what-to-include-in-the-report).

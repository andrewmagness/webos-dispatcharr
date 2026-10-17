# Dispatcharr TV Privacy Policy

Applies to Dispatcharr TV 0.1.0. Last updated 17 October 2026.

## Who is responsible for Dispatcharr TV data

The maintainer of this repository is responsible only for data Dispatcharr TV stores locally and for
optional reports you choose to share. Contact: the repository's issue tracker,
<https://github.com/andrewmagness/webos-dispatcharr/issues> (keep personal data out of a public issue).

## Your Dispatcharr server

To sign in, list channels, read the programme guide and play live TV, Dispatcharr TV communicates
directly with the Dispatcharr server you enter in Setup. Those requests are handled by that server
and its operator. Dispatcharr TV's developer does not receive them. Your password is sent to the
server once, to sign in, and is not stored.

## Data stored on this television

Dispatcharr TV stores:

- **Your account**: the server's address, the username you signed in with and the API key the
  server issued, in one file readable only by the app (mode `0600` when it is written).
- **Your choices for that server**: your favourite channels, the channel you last watched, and
  whether the app starts on it. Channels are named by the server's identifiers for them, never by
  what was on.
- **Channel logos**, cached as bounded files for reuse across restarts.
- **Local technical logs**: an event log and an error-output log, each emptied every time the app
  launches, a crash log that is kept until the television restarts, and a bounded storage status
  snapshot.
- **Optional-reporting state**: your answers to the two optional-reporting questions, the random
  Crash report ID if you turned crash reports on, the random Analytics ID if you turned product
  analytics on, any report waiting to be sent, and a marker recording how much of the crash log has
  already been read.

Dispatcharr TV has a developer-only input/frame recorder used to reproduce and test bugs. It is not
part of the app you installed: a release build has the developer-trigger inputs, including the
recorder, compiled out. The unconditional `dispatcharrtv-*.log` files are create-only diagnostics,
not trigger inputs.

Signing out (Setup) removes the account file. Your channel choices are not a credential and are
kept, so signing back in to the same server finds them; signing in to a different server starts
with none. A queued report is deleted once sent, or at the moment you switch its category off. The
event log and the error-output log are emptied every time the app launches, the crash log is kept
until the television restarts, and the storage snapshot is replaced when its bounded status
changes. **webOS gives an application no way to run code as it is removed**, so these files can
survive an uninstall.

## This version does not ask about optional reports

Crash reports and product analytics are both off until you choose to share them, and this version
of Dispatcharr TV has no screen that asks. Nothing below is sent from it. The rest of this policy
describes what each report contains, so the answer is on record before the question is asked.

## Optional crash reports

Crash reporting is off until you choose to share it. If enabled, Dispatcharr TV sends technical crash
details to Sentry in Germany. A report may include the signal, code addresses, thread information,
internal component labels, app and webOS versions, television model and hardware compatibility
details needed to reproduce and symbolicate the failure.

A native crash report may also contain the last 16 window diagnostic steps: entering or leaving
background, releasing the borrowed window surface, querying the native window, finishing the background
or return step, and beginning or completing the first frame after return. While the app is being sent to
the background or brought back it also records the video player's teardown and video-window steps:
unloading, destroying or retaining the player's native object, releasing the video plane, and creating
and destroying the video window. The create step is recorded once the create call has returned, whether
or not it produced a window, so it does not mean the window was obtained. These teardown and video-window
steps are not recorded when the player is merely restarted (a channel change or a reconnect).
These steps include technical timestamps, fixed labels, whether playback was active, whether display/surface handles were
available, and SDL's version numbers. They contain no window addresses or viewing content.
They are collected only while crash reporting is enabled and accompany a crash rather than being
sent as usage events. Disabling crash reports removes the local trace.

When the television's own crash reporter leaves nothing for a crash that was a native fault (a
signal such as a segmentation fault), Dispatcharr TV sends a simpler report built from its own crash
log instead. It carries the same kernel name and release, webOS and hardware compatibility
details as a native report (plus the same `probe` fact described for playback-error reports
below), and the file name (never the folder) and build identifier of each
library (a library of the television's firmware or one bundled with Dispatcharr TV) the faulting
address and the return address were in, with the library's load address and size. A library that
has no build identifier (the television's own C library has none) is still named, with its load
address and size and an all-zero placeholder identifier, as the native report lists it. It carries the
time of the crash, to the second, as the television's clock read it when the crash was logged (it
is left out when that clock reads an implausible time). It also says in
one fixed word why the primary report is missing: `no_envelope` (no usable report was left:
the crash reporter left none, or left one that was malformed and was deleted before the comparison,
which looks the same) or `envelope_unmatched` (it left reports, none of them this crash). No path, library
folder or other file content is sent.

When the crash was a Rust panic rather than a fault, the fallback report is smaller still: the
source file and line of the panic, a hash of the panic message (the message itself is never sent),
the app version and build identifier of Dispatcharr TV, the `environment` label of the build (`production`
or `development`), the time of the panic (read and bounded as above), the same kernel name and release,
webOS and hardware compatibility details (including the `probe` fact) that the fault fallback report carries, and
the Crash report ID below. It carries none of the library details above.

Every crash and error report carries a **Crash report ID**: a random identifier created on this
television when you turn crash reports on, sent as the report's `user.id`. It exists so that
repeated crashes under one Crash report ID are counted once rather than once each — Sentry's
"users affected" figure is the number of distinct Crash report IDs an issue has reached — which is
what tells a problem that hit many people apart from one television that hit it many times. It is not derived from your Dispatcharr
account, your television or anything about you, and it is never sent with product analytics.
Turning crash reports off deletes the local identifier; enabling them later creates a new one.
Reports already sent keep the old identifier.

A handled **playback-error report** is covered by the same choice. It is sent when playback reaches
its explicit terminal error screen. It contains a fixed failure `kind` (`no_video_track` /
`media_source` / `playback_interrupted` / `tv_pipeline` / `jail_missing_rtkmem` / `load_timeout` /
`unspecified`), the declared and observed rate classes, a coarse raster class, the video
pipeline's stage, an HTTP class and buffer class, whether a first picture appeared (`started`),
and at most 32 typed playback transitions with bucketed elapsed times. It contains no channel,
programme, URL, path, server identity, address, API key, account or username, and is not joined to
the product analytics identifier or `playback_id`. It carries the same Crash report ID as a crash
report, and the same television model, SoC, hardware revision, webOS release and the
`rtkmem`/`install` sandbox facts a crash report carries. It also carries a `probe` fact: two fixed
words, each `ok`, `no_keys`, `unreadable` or `unprobed` (the boot probe had not run yet), one for
each of the two firmware information files the television model and webOS release are read from
(`probe` is `ok/ok` when both answered), saying why that model, SoC or release is empty when it is.
Buffering or a reconnect does not by itself send a report.

When the media transfer itself died, the report also says how, as two more closed fields and
never as the network library's own number: `transport` — `none` / `partial` / `recv_error` /
`send_error` / `got_nothing` / `timeout` / `http2` / `reopen_failed` / `other` (the class of the
error, and `reopen_failed` when the retry after it was refused as well) — and `transfer_age` —
`unknown` / `<5s` / `5-30s` / `30-90s` / `90-300s` / `300s+` (how long that transfer had been
running, so a connection cut at a fixed time is visible as one bucket). Both appear as a tag and
in the playback context; both are `none` / `unknown` when no transfer failure was recorded.

The report also carries `pipeline_error`, which explains a `tv_pipeline` failure: the whole number
the television's own video pipeline gave with its first error in that playback (for example `601`
for a resource-allocation refusal), `0` when it gave none, and otherwise any 32-bit integer. It is
a number and nothing else — the firmware's accompanying text is not sent, because it can name
addresses or files. It appears as a tag and in the playback context.

The report also carries `pipeline_error`, which explains a `tv_pipeline` failure: the whole number
the television's own video pipeline gave with its first error in that playback (for example `601`
for a resource-allocation refusal), `0` when it gave none, and otherwise any 32-bit integer. It is
a number and nothing else — the firmware's accompanying text is not sent, because it can name
addresses or files. It appears as a tag and in the playback context.

A report also tells the attempt that failed apart from the playback it happened in, because a
channel that played for an hour can fail on a reconnect. `started` describes the playback as a
whole; `attempt_started`, `yes` or `no`, says whether the attempt that failed had put a picture on
screen; and `http_status` is the exact HTTP status number the server answered with (`100` to
`599`), or `0` when none was recorded. The status is a number and nothing else — the server's
accompanying text is not sent. Both appear as a tag and in the playback context; `http` keeps its
coarse class beside the number.

Two more closed fields say why that attempt was made and which one it was: `attempt_origin`, one of
`fresh` (a channel the viewer tuned: OK in the Guide, CH▲/▼ or the recall key) / `user_retry` (*Try
again* on the failure screen) / `recovery` (the app tuning the channel again on its own, after its
stream ended or stalled or carried other codecs than declared) / `resume` (tuning the channel
again at the live edge after the app returned to the foreground); and `attempt_ordinal`, which open
of this playback it was, `1` to `9` and then `9+`, counting the first open, every retry and every
reconnect. The count starts again at `1` with each channel the viewer tunes and is kept only in
memory. Both appear as a tag and in the playback context.

## Optional product analytics

Product analytics is a separate choice and is off until you choose to share it. If enabled,
Dispatcharr TV sends typed screen and feature events and broad playback outcome classes to PostHog
in Germany. Reports carry a random Analytics ID created when you turn product analytics on and may
include the app version, webOS version, television model and SoC. Turning product analytics off
deletes the local identifier; enabling it later creates a new one.

Every product analytics event also carries this bounded compatibility context:

| property | value |
|---|---|
| `app_version` | the Dispatcharr TV package version |
| `webos_release` | the webOS release reported by nyx |
| `webos_api` | the webOS API version reported by nyx |
| `webos_codename` | the webOS firmware family reported by nyx |
| `device_model` | the LG model/platform class reported by nyx |
| `soc` | the SoC/board class reported by nyx or, when nyx names none, the leading board-family token (at most eight lowercase letters and digits, such as `k5lp`) of the television's machine name |
| `hardware_revision` | the hardware revision class reported by nyx |
| `rtkmem` | `ok` / `missing` / `n/a` — the k5lp/k3lp `/dev/rtkmem` jail pre-flight |
| `install` | `devmode` / `homebrew` / `unknown` — never the install path |

| event | fields |
|---|---|
| `app.launch` | *(none)* |
| `route.entered` | `screen` — one of a fixed list of screen names |
| `feature.used` | `feature` — one of a fixed list of feature names |
| `playback.requested` | `playback_id` — a random number minted per attempt, never stored and never reused |
| `playback.started` | `playback_id` — a random number minted per attempt, never stored and never reused; `raster` — `sd` / `hd` / `fhd` / `uhd` / `unknown` — never the raster; `fps` — a fixed rung: `24`/`25`/`30`/`50`/`60`/`100`/`other`/`unknown` — never the measured rate; `video` — a codec name from a fixed table; anything else is `other`; `audio` — a codec name from a fixed table; anything else is `other`; `startup` — `<1s` / `1-3s` / `3-10s` / `10s+` — never the interval |
| `playback.failed` | `playback_id` — a random number minted per attempt, never stored and never reused; `kind` — `no_video_track` / `media_source` / `playback_interrupted` / `tv_pipeline` / `jail_missing_rtkmem` / `load_timeout` / `unspecified` |
| `playback.cancelled` | `playback_id` — a random number minted per attempt, never stored and never reused |
| `playback.abandoned` | `playback_id` — a random number minted per attempt, never stored and never reused |
| `playback.quality` | `playback_id` — a random number minted per attempt, never stored and never reused; `rebuffers` — `0` / `1` / `2-3` / `4+`; `buffering` — `none` / `<2s` / `2-10s` / `10s+` — never the interval |
| `playback.ended` | `playback_id` — a random number minted per attempt, never stored and never reused |

## Never included in optional reports

Optional reports have no fields for channel names or numbers, programme titles, server names or
addresses, usernames, API keys, passwords, or exact viewing history.

## Your choices

Crash reports and product analytics are independent. Withdrawing a choice stops new reports of that
category, removes queued records that are no longer permitted, and deletes that category's
identifier from this television. One report that the sender had already picked up at the moment
you withdraw may still be sent; no further report is picked up after it.

To ask what a category holds for your installation, or to have it deleted, write to the contact
below and quote that category's identifier — the Crash report ID for crash and error reports, the
Analytics ID for product analytics. Each identifier is the only handle its reports carry, so a
request without it cannot be matched to anything.

## Contact and non-affiliation

Privacy questions may be asked on the repository's issue tracker. Security vulnerabilities may be reported
privately through GitHub Security Advisories for `andrewmagness/webos-dispatcharr`.

Dispatcharr TV is an independent, unofficial application. It is not produced by, endorsed by, or
affiliated with the Dispatcharr project or LG Electronics Inc.

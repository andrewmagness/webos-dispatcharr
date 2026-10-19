# Known issues — check here before you decompile firmware or blame LG

A crash that looks like a firmware defect has, in this code's history, more than once turned out to
be a bug already fixed somewhere else, re-investigated from scratch with a device session and a
decompilation. Read the entries below before opening the `decompile-tv-lib` skill or writing off a
crash as LG's. Add an entry whenever one would have saved a device session.

## k5lp / k3lp: a crash right after the first Load, on every channel

**Symptom:** the app crashes immediately on every playback attempt on a **Realtek k5lp or k3lp
chassis** (`board=K5LP_ATSC` and similar), right after the event log's
`SMP loadCompleted (priming before Play)` line and before any video bind. Two variants: SIGABRT
preceded in stderr by `GStreamer-CRITICAL: Trying to dispose object "omxh264dec", but it still has a
parent "registry0"`, or SIGSEGV with `addr=0x0`.

**It is not a firmware bug, and the fix is in this tree.** Developer Mode's jail on those chassis
does not grant `/dev/rtkmem`
([webosbrew/webos-homebrew-channel#202](https://github.com/webosbrew/webos-homebrew-channel/issues/202)),
so Starfish's `Load` hangs inside video-output init. The original defect was that the native session
was marked ready before the synchronous `Load` returned, so the main thread's verbs (`isLoadCompleted`,
`Play`, `Feed`, the ACB bind) raced a `Load` still in flight — harmless on most sets, a crash where
`Load` hangs. Two things close it, and both must stay:

- `src/starfish.c`'s `LOAD_RETURNED()` gate: nothing drives the Starfish object until `Load` has
  returned (`rust-modules/media/src/player/CLAUDE.md` has the full rule);
- the `/dev/rtkmem` probe: on an affected chassis with the device missing, the player refuses to
  `Load` at all and reports `jail_missing_rtkmem` (`docs/native-video-sandbox.md` has the Repair path
  for rooted sets).

If this crash reappears, check first that both are still in the build that crashed (`grep
LOAD_RETURNED src/starfish.c`, and a `start_bufferfeed: refusing native video` line in the event log
on an affected set).

## Maintenance lines: a fix that never reached `main`

The code this project grew from once had a crash fix released on a `release/vX.Y` line that never
reached `main`, and the crash was re-diagnosed from scratch as a firmware defect. **After a patch lands
on a `release/vX.Y` line, port its real fixes back to `main`** (the `cut-release` skill), and when
chasing a crash, check whether a maintenance line already fixed it:
`git log --all -S '<a distinctive log line from the crash>'`.

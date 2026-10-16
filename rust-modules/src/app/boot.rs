//! Boot-time helpers of the app core: the desktop window size, the panic logger, the replay
//! budget, the direct-server trigger, the cursor and the loop-rate tick. Moved out of `app.rs`
//! verbatim in phase 1a of the UI restructure (a pure move; `pub(crate)` widening only).

use super::*;

/// The window size a DESKTOP should open at, in points: the authored 1920x1080 canvas divided by
/// the smallest whole number that fits the usable display area.
///
/// **An exact divisor, never a best fit.** `surface::scale` will letterbox any drawable it is
/// given, so an arbitrary size would *work* — it would just be soft, because every glyph and icon
/// mask in this app is rasterized for a 1:1 surface (`gfx::snap`, and the crispness contract in
/// `theme.rs`) and a fractional scale resamples all of it. 1/1, 1/2 and 1/3 keep whole texels whole.
///
/// The television is untouched by any of this: it takes the panel, and the canvas IS the surface.
/// A Mac is the case the `surface` doc was written against — 1920x1080 exceeds the usable area of
/// every laptop display Apple ships, so asking for it flatly would put the title bar above the
/// screen and the bottom of the interface under the Dock.
///
/// Falls back to the canvas size if SDL cannot answer, which is the behaviour this replaced.
#[cfg(feature = "hostsim")]
pub(crate) fn desktop_window_size() -> (c_int, c_int) {
    // `DISPATCHARRTV_WIN=<w>x<h>` overrides the fit entirely — `make sim-shot SIM_W=1920 SIM_H=1080`.
    // It exists because the fit below is chosen for a HUMAN looking at a window, and a screenshot
    // is not that: on a 1x display the divisor lands on 2 and every shot comes back 960x540, which
    // is half the canvas the UI is authored at. A hairline, a 1px edge-sheen and a snapped glyph
    // are exactly the things that do not survive that, so a shot taken to JUDGE the interface has
    // to be asked for at full size. Off-screen edges are fine for a headless grab: the drawable is
    // the window's own framebuffer, not the part of it the compositor happens to show.
    if let Some(v) = std::env::var_os("DISPATCHARRTV_WIN") {
        let v = v.to_string_lossy().to_lowercase();
        if let Some((w, h)) = v.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.trim().parse::<c_int>(), h.trim().parse::<c_int>()) {
                if w > 0 && h > 0 {
                    return (w, h);
                }
            }
        }
    }
    let mut r = [0 as c_int; 4]; // SDL_Rect: x, y, w, h
    let ok = unsafe { SDL_GetDisplayUsableBounds(0, r.as_mut_ptr()) } == 0;
    let (uw, uh) = (r[2], r[3]);
    if !ok || uw <= 0 || uh <= 0 {
        return (SCR_W, SCR_H);
    }
    // A little headroom under the usable bounds: a window flush against them reads as a fullscreen
    // that went wrong rather than as a deliberate size.
    for div in 1..=3 {
        let (w, h) = (SCR_W / div, SCR_H / div);
        if w <= (uw as f32 * 0.95) as c_int && h <= (uh as f32 * 0.95) as c_int {
            return (w, h);
        }
    }
    (SCR_W / 3, SCR_H / 3)
}

/// Log every Rust panic (message + source location + thread) to the event log AND the
/// persistent crash log BEFORE it unwinds. A panic that crosses an extern "C" boundary
/// (e.g. libav calling ff::read_cb/seek_cb) aborts the process (SIGABRT) — by then the
/// message is gone, so capturing it here is the only way to see WHAT panicked. Pairs with
/// `src/crashtrace.c` — not `main.c`, which the tracer left in 2026-08-29 — whose re-raise buys SAM
/// a real `WIFSIGNALED` status and **nothing else**: `core_pattern` on this firmware is the bare
/// string `core` and `RLIMIT_CORE` is 0, so no core is written and no crashd report is ever
/// generated. `crashtrace.c` says so itself. Two deliberate SIGSEGVs produced the signal status and
/// an empty `/var/log/reports/librdx/`.
///
/// The record this hook writes (the panic line and its `time:` line) is also the crash channel's
/// PANIC input: `telemetry::crashreport` reads the log on the next launch, hashes the message and
/// sends the location and the bounded crash time, never the message.
pub(crate) fn install_panic_logger() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic>".into());
        let cur = std::thread::current();
        let thread = cur.name().unwrap_or("?");
        let line = format!("*** RUST PANIC [{thread}] at {loc}: {msg}");
        log(&line);
        let _ = record_panic_in_crash_log(
            &plx_base::paths::in_runtime_dir(plx_base::paths::runtime_file::CRASH),
            &line,
            plx_telemetry::telemetry::crashreport::now_secs(),
        );
        default(info); // preserve default behaviour (stderr -> dispatcharrtv-stderr.log)
    }));
}

/// Append the panic's record to the crash log at `path` (`dispatcharrtv-crash.log`, which `src/main.c`
/// opens and the signal tracer appends to).
///
/// **Two disciplines, both the event log's** (the crash log is group-readable for the same Dev Mode
/// reason, `eventlog::LOG_MODE`). The text passes [`plx_base::eventlog::scrub::scrub_local`]: a panic
/// message is whatever the failing call was formatting, and the raw line used to go to this file
/// while only the event-log copy ([`log`]) was redacted. And the open is
/// [`plx_base::eventlog::open_log_append`]'s (`O_NOFOLLOW`, a regular file this uid owns with ONE
/// link, 0640, never truncated) rather than a bare `OpenOptions` at the umask's mode, so a name
/// planted in the shared `/tmp` is refused instead of written through. `Err` is for the hook to
/// ignore: a panic hook has nowhere to report a failure to.
///
/// **The record is two lines in ONE write:** the panic line and the `time: <unix seconds>` line the
/// C tracer also writes under a signal record ([`plx_telemetry::telemetry::crashreport::crash_time_record`]
/// is the one spelling). The crash importer sends it as the event's `timestamp`, so the report says
/// when the panic happened rather than when the next launch found it. `0` (a clock that reads
/// nothing) writes no time line.
fn record_panic_in_crash_log(path: &std::path::Path, line: &str, unix_secs: u64) -> std::io::Result<()> {
    let mut record = plx_base::eventlog::scrub::scrub_local(line);
    if unix_secs > 0 {
        record.push('\n');
        record.push_str(&plx_telemetry::telemetry::crashreport::crash_time_record(unix_secs));
    }
    plx_base::eventlog::append_record(path, &record)
}

#[cfg(test)]
mod panic_log_tests {
    use super::record_panic_in_crash_log;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    const LINE: &str = "*** RUST PANIC [demux] at src/ff.rs:1204: bad url https://192-168-1-50.0123456789abcdef0123456789abcdef.plex.direct:32400/x?X-Plex-Token=SECRETTOKEN";

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("plx-panic-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    /// The crash log is group-readable (a Dev Mode user fetches it), so the panic text a hook writes
    /// to it passes the same redaction every event-log line does: a panic message can embed whatever
    /// the failing call was formatting, including a URL.
    #[test]
    fn the_panic_text_is_scrubbed_like_every_event_log_line() {
        let dir = scratch("scrub");
        let path = dir.join("crash.log");
        record_panic_in_crash_log(&path, LINE, 1_750_000_000).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("SECRETTOKEN"), "{written}");
        assert!(!written.contains("192-168-1-50") && !written.contains("plex.direct"), "{written}");
        assert!(written.starts_with("*** RUST PANIC [demux] at src/ff.rs:1204: "), "the record's shape survives: {written}");
        assert!(
            written.ends_with("\ntime: 1750000000\n") && written.matches('\n').count() == 2,
            "the panic line, then its time line, newline-terminated: {written}"
        );
        // And the crash report parser still reads the scrubbed record, with the time it was given.
        let reports = plx_telemetry::telemetry::crashreport::parse(&written);
        assert_eq!(reports.len(), 1, "{written}");
        assert!(
            matches!(&reports[0], plx_telemetry::telemetry::crashreport::Report::Panic(p) if p.crash_time == Some(1_750_000_000)),
            "{reports:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same open discipline as the event log's sink ([`plx_base::eventlog::open_log_append`]):
    /// 0640 even under a restrictive umask, appended to, never truncated.
    #[test]
    fn the_crash_log_is_0640_and_appended_to() {
        let dir = scratch("mode");
        let path = dir.join("crash.log");
        std::fs::write(&path, b"*** SIGNAL 11 earlier\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        record_panic_in_crash_log(&path, "*** RUST PANIC [t] at src/a.rs:1: boom", 1_750_000_000).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o7777, 0o640);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "*** SIGNAL 11 earlier\n*** RUST PANIC [t] at src/a.rs:1: boom\ntime: 1750000000\n"
        );
        let fresh = dir.join("fresh.log");
        record_panic_in_crash_log(&fresh, "*** RUST PANIC [t] at src/a.rs:1: boom", 1_750_000_000).unwrap();
        assert_eq!(std::fs::metadata(&fresh).unwrap().mode() & 0o7777, 0o640);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A planted symlink or a second hard link at the crash log's name is somebody else's file:
    /// refused, and it keeps its content and mode.
    #[test]
    fn a_symlinked_or_hard_linked_crash_log_is_refused_and_the_other_file_is_untouched() {
        let dir = scratch("refuse");
        let victim = dir.join("auth.json");
        std::fs::write(&victim, b"session").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();

        let linked = dir.join("symlinked.log");
        symlink(&victim, &linked).unwrap();
        assert!(record_panic_in_crash_log(&linked, LINE, 1_750_000_000).is_err());

        let hard = dir.join("hardlinked.log");
        std::fs::hard_link(&victim, &hard).unwrap();
        assert!(record_panic_in_crash_log(&hard, LINE, 1_750_000_000).is_err());

        let meta = std::fs::metadata(&victim).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o600, "never widened");
        assert_eq!(std::fs::read(&victim).unwrap(), b"session", "never written to");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod seam_order_tests {
    /// `plx_base::identity` reports the version it is handed (`set_version`) and, in a
    /// shipping build, panics when asked for it earlier, so the hand-in has to be the first thing
    /// the one entry point does: nothing before it can build a header or a `User-Agent`. It also
    /// has to hand in `PLX_VERSION` itself, the string `ci/check-package.py` grades in the binary
    /// (as `dispatcharrtv@` + version, see the telemetry hand-in below), and no second production site
    /// may hand in anything else. Reads the source, like the tests
    /// below, because no host test can run `enter_application`.
    #[test]
    fn the_version_is_handed_in_first_and_only_there() {
        let hand_in = concat!("plx_base::identity::", "set_version(env!(\"PLX_VERSION\"));");
        let source = include_str!("mod.rs");
        let body = source
            .split_once("fn enter_application(")
            .expect("enter_application")
            .1
            .split_once('{')
            .expect("enter_application's body")
            .1;
        let first = body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("//"))
            .expect("a first statement");
        assert_eq!(first, hand_in, "set_version must be enter_application's first statement");
        // `plx_telemetry` reports the release it is handed the same way (and also panics when asked
        // earlier in a shipping build). It is composed here with `concat!`, not by the telemetry
        // crate, because `ci/check-package.py` greps the packaged binary for that exact string.
        let release = concat!("plx_telemetry::telemetry::", "set_release(concat!(\"dispatcharrtv@\", env!(\"PLX_VERSION\")));");
        let second = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .nth(1)
            .expect("a second statement");
        assert_eq!(second, release, "set_release must be enter_application's second statement");
        assert_eq!(
            source.matches(concat!("telemetry::", "set_release(")).count(),
            1,
            "one production hand-in of the release"
        );
        assert_eq!(
            source.matches(concat!("identity::", "set_version(")).count(),
            1,
            "one production hand-in; a second could name a different version"
        );
        assert_eq!(
            source.matches(concat!("enter_application(", "pms_host,pms_port)")).count(),
            1,
            "enter_application has one caller, run_application"
        );
    }

    /// `enter_application` installs the player's error-trace eraser before telemetry first loads a
    /// decision (`pre_boot_diagnostics` -> `telemetry::boot`, which already erases through it) and
    /// before anything can play. It was installed by `player::report::requested`, which a
    /// detail-page preview never passes, so a failed preview's trace outlived a withdrawal. This
    /// reads the source, like the diagnostics boot-order test below, because no host test can
    /// run `enter_application`; what the eraser reaches is graded by `player::report`'s
    /// `a_failed_preview_trace_is_erased_through_telemetrys_hook`.
    #[test]
    fn the_trace_eraser_is_installed_before_telemetry_loads_or_anything_plays() {
        let source = include_str!("mod.rs");
        let body = source
            .split_once("fn enter_application(")
            .expect("enter_application")
            .1
            .split_once("/// The ten-line public skeleton")
            .expect("end of enter_application")
            .0;
        let eraser = body
            .find("plx_media::player::report::install_trace_eraser();")
            .expect("enter_application must install telemetry's error-trace eraser");
        let telemetry = body
            .find("pre_boot_diagnostics();")
            .expect("telemetry's boot load");
        let boot = body.find("boot(pms_host").expect("application boot");
        assert!(
            eraser < telemetry && eraser < boot,
            "the eraser must be installed before telemetry loads and before anything can play"
        );
    }

    /// The storage diagnostics worker probes `app_dir()` on its own thread, so it starts only after the
    /// live identity preamble has written the first two event-log lines. Reads the source like the
    /// test above, for the same reason: no host test can run `enter_application`. It lived in
    /// `plx_platform::storage::diagnostics` until that crate stopped being able to see this file.
    #[test]
    fn worker_start_follows_the_live_boot_identity_preamble() {
        let source = include_str!("mod.rs");
        assert_eq!(
            source
                .matches("plx_platform::storage::diagnostics::start(")
                .count(),
            1,
            "every start site must stay behind the identity preamble"
        );
        let pre_boot = source
            .split_once("fn pre_boot_diagnostics")
            .expect("pre_boot_diagnostics")
            .1
            .split_once("unsafe fn run_and_shutdown")
            .expect("end of pre_boot_diagnostics")
            .0;
        let install = pre_boot.find("\"install: id=").expect("install line");
        let app_dir = pre_boot
            .find("plx_base::paths::app_dir_line()")
            .expect("appdir line");
        assert!(install < app_dir);
        let body = source
            .split_once("fn enter_application")
            .expect("enter_application")
            .1
            .split_once("/// The ten-line public skeleton")
            .expect("end of enter_application")
            .0;
        let identity = body
            .find("pre_boot_diagnostics();")
            .expect("identity preamble");
        let diagnostics = body
            .find("plx_platform::storage::diagnostics::start(")
            .expect("diagnostics start");
        let boot = body.find("boot(pms_host").expect("application boot");
        assert!(identity < diagnostics && diagnostics < boot);
    }

    /// `construct` hands the transport its `User-Agent` before `net::global_init` makes HTTPS
    /// available. `net` reads it only once a request has passed `available()`, so with this order
    /// no request can leave without one; the other order sends the first ones bare.
    #[test]
    fn the_user_agent_is_installed_before_libcurl_is_made_available() {
        // The needles are split with `concat!` so this test's own text, which `include_str!`
        // reads too and which sits above `construct`, can never be what they match.
        let source = include_str!("boot.rs");
        let body = source
            .split_once(concat!("pub(crate) unsafe fn ", "construct("))
            .expect("construct")
            .1;
        let user_agent = body
            .find(concat!("plx_net::net::", "set_user_agent("))
            .expect("construct must install the User-Agent");
        let init = body.find(concat!("plx_net::net::", "global_init()")).expect("construct's libcurl init");
        assert!(user_agent < init, "set_user_agent must come before net::global_init");
    }
}

/// Hide the Magic Remote's on-screen pointer. A webOS-only concept: there is no such cursor to
/// hide on a desktop, and `SDL_webOSCursorVisibility` exists in no SDL but LG's fork.
///
/// One door rather than a branch at each of the five call sites, so the platform question is
/// asked once and the call sites read the same on both.
///
/// Resolved at runtime rather than linked: webOS 3.4 ships SDL 2.0.3, which has none of LG's
/// `SDL_webOS*` extensions, and a strong reference there is a lazily-bound PLT slot that kills
/// the process on the first D-pad press (`tools/fwcompat.py --release 3.4.0`).
#[inline]
pub(crate) unsafe fn hide_cursor() {
    #[cfg(not(feature = "hostsim"))]
    {
        type CursorVisibility = unsafe extern "C" fn(c_int) -> c_int;
        static SYM: std::sync::OnceLock<Option<CursorVisibility>> = std::sync::OnceLock::new();
        let f = SYM.get_or_init(|| {
            let p = plx_base::dynlib::Handle::self_handle().sym("SDL_webOSCursorVisibility")?;
            (!p.is_null()).then(|| std::mem::transmute::<*mut std::ffi::c_void, CursorVisibility>(p))
        });
        if let Some(f) = f {
            f(0);
        }
    }
}
/// Advance the once-per-second LOOP-RATE window: bump `iters_ct` and, when a full second has
/// elapsed, recompute `loop_shown`, reset the window, and return `true` so the caller logs the
/// heartbeat with its own route/overlay tag. Shared by the player and home/detail draw paths.
///
/// This counts **loop iterations, not frames**. Since the present gate (`plx_machine::idle`) landed the two
/// are different numbers, and conflating them is the single most reliable way to misread this app:
/// a settled screen runs the loop at the `IDLE_POLL_MS` rate while swapping nothing. The frame
/// count lives beside it in the heartbeat as `fps=`, from `plx_machine::idle::take_presents`.
pub(crate) fn loop_tick(iters_ct: &mut i32, loop_t: &mut u32, loop_shown: &mut i32, now: u32) -> bool {
    *iters_ct += 1;
    if now.wrapping_sub(*loop_t) < 1000 {
        return false;
    }
    *loop_shown = (*iters_ct as f32 * 1000.0 / now.wrapping_sub(*loop_t) as f32 + 0.5) as i32;
    *iters_ct = 0;
    *loop_t = now;
    true
}
/// How many times a finished `dispatcharrtv-playurl` playback may start itself AGAIN — the
/// `/tmp/dispatcharrtv-replay` trigger's content, as a number (LG App Self Checklist #46).
///
/// A named function rather than a closure at the one call site, for `note_global_press`'s reason
/// one step removed: the call site is inside the SDL event loop, which no host test can enter, and
/// this half touches no SDL at all. Splitting it puts the parsing under `make check` instead of
/// leaving it gradeable only by a television — which matters more here than it looks, because
/// EVERY value this returns is a plausible one and a misparse is invisible on the panel: 0 reads
/// as "the replay arm is missing from this binary" and 2 reads as a loop.
///
/// `None` (no file) is 0 — the one-shot behaviour every other boot has always had. An EMPTY file
/// is 1, which is the whole idiom of this trigger surface (`touch` it and get the obvious thing).
/// An explicit `0` is honoured, so a script can arm the file and turn it off without deleting it.
/// Anything unparseable is 1 rather than 0: this file is armed by hand, and answering a typo with
/// "silently do nothing" is how a green run comes to mean the opposite of what it says.
pub(crate) fn replay_budget(raw: Option<&str>) -> u32 {
    match raw {
        None => 0,
        Some(s) => match s.trim() {
            "" => 1,
            t => t.parse::<u32>().unwrap_or(1),
        },
    }
}

#[cfg(test)]
mod replay_budget_tests {
    #[test]
    fn absent_is_one_shot_and_empty_is_one_replay() {
        assert_eq!(
            super::replay_budget(None),
            0,
            "no trigger must change nothing"
        );
        assert_eq!(super::replay_budget(Some("")), 1);
        assert_eq!(super::replay_budget(Some("  ")), 1);
    }

    #[test]
    fn a_number_is_honoured_including_a_deliberate_zero() {
        assert_eq!(super::replay_budget(Some("1")), 1);
        assert_eq!(super::replay_budget(Some("3")), 3);
        assert_eq!(super::replay_budget(Some(" 2 ")), 2);
        // An armed-but-off file, so a script can stop replaying without deleting the trigger.
        assert_eq!(super::replay_budget(Some("0")), 0);
    }

    #[test]
    fn a_typo_replays_once_rather_than_silently_doing_nothing() {
        // The file is armed by hand. Answering `-1` or `one` with 0 would make the case fail as
        // "the app never re-entered the player", i.e. as a missing feature rather than a typo.
        for bad in ["one", "-1", "1.5", "999999999999999999999"] {
            assert_eq!(super::replay_budget(Some(bad)), 1, "{bad}");
        }
    }
}

/// Everything before the loop: SDL and the window, GL, text, the poster workers, the boot gate
/// (login / token / session / picker), every dev trigger read once, and the `App` literal —
/// `plex_run`'s former body up to `while app.running`, moved verbatim in phase 1b-ii. An early
/// exit is the process exit code `plex_run` returns.
///
/// `mt`, THE main-thread token, is minted once by `plex_run` itself (`plx_base::task::MainThread::
/// assume()` — that function IS the SDL main thread) and MOVES into `App.adapters.player` here,
/// from where a `&mut PlayerAdapter` is the proof it is still held: the ACB/Starfish seam takes
/// `&MainThread` (which is `!Send`, so `task::spawn` rejects any closure that captured one), and
/// the native session slot takes the adapter itself. See `task::MainThread` and `player::adapter`.
pub(crate) unsafe fn boot(
    pms_host: *const c_char,
    pms_port: c_int,
    mt: plx_base::task::MainThread,
) -> Result<App, c_int> {
    // The dev frame probe's clock is `platform`'s, which `gfx` does not depend on.
    plx_gfx::gfx::install_clear_probe(
        plx_platform::tv::window::frame_probe_waiting,
        plx_platform::tv::window::frame_probe_acquired,
    );
    construct(pms_host, pms_port, mt)
}

pub(crate) unsafe fn construct(
    _pms_host: *const c_char, _pms_port: c_int, mt: plx_base::task::MainThread,
) -> Result<App, c_int> {
    SDL_SetMainReady();
    // DEAD END, measured 2026-07-31 — do not re-try this. The obvious answer to "a parked TV
    // should blank itself" is to stop inhibiting the platform screensaver here (and re-allow it
    // per route, since webOS BACKGROUNDS the app to run one and `0x103` suspends the
    // buffer-feed, so it could never be on during playback). It does not work, for a reason
    // upstream of this app: the TV's SDL 2.0.4 fork carries the
    // `SDL_VIDEO_ALLOW_SCREENSAVER` hint STRING but implements no wayland idle-inhibit
    // (`strings libSDL2-2.0.so.0` finds no `idle_inhibit`/`suspend_screensaver` symbol), so
    // this call and `SDL_EnableScreenSaver` are both no-ops. Soaked 34 min on Home with the
    // TV's own `screenSaverEnabled: on`: no screensaver, no `LIFECYCLE: background`, CPU flat,
    // our UI still at full brightness on the panel. webOS does not blank a foreground native
    // app, and nothing reachable from SDL changes that. The line stays because it costs
    // nothing and states the intent; it is not what keeps the screensaver away.
    SDL_SetHint(c"SDL_VIDEO_ALLOW_SCREENSAVER".as_ptr(), c"0".as_ptr());
    if SDL_Init(SDL_INIT_VIDEO) != 0 {
        log("SDL_Init failed");
        return Err(1);
    }
    {
        let d = SDL_GetCurrentVideoDriver();
        if !d.is_null() {
            log(&format!(
                "video driver: {}",
                std::ffi::CStr::from_ptr(d).to_string_lossy()
            ));
        }
    }
    // The television has a real GLES2 driver (a shim over libmali). macOS has none at all —
    // Apple ships desktop GL only, capped at 4.1 core — so asking for ES here fails context
    // creation outright. 4.1 core is the closest thing that exists, and it is a superset for
    // everything this renderer does: a real VBO (never client arrays) and RGBA/UNSIGNED_BYTE
    // textures, both core-profile-legal. The shader sources are adapted at compile time by
    // `gfx::glsl_preamble`, which reads the driver's GLSL version rather than assuming.
    if cfg!(feature = "hostsim") {
        SDL_GL_SetAttribute(A_CTX_PROFILE_MASK, CTX_PROFILE_CORE);
        SDL_GL_SetAttribute(A_CTX_MAJOR, 4);
        SDL_GL_SetAttribute(A_CTX_MINOR, 1);
    } else {
        SDL_GL_SetAttribute(A_CTX_PROFILE_MASK, CTX_PROFILE_ES);
        SDL_GL_SetAttribute(A_CTX_MAJOR, 2);
        SDL_GL_SetAttribute(A_CTX_MINOR, 0);
    }
    // full 32-bit RGBA so the video plane shows through
    SDL_GL_SetAttribute(A_RED, 8);
    SDL_GL_SetAttribute(A_GREEN, 8);
    SDL_GL_SetAttribute(A_BLUE, 8);
    SDL_GL_SetAttribute(A_ALPHA, 8);
    SDL_GL_SetAttribute(A_BUFFER_SIZE, 32);
    // ...and NO depth or stencil, which SDL would otherwise give us anyway: its defaults are
    // 16 bits of depth and 0 of stencil, and asking for neither had simply never been written
    // down. **This renderer has no use for either.** There is no `GL_DEPTH_TEST`, no
    // `glDepthFunc`, no `glDepthMask` and no `glClear(GL_DEPTH_BUFFER_BIT)` anywhere in the
    // crate — every screen is painter's-algorithm 2-D, drawn back to front — and the one
    // scissor user (`gfx::clip_set`) is a scissor, not a stencil.
    //
    // On a TILER this is not merely 4 MB of address space. Midgard allocates the depth buffer
    // per tile alongside colour and, unless the driver proves it dead, RESOLVES it to memory at
    // end-of-frame: 1920x1080x2 bytes written per presented frame for a buffer nothing ever
    // reads. `system.rs` logs what the config actually came back with — a request is not a
    // grant, and the only honest confirmation is `FB bits: … depth=0`.
    SDL_GL_SetAttribute(A_DEPTH, 0);
    SDL_GL_SetAttribute(A_STENCIL, 0);
    // The television is placed at 0,0 at exactly canvas size and takes the panel. A desktop
    // window is centred (`SDL_WINDOWPOS_CENTERED`) at whatever fits — see `desktop_window_size`.
    #[cfg(feature = "hostsim")]
    let (wx, wy, ww_req, wh_req) = {
        let (w, h) = desktop_window_size();
        (0x2FFF_0000u32 as c_int, 0x2FFF_0000u32 as c_int, w, h)
    };
    #[cfg(not(feature = "hostsim"))]
    let (wx, wy, ww_req, wh_req) = (0, 0, SCR_W, SCR_H);
    // The title is furniture a television never draws (no window manager, no decoration) and
    // the first thing a desktop shows, so the two builds spell it differently: the device keeps
    // the process-shaped name every log, `pidof` recipe and skill already uses.
    #[cfg(feature = "hostsim")]
    let title = c"Dispatcharr TV";
    #[cfg(not(feature = "hostsim"))]
    let title = c"dispatcharrtv";
    let win = SDL_CreateWindow(title.as_ptr(), wx, wy, ww_req, wh_req, SDL_WINDOW_FLAGS);
    if win.is_null() {
        log("CreateWindow failed");
        return Err(1);
    }
    let ctx = SDL_GL_CreateContext(win);
    if ctx.is_null() {
        log("GL ctx failed");
        return Err(1);
    }
    plx_base::surface::probe(win);
    // vsync on → the frame rate locks to the panel refresh. `/tmp/dispatcharrtv-novsync` uncaps it so
    // `fps=` reports the true GPU render rate. WSLg's X11/GLX swap and SDL's offscreen driver accept
    // interval 1 without blocking, so the software budget in `run` follows the same switch.
    let vsync_enabled = !crate::dev::scenarios::novsync_armed();
    SDL_GL_SetSwapInterval(if vsync_enabled { 1 } else { 0 });
    #[cfg(all(feature = "hostsim", target_os = "linux"))]
    let soft_frame_pacing = vsync_enabled
        && super::window_activity::swap_ignores_vsync(
            std::env::var_os("WSL_DISTRO_NAME").is_some(),
            std::env::var("SDL_VIDEODRIVER").ok().as_deref(),
        );
    {
        let r = glGetString(GL_RENDERER);
        let v = glGetString(GL_VERSION);
        let renderer = (!r.is_null()).then(|| std::ffi::CStr::from_ptr(r).to_string_lossy());
        if let (Some(renderer), false) = (&renderer, v.is_null()) {
            log(&format!("GL: {} / {}", renderer, std::ffi::CStr::from_ptr(v).to_string_lossy()));
        }
        // A CPU rasterizer's frame time is not a main-thread hang (`task::runtime_check`).
        #[cfg(feature = "threadcheck")]
        plx_base::task::runtime_check::note_renderer(renderer.as_deref());
    }
    // The system on-screen keyboard, PROBED — see `crate::textinput`'s module doc. Both facts
    // on this line are preconditions that fail in complete silence, and nothing in this tree
    // had ever read either of them:
    //   support= `SDL_HasScreenKeyboardSupport` — does this firmware's SDL have a panel at all.
    //   focus=   `SDL_WINDOW_INPUT_FOCUS` — `SDL_StartTextInput` shows the panel only
    //            `if (SDL_GetKeyboardFocus())`. Clear, and it enables text events, returns
    //            void, and no panel appears.
    //   active=  whether text events are already on. It is 1 on a desktop and 0 here, because
    //            SDL only auto-starts text input on platforms with NO screen keyboard — which
    //            is precisely why `textinput` tracks its own started flag instead of this one.
    // A `focus=0` HERE is not yet a verdict: the flag arrives with the wayland keyboard
    // `enter`, which needs the event loop below. `textinput::start` logs it again at the
    // moment the field asks for the panel, which is the reading that decides anything.
    // What EGL this set has — extension string, swap behaviour, buffer age. One boot-time
    // read, logged and used for nothing: `docs/egl-partial-update-and-damage.md` is what it
    // was for. Deliberately NOT a new link dependency; see `egl.rs`'s module doc for why
    // `-lEGL` would kill the process at exec() on the very firmwares this app runs on.
    // No platform carve-out: the probe asks EGL nothing unless an EGL context is current on this
    // thread (`egl::current_with`), which is what makes it safe on a GLX-backed Linux simulator.
    plx_gfx::egl::probe();
    crate::textinput::bind(win);
    // …and the same handshake for the ROOT press: `tv::home::go_home`'s fallback leg minimizes
    // this window, and the window is created here, a long way from where BACK is decided.
    plx_platform::tv::window::bind_window(win);
    let wflags = SDL_GetWindowFlags(win);
    log(&format!(
        "keyboard: support={} active={} focus={} winflags=0x{wflags:x}",
        SDL_HasScreenKeyboardSupport(),
        SDL_IsTextInputActive(),
        i32::from(wflags & SDL_WINDOW_INPUT_FOCUS != 0)
    ));

    plx_platform::tv::window::grab(win);
    // EXPERIMENT (`/tmp/dispatcharrtv-opaque`), no-op without the trigger: build the full-surface
    // wl_region once, so `opaque_route` below can declare the UI plane opaque on every screen
    // that has nothing behind it. See `system.rs`'s section on it.
    plx_platform::tv::window::arm_opaque_region();
    plx_gfx::gfx::init_gl();
    plx_gfx::text::init_text();
    plx_gfx::gfx::init_image();
    plx_gfx::gfx::init_blur();
    // The transport takes the client's `User-Agent` as a value (it names no Plex layer): hand it
    // over before any request can be made, so the first one already carries it.
    plx_net::net::set_user_agent(plx_base::identity::user_agent());
    // One-time libcurl bind + init (main thread) before any threaded HTTPS call. A false here
    // means this device has no libcurl we can bind, so plex.tv sign-in will not work — the app
    // still runs, and `net::global_init` has already said so in the event log.
    let _ = plx_net::net::global_init();
    // Drain whatever the LAST session left behind, on a worker — and **after `global_init`,
    // which is the whole reason this line is here and not beside `telemetry::boot()` 170 lines
    // up.** It was there first, and the end-to-end run showed why that was wrong: the worker
    // reached `post_ca` before libcurl was bound, `net::available()` was false, every record
    // came back Keep, and the log read `holding 5 records` immediately ABOVE `net: bound
    // libcurl`. So the first flush of every launch failed, always, and the failure was
    // indistinguishable from a television with no network. Worse than the lost flush: curl's
    // own init is documented as not thread-safe, and a worker that got there first would have
    // been doing it off the main thread.
    //
    // Boot is the right cadence for a television. Sessions are long, and the reports most worth
    // having are about how one ENDED — a crash is the end, so the record was written by a
    // process that no longer exists and this is the first moment anything can send it. A record
    // queued during THIS session goes out at the next launch, or sooner if a consent change
    // flushes.
    plx_telemetry::telemetry::flush_soon();

    // The poster workers and the dev live UI capture stream: the Guide's channel logos come
    // through the same texture pipeline the poster adapter feeds.
    super::adapters::poster::init();
    crate::capture::init();

    // dev: /tmp/dispatcharrtv-toast=<text> — asks the TV's notification service for a system toast,
    // once plain and once as the app, and logs each full outcome (`toast-probe …`).
    #[cfg(feature = "devtriggers")]
    crate::dev::scenarios::toast_probe::arm_at_boot();
    // dev: /tmp/dispatcharrtv-hintoff — the hold hint taught for this run, on every screen kind, read
    // once here on the frame thread (the hint's latches are thread-local). The site demo film is
    // shot with it: the capsule is a one-time teaching overlay, and a film has no viewer to teach.
    #[cfg(feature = "devtriggers")]
    if plx_base::devtrig::flag("hintoff") {
        plx_ui::hold_hint::teach_all_for_dev();
    }
    // The interface follows the television's own language: no Settings page stores a pick.
    #[cfg(not(test))]
    plx_platform::i18n::initialize(plx_platform::i18n::Preference::System, false);
    let bridge = super::bridge::Bridge::new(plx_base::diag::heartbeat::now_us);
    // Construct the one dispatcher; the App takes this same queue.
    let pages = plx_ui::dispatch::Dispatcher::with_transition(Box::new(
        plx_ui::containers::transition::PageDip::new(),
    ));
    // Every ordinary boot lands on Setup, which reads the stored Dispatcharr account itself —
    // showing the form when there is none and handing over to the Guide when there is.
    log("boot: Dispatcharr setup");
    plx_media::player::acb_init(&mt);
    plx_media::ff::boot(); // Live playback resource boot.
    // dev: the animation-diagnostic overlay is OFF by default; /tmp/dispatcharrtv-anim enables it (its
    // trace goes to /tmp/dispatcharrtv-anim.log, a separate stream from the main event log)
    crate::dev::scenarios::arm_anim();
    // dev: /tmp/dispatcharrtv-stillclock=<ms> holds every free-running animation clock (spinners)
    // still, so a screenshot of a waiting screen settles on one deterministic frame.
    crate::dev::scenarios::arm_stillclock();
    // dev: profile is asynchronous EXT_disjoint_timer_query timing; hwcnt is the serialized
    // direct Mali counter-attribution run. Their content names ONE phase (empty = frame.ui).
    // Combining them would perturb the timer result, so fail closed when both are present.
    // dev: /tmp/dispatcharrtv-glassload is the backdrop-glass LOAD DIAL — a sweep of glass-surface
    // count, size and refresh cadence that cycles its own steps inside one launch, so legs are
    // interleaved by construction. /tmp/dispatcharrtv-navblur is the blurred-route-transition
    // prototype. Both live in `ui::glassload`; both are absent from a release build.
    // The plan is built HERE rather than in the `App` literal below so the dial is armed at the
    // point in boot it always was: `configure` logs what it will run, and that line's position in
    // the event log is what a sweep is read against.
    let mut glass = plx_ui::frame::glass::GlassPlan::new();
    crate::dev::scenarios::arm_glassload(&mut glass);
    crate::dev::scenarios::arm_navblur(&mut glass);
    // dev: the two OVERDRAW surfaces (`ui::overdraw`, docs/backdrop-blur-profiling.md Part 5).
    // `dispatcharrtv-overdraw` arms the CPU-side per-draw-class ledger — how much screen-visible
    // quad area this app submits, per primitive family, per frame. It is not billed for the
    // wayland compositor's work and is not `glFinish`-serialised, which is what the GPU's
    // global FRAG_QUADS_RAST cannot say. `dispatcharrtv-drawmask=<classes>` REFUSES every draw of
    // the named classes, so a whole-frame `frame.ui` A/B against the unmasked control prices
    // that class as the frame sees it; `all` draws nothing and is therefore the compositor
    // floor. A masked leg is a broken picture on purpose.
    crate::dev::scenarios::arm_overdraw();
    crate::dev::scenarios::arm_drawmask();
    // dev: /tmp/dispatcharrtv-nobudget — the frame budget's A/B CONTROL leg (spec §8.1). Read here
    // with the other boot triggers; applied to the one `Budget` below, once the tree exists.
    let nobudget = crate::dev::scenarios::nobudget_armed();
    crate::dev::scenarios::arm_profile_hwcnt();
    // dev: /tmp/dispatcharrtv-cpuprof — the render thread's OWN time per phase, every phase at
    // once, no glFinish. The one mode that can see a frame the frame-drop detector reports as
    // all `draw=` and no `swap=`; the two GPU modes above are blind to it by construction.
    crate::dev::scenarios::arm_cpuprof();
    // dev: /tmp/dispatcharrtv-noidle turns the whole-frame present gate (plx_machine::idle) OFF, so a still
    // screen goes back to repainting at panel rate. It is a DIAG trigger (see the list above)
    // precisely so an A/B costs one file and does not also change which screen you boot to —
    // and so that if a frame ever looks wrong on the panel, ruling this feature out is one
    // `rm` rather than a redeploy.
    plx_ui::testpat::boot();
    plx_media::player::seed_dev_track_names();
    crate::dev::scenarios::arm_noidle();
    // dev: /tmp/dispatcharrtv-framedrop — the FRAME-DROP DETECTOR. When present, each frame is timed with
    // the high-res perf counter (pump / draw / swap, NO glFinish so it doesn't perturb the pipeline),
    // and any frame whose total exceeds a threshold (ms; file content overrides the 22ms default) is
    // logged with its phase breakdown + GL texture-upload count — so a scroll judder shows *what* stalled
    // (high `pump`+`up` ⇒ synchronous poster uploads; high `swap` with low pump/draw ⇒ GPU fill).
    let framedrop = crate::dev::scenarios::framedrop_value();
    let framedrop_on = framedrop.is_some();
    let framedrop_thresh: f64 = framedrop
        .and_then(|s| s.parse().ok())
        .filter(|v: &f64| *v > 0.0)
        .unwrap_or(22.0);
    let mut instr = plx_base::diag::heartbeat::Instruments::new(framedrop_on, framedrop_thresh);
    if let Some(slow_ms) = crate::dev::scenarios::framering_ms() {
        instr.arm_ring(slow_ms);
    }
    if framedrop_on {
        plx_base::diag::spans::arm();
    }

    let last_input = clock::now();
    let t0 = last_input;
    let loop_t = t0;
    let iters_ct = 0i32;
    let loop_shown = 0i32;
    // The dev number painted in the top-right corner. Unlike `loop_shown`, this is a real
    // presentation rate: the same completed-window value the heartbeat publishes as `fps=`.
    // It is updated only when that heartbeat drains PRESENTS, so pixels and logs cannot
    // disagree by observing two different counters. On a settled screen the number changes
    // only when the ordinary keepalive next buys a frame; the diagnostic must never defeat
    // the present gate merely to repaint itself.
    #[cfg(feature = "devtools")]
    let fps_shown = 0i32;
    // (media ns, SDL ticks) at the previous heartbeat, for `play=` below. `None` while
    // nothing is presenting, so the first beat of a playback reports no rate rather than a
    // fabricated one.
    let play_prev: Option<(i64, u32)> = None;
    let running = true;
    // Dev-only panel proof: advance a red/green counter phase only after SDL_GL_SwapWindow
    // returns. Hold each colour for 30 swaps: per-buffer alternation blends yellow at 60 Hz,
    // while this ~2 Hz change is human-visible and still freezes immediately with presentation.
    #[cfg(feature = "devtools")]
    let buffer_flip_count = 0u8;

    // All that is left of `HeldKey` on the loop's side (phase 10) — see `App::down_sym`.
    let down_sym = 0u32;
    // Item 13: rate-limits a hardware auto-repeat forwarded into the Settings family, which is
    // owned by the dispatcher since phase 5b — so the gate is applied at the loop's hand-over,
    // to the DIRECTIONS only. See `run`'s auto-repeat arm for why the OK edges go through
    // ungated, and `on_auto_repeat`'s doc for the one thing left on the legacy side (the
    // deferred press's liveness beat, and nothing else since phase 12).
    let modal_repeat = RepeatGate::IDLE;
    let player = plx_media::player::machine::Player::new();
    // The token stops being an argument here and becomes a field. `PlayerAdapter::new` CONSUMES
    // it, so the adapter is the only thing in the process that holds one, and a `&mut` to it is
    // what every native-session call now asks for. See `player::adapter`.
    let adapters = super::Adapters {
        player: plx_media::player::adapter::PlayerAdapter::new(mt),
    };
    // ui::press click state: a grid-card OK is deferred (press-in on down, activate on the
    // spring-back after key-up) so `ok_armed` marks "a press is in flight, commit it from the
    // per-frame loop when press::take_commit fires". Only ever set on Home's grid.
    let ok_armed = false;
    // Which route name was last REPORTED as an event. Not `route` itself: several `Route`
    // values share one name (every `Route::Player` is "player"), and an overlay
    // opening is not a screen change.
    let last_route_reported: &'static str = "";
    let ptr = Pointer::IDLE;

    // The boot page: Setup, which hands over to the Guide once it has read a stored account.
    let route = AppArg::Setup;
    // A last-page bookmark left by an older install is retired unconditionally, so automated and
    // ordinary upgrades retire the same state.
    crate::coldstart::retire();
    // dev: `/tmp/dispatcharrtv-replay[=N]` — how many times a finished `dispatcharrtv-playurl`
    // playback may be started AGAIN (LG App Self Checklist #46, "replay after completion").
    //
    // A COUNTER re-arming `auto_tried`, rather than the latch being lifted: `auto_tried` also
    // guards the `autoplay`+`playidx` arm below, which does a `request_play_movie` +
    // `load_detail_now`, so an unconditionally re-armable latch would re-fetch a catalog item
    // on every player exit and loop a real playback forever. Bounded and opt-in instead — an
    // absent file is 0, which leaves every existing boot byte-identical, and every pipeline
    // case but the one that asks for a replay is untouched.
    //
    // Why the app needs this at all: the synthetic tier boots with NO Plex session, so after a
    // stream ends there is no detail page, no Play control and no key path back into the
    // player. Everything else was already in place — `teardown` clears the URL and `ended` on a
    // real stop, and `engine::start_bufferfeed` re-reads `player::playurl::playurl()` whenever
    // `route::url()` is empty — so a replay is a second trip through the entry below.
    let replay_left: u32 = replay_budget(crate::dev::scenarios::replay_trigger_value().as_deref());
    let prev = 0u32;
    let ev = [0u8; 128];
    // dev/testing remote: drain any tokens written to /tmp/dispatcharrtv-remote and push
    // them as synthetic key events BEFORE the poll loop, so they're consumed this frame
    // by the ONE real key handler (see crate::remote / tools/stream-screen.py).
    let remote = crate::remote::Remote::open();
    // A LAB package may opt into the outbound long-poll command channel. Start only now: curl
    // has been initialised and, unlike the earlier boot/discovery work, the SDL loop below is
    // ready to dispatch a delivered command within one frame. Compile-time no-op otherwise.
    crate::lab::start_control();
    let mut app = App {
        last_input,
        loop_t,
        iters_ct,
        loop_shown,
        #[cfg(feature = "devtools")]
        fps_shown,
        play_prev,
        running,
        window_activity: super::window_activity::WindowActivity::new(),
        #[cfg(feature = "devtools")]
        buffer_flip_count,
        down_sym,
        modal_repeat,
        diagnostics: crate::app::diagnostics::Diagnostics::default(),
        player,
        adapters,
        live_reconnect: super::playback::ReconnectBudget::default(),
        ok_armed,
        last_route_reported,
        ptr,
        prev,
        ev,
        remote,
        win,
        #[cfg(all(feature = "hostsim", target_os = "linux"))]
        soft_frame_pacing,
        t0,
        instr,
        measure_fault_logged: false,
        input: plx_ui::input::Input::new(),
        telemetry_guard: None,
        present: plx_machine::present::Present::new(),
        glass,
        // **The application's page stack runs the route DIP** (§6.2). It ran `Immediate` until
        // phase 12 while `ui::nav` held a second fader and the loop applied its own route change
        // at THAT floor; `PageDip` is the same schedule in the container that owns the op.
        pages,
        inputs: Vec::new(),
        bridge,
        // Every dev-trigger arm's own state (spec: `dev/scenarios.rs`'s module doc).
        scenarios: crate::dev::scenarios::Scenarios {
            replay_left,
            seek_gap_ms: 300,
            dev: crate::dev::scenarios::DevFlags { nobudget },
            ..Default::default()
        },
    };
    // dev: /tmp/dispatcharrtv-nobudget — put the ONE frame budget (the tree's, spec §2.2) into its
    // pre-phase-11 shape for the A/B's control leg. The tree exists now, which is why this is
    // here rather than beside the trigger read.
    //
    // The log line is gated on its own: the flag reaches here through a struct field, and the
    // optimizer does not always prove that field `false` in a release build, so the literal
    // shipped (`ci/check-package.py` failed on it). See `dev.rs`'s module doc.
    if app.scenarios.dev.nobudget {
        app.pages.budget = plx_ui::frame::Budget::pre_phase_11();
        #[cfg(feature = "devtriggers")]
        plx_base::eventlog::log("budget: pre-phase-11 admission (quota only) by /tmp/dispatcharrtv-nobudget");
    }
    // **ROOT THE TREE AT THE BOOT PAGE.** The `route` chosen above used to be a field of `App`
    // that `bridge::sync_page` mirrored onto the container on the loop's first frame; there is no
    // mirror, so the boot says what it wants once, here, the moment the tree exists. A `Root` on
    // an empty stack mints the first entry, which is what makes this a hard CUT — there is no
    // outgoing screen to dip.
    super::bridge::nav_root(&mut app.pages, route);
    Ok(app)
}

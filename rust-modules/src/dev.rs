//! The `/tmp` developer-trigger surface, behind one door.
//!
//! This app is driven headlessly by ~44 files under `/tmp/plxnative-*`: which screen to boot to,
//! which item to play, which URL to stream, whether to auto-press OK, which PMS token to use.
//! That is how `tests/run.py` and every capture scene work, and it is not going away.
//!
//! It must not exist in a public build. `/tmp` is the SHARED system `/tmp` in the production jail
//! too (mode 1777, both jail profiles), so on an ordinary user's TV every one of those files is a
//! behaviour switch any co-resident process can throw. Three are outright takeovers:
//! `plxnative-token` beats the signed-in session (`app.rs`'s boot gate), `plxnative-servers` hands
//! the app a whole additional server — an address AND the token to trust it with (see [`servers`])
//! — and `plxnative-url` replaces the stream the player feeds.
//!
//! So every read goes through [`plx_base::devtrig`] — the primitives (`flag`, `read`, `latched_flag!`,
//! `read_sample`, `no_wan`, …), a base-layer module no application type can leak into — and that
//! door is `#[cfg]`-gated on the `devtriggers` feature. In a `--no-default-features` build
//! `devtrig::flag` is `false` and `devtrig::read` is `None` at COMPILE time, so no trigger can be
//! armed. Storage and diagnostics still use runtime files; none are developer triggers. THIS
//! module is the application-layer half: the typed triggers only app code consumes, the directory
//! scans ([`any_trigger_present`], [`armed_triggers`]) and the one-shot boot instruments.
//!
//! That does NOT keep a trigger's NAME out of the binary. A branch behind `flag`/`read` usually
//! folds away, but not reliably: once the answer is carried through a struct field (the
//! `nobudget` flag on `DevFlags`), the optimizer may keep the branch and its string literals
//! in a release build, and `ci/check-package.py` fails the package because it greps the shipped
//! bytes for every trigger name this module's [`DIAG`] and `devtrig`'s `CONTROLLED` list. So every
//! statement whose literal names a trigger (a log line saying `/tmp/plxnative-…`) carries its own
//! `#[cfg(feature = "devtriggers")]`. Never rely on constant folding for this.
//!
//! Two rules for anything added later:
//!
//! 1. **Never open a `/tmp` path directly.** Read through `plx_base::devtrig`. The grep that audits
//!    this (`/tmp/plxnative-` outside this module and the unconditional log sinks) is the only
//!    thing keeping the property true. The two profiler logs are dev-only and listed in [`DIAG`]
//!    below.
//! 2. **A gate is not always a path.** `any_trigger_present` scans the whole directory and names
//!    no file at all — it was the one surface a literal-replacement sweep would have missed, and
//!    it silently changes which screen the app boots to. Structural surfaces that take no name
//!    (the capture listener's `INADDR_ANY` socket, the remote FIFO's `mkfifo`) are gated at their
//!    call sites in `app.rs` for the same reason.
//!
//! The unconditional LOG sinks are deliberately NOT here and stay in every build: they are creates, not
//! reads, they are how on-device crash triage works at all, and writing them is not a way for
//! another process to steer this one.
//!
//! **Since UI restructure phase 10, [`scenarios`] is where a read gets ACTED on.** Both modules
//! reach `/tmp` only through the one door, [`plx_base::devtrig`] (`flag`/`read`); `dev::scenarios`
//! gathers every ARM — the app-core code that calls through this door and reacts — that used to be scattered
//! across `app/boot.rs`, `app/run.rs`, `app/content.rs` and `app/mod.rs`, plus the per-arm state
//! (oscillator phases, retry latches) those arms used to keep on `App` itself. Read that module's
//! doc before adding a new trigger that `app/` consumes.

pub(crate) mod scenarios;


/// **Turn on the TELEVISION'S OWN GStreamer logging** — `/tmp/plxnative-gstlog`.
///
/// This is the only instrument that can see inside LG's Dolby Vision chain. That chain is
/// `dvbin` → `h265parse` → `dvsplitter` → {`lxvideodec`, `dvmdparse`} → `dualsequencer`, all of it
/// closed, and the app's own logs stop at `Feed()`. Decompilation established that
/// `mediapipeline::PlayerFactory::create()` calls `gst_debug_is_active()` and, when it is, honours
/// `GST_DEBUG_FILE_OVERWRITE` / `GST_DEBUG_FILE` by installing its own log function — so these four
/// variables are read by libpf itself and need no cooperation from us beyond setting them.
///
/// **Timing is the whole reason this is here and not later.** Neither `libpf` nor `libplayerAPIs`
/// imports `gst_init`; they use LG's lazy `gst_cool_init_check`, which does not run until a player
/// is created. `plex_run` is therefore comfortably early — but anything that arms this AFTER the
/// first `Load` would be setting variables nobody reads again.
///
/// An empty trigger takes the five Dolby categories at level 6; content overrides the whole
/// `GST_DEBUG` spec, so `dvbin:9,dualsequencer:9` or `*:3` both work. The log goes to the runtime
/// directory beside the event log.
///
/// **Not free.** Level 6 on five categories is a lot of formatted I/O on an ARM TV and it is not a
/// setting to leave armed while measuring anything about frame pacing.
#[cfg(feature = "devtriggers")]
pub(crate) fn arm_gst_logging() {
    let Some(spec) = plx_base::devtrig::read("gstlog") else { return };
    let spec = if spec.is_empty() {
        "dvbin:6,dvsplitter:6,dvsplitter_algo:6,dvmdparse:6,dualsequencer:6".to_string()
    } else {
        spec
    };
    let log = plx_base::paths::in_runtime_dir(plx_base::paths::runtime_file::GST);
    // SAFETY: single-threaded here by construction — `plex_run` has not yet minted a worker, and
    // this runs before SDL init. `set_var` is only unsound against a concurrent reader.
    std::env::set_var("GST_DEBUG", &spec);
    std::env::set_var("GST_DEBUG_FILE", &log);
    std::env::set_var("GST_DEBUG_FILE_OVERWRITE", "enable");
    std::env::set_var("GST_DEBUG_NO_COLOR", "1");
    plx_base::eventlog::log(&format!("gstlog: GST_DEBUG={spec} -> {}", log.display()));
}
#[cfg(not(feature = "devtriggers"))]
pub(crate) fn arm_gst_logging() {}

/// **Sign in to a Dispatcharr server headlessly and log what it serves** —
/// `/tmp/plxnative-dispatcharr`, three lines: server address, username, password.
///
/// The headless twin of the Setup screen (`screens::setup`), which stores what it signs in with;
/// this probe stores nothing. It runs [`plx_dispatcharr`]'s whole
/// sign-in (version probe, JWT, existing-or-new API key), the channel and group reads and the guide
/// on a worker, and writes the outcome to the event log as `dispatcharr:` lines. It never logs the
/// password or the key. Like `plxnative-servers` it carries a credential, which is why it is not
/// in [`DIAG`].
#[cfg(feature = "devtriggers")]
pub(crate) fn dispatcharr_probe() {
    use plx_base::eventlog::log;
    let Some(spec) = plx_base::devtrig::read("dispatcharr") else { return };
    let mut lines = spec.lines().map(str::trim);
    let (Some(address), Some(username), Some(password)) = (lines.next(), lines.next(), lines.next()) else {
        log("dispatcharr: /tmp/plxnative-dispatcharr needs three lines: address, username, password");
        return;
    };
    let server = match plx_dispatcharr::dispatcharr::Server::parse(address) {
        Ok(server) => server,
        Err(e) => return log(&format!("dispatcharr: address {address:?}: {e}")),
    };
    let (username, password) = (username.to_owned(), password.to_owned());
    plx_base::task::spawn_small("dispatcharr-probe", move || {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        for line in dispatcharr_report(&server, &username, &password, now) {
            log(&line);
        }
    });
}
#[cfg(not(feature = "devtriggers"))]
pub(crate) fn dispatcharr_probe() {}

/// The `dispatcharr:` event-log lines for one sign-in and listing. Split from the trigger so the
/// formatting is testable against a loopback server without `/tmp`.
#[cfg(any(feature = "devtriggers", test))]
fn dispatcharr_report(server: &plx_dispatcharr::dispatcharr::Server, username: &str, password: &str, now: i64) -> Vec<String> {
    use plx_dispatcharr::dispatcharr::{api::Account, auth};
    /// Enough rows to recognise the lineup; the count line says how many there are in all.
    const LISTED: usize = 25;
    let mut out = Vec::new();
    let signed = match auth::sign_in(server, username, password) {
        Ok(signed) => signed,
        Err(e) => {
            out.push(format!("dispatcharr: sign-in to {} failed: {e}", server.base()));
            return out;
        }
    };
    out.push(format!(
        "dispatcharr: signed in to {} version={} tested={} key={} plaintext={}",
        server.base(),
        signed.version.version,
        if signed.version.is_tested() { "yes" } else { "no" },
        if signed.key_generated { "generated" } else { "reused" },
        server.is_plaintext(),
    ));
    let account = Account { server: signed.server, api_key: signed.api_key };
    let groups = account.groups();
    match account.channels() {
        Ok(channels) => {
            out.push(format!(
                "dispatcharr: channels={} groups={}",
                channels.len(),
                groups.as_ref().map_or_else(|e| format!("error ({e})"), |g| g.len().to_string())
            ));
            let guide = match account.guide(&channels, now) {
                Ok(guide) => {
                    let w = guide.window().expect("a built guide has its window");
                    out.push(format!(
                        "dispatcharr: guide {}..{} programmes on {} of {} channels",
                        plx_dispatcharr::dispatcharr::time::format_iso8601(w.start),
                        plx_dispatcharr::dispatcharr::time::format_iso8601(w.end),
                        guide.channels_with_programmes(),
                        channels.len()
                    ));
                    Some(guide)
                }
                Err(e) => {
                    out.push(format!("dispatcharr: guide failed: {e}"));
                    None
                }
            };
            for c in channels.iter().take(LISTED) {
                let (on_now, up_next) = guide.as_ref().map_or((None, None), |g| g.now_next(c.id, now));
                let title = |p: Option<&plx_dispatcharr::dispatcharr::models::Programme>| p.map_or("-", |p| p.title.as_str()).to_owned();
                out.push(format!(
                    "dispatcharr: ch {:>6} {} uuid={} group={} epg={} logo={} now={:?} next={:?}",
                    c.number_label(),
                    c.name,
                    c.uuid,
                    c.channel_group_id.map_or("-".into(), |g| g.to_string()),
                    c.epg_data_id.map_or("-".into(), |e| e.to_string()),
                    c.logo_id.map_or("-".into(), |l| l.to_string()),
                    title(on_now),
                    title(up_next),
                ));
            }
        }
        Err(e) => out.push(format!("dispatcharr: channels failed: {e}")),
    }
    out
}

/// **Turn on libwayland-client's protocol log** — `/tmp/plxnative-wldebug`.
///
/// `WAYLAND_DEBUG` makes the client library print every request it sends and every event it
/// dispatches, each with a microsecond wall-clock stamp, to stderr (`plxnative-stderr.log`). It is
/// the one place the compositor's side of a present is visible from inside the app: when
/// `wl_buffer.release` and `wl_callback.done` actually arrive, against when the driver's
/// `attach`/`commit` went out. libwayland reads the variable in `wl_display_connect`, so this has
/// to run before SDL opens its video device. The stamps are `CLOCK_REALTIME`; the line logged
/// here carries the offset to `CLOCK_MONOTONIC`, which is what `FRAMEDROP`'s `mono=` is.
///
/// **Not free**: a dozen formatted lines a frame. Arm it for the leg that needs it, and read
/// pacing from another.
#[cfg(feature = "devtriggers")]
pub(crate) fn arm_wayland_debug() {
    if !plx_base::devtrig::flag("wldebug") {
        return;
    }
    // SAFETY (of the environment write): the caller (`app::pre_boot_diagnostics`) runs this before
    // `telemetry::boot`, whose `sentry_init` is the first step that starts threads (sentry-native's
    // "sentry-tele" pool; `lab::boot` starts none, its poll thread comes later from
    // `lab::start_control`). Checked against vendor/sentry-native-src/src/sentry_telemetry.c.
    // `arm_gst_logging`'s own write runs after it and is not changed here.
    std::env::set_var("WAYLAND_DEBUG", "client");
    let stamp = |clock| {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: a plain clock read into a local.
        unsafe { libc::clock_gettime(clock, &mut ts) };
        ts.tv_sec as i64 * 1_000_000 + ts.tv_nsec as i64 / 1000
    };
    let (real, mono) = (stamp(libc::CLOCK_REALTIME), stamp(libc::CLOCK_MONOTONIC));
    plx_base::eventlog::log(&format!("wldebug: WAYLAND_DEBUG=client realtime_us={real} mono_us={mono} offset_us={}", real - mono));
}
#[cfg(not(feature = "devtriggers"))]
pub(crate) fn arm_wayland_debug() {}

/// **Crash the app on purpose** — `plxnative-crashtest=<segv|abrt|bus|ill|trap|panic|unwind>`.
///
/// Not a feature. An INSTRUMENT for the instrument, and it exists because of the rule this repo
/// keeps re-learning: prove the instrument can see the thing before you read its silence, and
/// prove the recorder records before you trust an empty recording. The fallback C crash tracer is
/// always one witness; when crash consent and a DSN are present, Sentry Native's out-of-process
/// handler is a second witness that can safely inspect the stopped process. Until
/// 2026-08-29 nothing had ever exercised it deliberately — which is how it went seven weeks with a
/// re-raise that did not re-raise, silently costing every crash its `WIFSIGNALED` status — and only
/// that: no crashd backtrace was lost, because this firmware writes no core and so produces none.
/// The log looked perfectly normal either way, which is the whole reason it went seven weeks.
///
/// `segv` is a genuine null write, so the kernel raises the signal from a faulting instruction and
/// the record carries a real faulting PC and a real `si_addr`. The rest go through `raise`, which
/// proves five of the seven `sigaction` calls took but cannot produce a meaningful PC.
///
/// `panic` is a Rust panic inside an `extern "C"` CALLBACK — the shape of `ff::read_cb` under
/// libav: the hook writes its `*** RUST PANIC` line, the unwind stops at the callback's own
/// boundary, and the process aborts with `plex_run`'s frame intact. That leaves BOTH a panic
/// record and a native SIGABRT envelope, the pair `telemetry::crashreport` must send as the one
/// panic.
///
/// `unwind` panics straight in here instead, so the unwind crosses `plex_run`'s own frame before
/// aborting at its `extern "C"` boundary. That used to drop the telemetry `Guard` on the way past
/// and stop the native backend before the abort landed (dev set, 2026-09-19), so no envelope was
/// written. `Drop for Guard` (`telemetry::native`) now checks `std::thread::panicking()` and
/// returns early instead of tearing the backend down, so the backend stays armed through the
/// unwind and the boundary abort still produces a native SIGABRT envelope (PR #168). `panic`
/// remains the shape worth exercising for a real panic under libav; `unwind` is the regression
/// test for the `Guard` fix itself.
///
/// **Compiled out of a release build** with the rest of `devtriggers`, so a shipped binary has no
/// path to it at all. Called after telemetry boot (so native capture can be armed) but before SDL
/// or any screen is created, so it remains reachable when the fault being chased prevents UI boot.
#[cfg(feature = "devtriggers")]
pub(crate) fn crash_on_purpose() {
    let Some(kind) = plx_base::devtrig::read("crashtest") else {
        return;
    };
    if kind == "panic" {
        extern "C" fn callback() {
            panic!("crashtest: deliberate panic");
        }
        plx_base::eventlog::log("crashtest: DELIBERATE crash, kind=panic (aborts at an extern \"C\" callback)");
        callback();
        return;
    }
    if kind == "unwind" {
        plx_base::eventlog::log(
            "crashtest: DELIBERATE crash, kind=unwind (unwinds plex_run to its extern \"C\" boundary)",
        );
        panic!("crashtest: deliberate unwinding panic");
    }
    // The signal numbers are Linux's, written out rather than taken from a libc crate: this crate
    // binds no libc, and these five have been stable in the Linux ABI since it had one.
    let sig = match kind.as_str() {
        "" | "segv" => 11,
        "abrt" => 6,
        "bus" => 7,
        "ill" => 4,
        "trap" => 5,
        other => {
            plx_base::eventlog::log(&format!("crashtest: unknown kind {other:?} — not crashing"));
            return;
        }
    };
    // Logged BEFORE the fault, and flushed by `plx_base::eventlog::log`'s own O_APPEND write, so the event log
    // says the death was deliberate. Without this line a deliberate crash is indistinguishable
    // from the real one somebody is hunting.
    plx_base::eventlog::log(&format!(
        "crashtest: DELIBERATE crash, kind={kind} signal={sig}"
    ));
    if sig == 11 {
        // A real memory fault. `write_volatile` so the optimiser cannot decide a null write is
        // undefined and therefore removable — which it may, and then this proves nothing.
        unsafe { std::ptr::null_mut::<u8>().write_volatile(1) };
    }
    unsafe extern "C" {
        fn raise(sig: i32) -> i32;
    }
    unsafe { raise(sig) };
}
#[cfg(not(feature = "devtriggers"))]
pub(crate) fn crash_on_purpose() {}

/// `plxnative-softfloat`: the ARM half of `plx_machine::motion`'s differential claim (spec §4.2). Runs the
/// 4,096-operand table through this binary's own arithmetic, logs its hash beside the host's
/// pinned one, and writes the words to `plxnative-softfloat.tbl` in the runtime root so a
/// divergence can be diffed word by word. `make softfloat-probe` is the recipe.
#[cfg(feature = "devtriggers")]
pub(crate) fn softfloat_probe() {
    if !plx_base::devtrig::flag("softfloat") {
        return;
    }
    let host = plx_machine::motion::DIFFERENTIAL_HASH_HOST;
    let here = plx_machine::motion::differential_hash();
    plx_base::eventlog::log(&format!(
        "softfloat: n={} hash={here:#018x} host={host:#018x} {}",
        plx_machine::motion::DIFFERENTIAL_N,
        if here == host { "MATCH" } else { "DIVERGE" }
    ));
    let mut t = Vec::new();
    plx_machine::motion::differential_table(&mut t);
    let body: String = t.iter().map(|w| format!("{w:08x}\n")).collect();
    let path = plx_base::paths::runtime_dir().join("plxnative-softfloat.tbl");
    if let Err(e) = std::fs::write(&path, body) {
        plx_base::eventlog::log(&format!("softfloat: table write failed: {e}"));
    }
}
#[cfg(not(feature = "devtriggers"))]
pub(crate) fn softfloat_probe() {}

/// A test-only playback policy override from `plxnative-quality`.
///
/// The server matrix grades established direct-play/remux/transcode routes. It must not silently
/// become an Auto-HLS matrix because the television happened to persist that user preference in
/// an earlier run. This is deliberately an in-memory boot override: writing the session would
/// make a test change the owner's real preference. Unknown and empty values fail closed by
/// producing no override.
pub(crate) fn playback_quality_override() -> Option<plx_plex::plex::session::PlaybackQuality> {
    let value = plx_base::devtrig::read("quality")?;
    parse_playback_quality(&value)
}

/// The inverse of [`parse_playback_quality`], and it lives here so the two cannot drift.
///
/// The log line a test greps must carry the SAME string the trigger accepts. `Quality::label()`
/// is display text ("1080p \u{b7} 8 Mbps") and would make a case state its rung twice, in two
/// spellings, with nothing keeping them in step — which is the shape that rots.
pub(crate) fn quality_wire_name(q: plx_plex::plex::session::PlaybackQuality) -> &'static str {
    use plx_plex::plex::session::PlaybackQuality as Q;
    match q {
        Q::Auto => "auto",
        Q::Original => "original",
        Q::P1080High => "1080p_20_mbps",
        Q::P1080 => "1080p_8_mbps",
        Q::P720 => "720p_4_mbps",
        Q::P720Low => "720p_2_mbps",
        Q::P480 => "480p_720_kbps",
    }
}

fn parse_playback_quality(value: &str) -> Option<plx_plex::plex::session::PlaybackQuality> {
    use plx_plex::plex::session::PlaybackQuality;
    match value {
        "auto" => Some(PlaybackQuality::Auto),
        "original" => Some(PlaybackQuality::Original),
        "1080p_20_mbps" => Some(PlaybackQuality::P1080High),
        "1080p_8_mbps" => Some(PlaybackQuality::P1080),
        "720p_4_mbps" => Some(PlaybackQuality::P720),
        "720p_2_mbps" => Some(PlaybackQuality::P720Low),
        "480p_720_kbps" => Some(PlaybackQuality::P480),
        _ => None,
    }
}

/// **Switch the playback quality MID-PLAYBACK** — `plxnative-qualityswitch=[gap=<ms>,]<q>[,<q>…]`.
///
/// The one thing [`playback_quality_override`] above cannot do. That is a BOOT override: it decides
/// what the playback starts as and is read once. What a person actually does at the television is
/// start something, watch it, and then change the quality while it plays — which re-asks the
/// routing question against a stream already on screen, reloads if the answer moved, and (on the
/// way out of Auto) tears down a running ABR controller. None of that is reachable from a boot
/// value, and none of it was reachable from a test at all.
///
/// Same vocabulary as `plxnative-quality`, deliberately — one spelling of a rung across both
/// triggers, so a case cannot name a quality one of them accepts and the other silently ignores.
/// Same grammar as `plxnative-autoseek`: an optional leading `gap=<ms>` then comma-separated steps
/// fired one per gap. There is no default gap and none is invented: with a single step no cadence
/// exists to state, and a case wanting several states its own, in the manifest, where a reader can
/// see it beside the assertions it enables.
///
/// **This writes the stored preference, and that is not incidental.** `route::set_quality` persists
/// through `plex::session::update`, because a person picking a rung means it. A test therefore
/// leaves the value behind — on the `debug` flavour's own session, never the install you watch
/// with, and `tests/run.py` writes `plxnative-quality` on every server case, so the next boot
/// overrides whatever this left. Both halves are load-bearing; neither alone would make it safe.
fn parse_quality_switch_script(
    raw: &str,
) -> Option<(u32, Vec<plx_plex::plex::session::PlaybackQuality>)> {
    let mut steps: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let mut gap_ms = None;
    if let Some(g) = steps.first().and_then(|f| f.strip_prefix("gap=")) {
        gap_ms = Some(g.parse().ok()?);
        steps.remove(0);
    }
    // A sequence needs an authored cadence. Without one, several valid rungs would all fire on
    // successive render loops: technically ordered, but not the mid-playback interactions the
    // trigger claims to reproduce.
    if steps.len() > 1 && gap_ms.is_none() {
        return None;
    }
    // Fail the WHOLE script when one rung is unparseable. Running a valid subset would still
    // change playback to a sequence nobody requested, merely without inventing a default rung.
    let qs: Vec<_> = steps
        .iter()
        .map(|t| parse_playback_quality(t))
        .collect::<Option<Vec<_>>>()?;
    if qs.is_empty() {
        None
    } else {
        Some((gap_ms.unwrap_or(0), qs))
    }
}

pub(crate) fn quality_switch_script() -> Option<(u32, Vec<plx_plex::plex::session::PlaybackQuality>)> {
    parse_quality_switch_script(&plx_base::devtrig::read("qualityswitch")?)
}

/// One synchronized user Pause, optionally followed by Resume —
/// `plxnative-autopause=[delay=<ms>,][at=<ms>,][hold=<ms>]`.
///
/// An empty file preserves the original paused-HUD capture contract: pause at the player's
/// ordinary six-second dev gate and stay paused. A non-empty script may delay that edge and name a
/// finite accepted hold. `at` also holds the edge until the PUBLISHED playhead has reached that
/// media position (and, in the simulator, stops the clock sink exactly on it, so the pause freezes
/// that position: `ffi_host.rs::stop_clock_at`): a wall-clock delay pauses wherever the host's
/// scheduling has got playback to, which moves from run to run, and the documentation's player
/// figure wants the same frame and the same clock every time. Unknown/duplicate/invalid fields fail the whole trigger closed; silently
/// substituting a duration would exercise a different interleaving from the manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PauseScript {
    pub(crate) delay_ms: u32,
    pub(crate) hold_ms: Option<u32>,
    pub(crate) at_ms: Option<u32>,
}

fn parse_pause_script(raw: &str) -> Option<PauseScript> {
    if raw.trim().is_empty() {
        return Some(PauseScript {
            delay_ms: 0,
            hold_ms: None,
            at_ms: None,
        });
    }
    let mut delay_ms = None;
    let mut hold_ms = None;
    let mut at_ms = None;
    for field in raw
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
    {
        if let Some(value) = field.strip_prefix("delay=") {
            if delay_ms.is_some() {
                return None;
            }
            delay_ms = Some(value.parse().ok()?);
        } else if let Some(value) = field.strip_prefix("at=") {
            if at_ms.is_some() {
                return None;
            }
            at_ms = Some(value.parse().ok()?);
        } else if let Some(value) = field.strip_prefix("hold=") {
            if hold_ms.is_some() {
                return None;
            }
            let value: u32 = value.parse().ok()?;
            if value == 0 {
                return None;
            }
            hold_ms = Some(value);
        } else {
            return None;
        }
    }
    Some(PauseScript {
        delay_ms: delay_ms.unwrap_or(0),
        hold_ms,
        at_ms,
    })
}

pub(crate) fn pause_script() -> Option<PauseScript> {
    parse_pause_script(&plx_base::devtrig::read("autopause")?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dispatcharr_report_lists_the_lineup_and_never_the_credentials() {
        use plx_dispatcharr::dispatcharr::test_double::{curl_ready, dispatcharr};
        if !curl_ready() {
            return;
        }
        let (server, _) = dispatcharr(Some("SECRETKEY"));
        let noon = 1_791_979_200; // 2026-10-14T12:00:00Z, inside the double's grid
        let lines = super::dispatcharr_report(&server, "andrew", "hunter2", noon);
        assert!(lines[0].starts_with(&format!("dispatcharr: signed in to {} version=0.32.0 tested=yes key=reused", server.base())), "{lines:?}");
        assert_eq!(lines[1], "dispatcharr: channels=4 groups=1");
        assert_eq!(lines[2], "dispatcharr: guide 2026-10-14T11:00:00Z..2026-10-15T00:00:00Z programmes on 2 of 4 channels");
        assert_eq!(lines[3], format!("dispatcharr: ch {:>6} Movies uuid=0b6c1c1e-0000-4000-8000-000000000009 group=2 epg=13 logo=4 now=\"-\" next=\"-\"", "5"));
        assert!(lines[4].ends_with("now=\"Midday News\" next=\"Weather\""), "{lines:?}");
        assert_eq!(lines.len(), 7);
        assert!(lines.iter().all(|l| !l.contains("SECRETKEY") && !l.contains("hunter2")), "{lines:?}");
    }

    #[test]
    fn dispatcharr_report_names_a_failed_sign_in() {
        use plx_dispatcharr::dispatcharr::test_double::{curl_ready, spawn};
        if !curl_ready() {
            return;
        }
        let (server, _) = spawn(|_, path| match path {
            "/api/core/version/" => (200, r#"{"version":"0.32.0"}"#.into()),
            _ => (401, r#"{"detail":"No active account found with the given credentials"}"#.into()),
        });
        let lines = super::dispatcharr_report(&server, "andrew", "wrong", 0);
        assert_eq!(lines, [format!("dispatcharr: sign-in to {} failed: unauthorized (401)", server.base())]);
    }

    #[test]
    fn quality_trigger_accepts_only_persisted_policy_spellings() {
        use plx_plex::plex::session::PlaybackQuality;

        assert_eq!(
            super::parse_playback_quality("auto"),
            Some(PlaybackQuality::Auto)
        );
        assert_eq!(
            super::parse_playback_quality("original"),
            Some(PlaybackQuality::Original)
        );
        assert_eq!(
            super::parse_playback_quality("720p_4_mbps"),
            Some(PlaybackQuality::P720)
        );
        for invalid in ["", "Auto", "720p", "unlimited"] {
            assert_eq!(super::parse_playback_quality(invalid), None, "{invalid}");
        }
    }

    /// A DIRECTORY whose name matches the trigger prefix must not read as an armed trigger.
    ///
    /// The failure it prevents is silent and permanent: `any_trigger_present` suppresses the boot
    /// who's-watching picker, so a squatted entry changes which screen this install comes up on
    /// with nothing logged anywhere. It became reachable when two installs started sharing `/tmp`
    /// — the second install's runtime root is a directory sitting right there.
    // `is_armed_trigger` itself is compiled out entirely under `--no-default-features` (it is
    // `devtriggers`-only, not merely dead code behind the runtime `ENABLED` check below), so this
    // test cannot exist in that build at all rather than just skip at runtime.

    // ---- plxnative-playurl (the pipeline test tier's one trigger) ----

    /// **One vocabulary for a rung, in both directions.** `plxnative-quality` and
    /// `plxnative-qualityswitch` accept these strings and `quality: switch → …` prints them, so a
    /// case states its rung once and asserts on the same word. A one-way table would let the log
    /// drift from what the trigger accepts and the drift would show up as a case that arms
    /// correctly and then matches nothing — indistinguishable from the feature not working.
    #[test]
    fn every_quality_wire_name_parses_back_to_itself() {
        use plx_plex::plex::session::PlaybackQuality as Q;
        for q in [
            Q::Auto,
            Q::Original,
            Q::P1080High,
            Q::P1080,
            Q::P720,
            Q::P720Low,
            Q::P480,
        ] {
            let name = super::quality_wire_name(q);
            assert_eq!(
                super::parse_playback_quality(name),
                Some(q),
                "{name} does not round-trip"
            );
        }
    }

    /// The script grammar, including the two ways it must FAIL CLOSED. A typo that resolved to a
    /// default would switch the playback to something the case never asked for — the same hazard
    /// `plxnative-abrpin` documents for its own value.
    #[test]
    fn a_quality_script_fails_closed_and_never_substitutes() {
        use plx_plex::plex::session::PlaybackQuality as Q;
        let parse = super::parse_quality_switch_script;
        assert_eq!(
            parse("720p_4_mbps"),
            Some((0, vec![Q::P720])),
            "one step needs no cadence"
        );
        assert_eq!(
            parse("gap=9000,1080p_8_mbps,auto"),
            Some((9_000, vec![Q::P1080, Q::Auto])),
            "a leading gap is consumed, not treated as a rung",
        );
        assert_eq!(
            parse("gap=9000,nonsense"),
            None,
            "a typo must arm NOTHING, not a default"
        );
        assert_eq!(
            parse("gap=oops,auto"),
            None,
            "an invalid cadence must not become zero"
        );
        assert_eq!(
            parse("720p_4_mbps,auto"),
            None,
            "a multi-step script must state its cadence",
        );
        assert_eq!(parse(""), None);
        assert_eq!(
            parse("nonsense,auto"),
            None,
            "one invalid rung must not leave a valid subset running",
        );
    }

    #[test]
    fn a_pause_script_preserves_the_legacy_hold_and_fails_closed() {
        let parse = super::parse_pause_script;
        assert_eq!(
            parse(""),
            Some(super::PauseScript {
                delay_ms: 0,
                hold_ms: None,
                at_ms: None,
            }),
            "the empty screenshot trigger remains a permanent Pause",
        );
        assert_eq!(
            parse("delay=25000,hold=6000"),
            Some(super::PauseScript {
                delay_ms: 25_000,
                hold_ms: Some(6_000),
                at_ms: None,
            }),
        );
        assert_eq!(
            parse("at=432000"),
            Some(super::PauseScript {
                delay_ms: 0,
                hold_ms: None,
                at_ms: Some(432_000),
            }),
            "a position-gated pause: the published playhead, not a wall-clock delay",
        );
        assert_eq!(parse("at=1,at=2"), None);
        assert_eq!(parse("at=soon"), None);
        assert_eq!(parse("hold=0"), None);
        assert_eq!(parse("delay=10,delay=20"), None);
        assert_eq!(parse("hold=oops"), None);
        assert_eq!(parse("resume=6000"), None);
    }
}

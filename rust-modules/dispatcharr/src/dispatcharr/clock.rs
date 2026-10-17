//! **The server's clock, for a television whose own one cannot be trusted.**
//!
//! The guide asks the server for programmes around "now", and a live tune records when it began.
//! Both read the television's wall clock, and on the dev set (webOS 6.5.3, 2026-10-17) that clock
//! read 2021-01-01 for over an hour after a boot — the guide came back with no programme on any of
//! 63 channels, because it had asked for a day five years gone. Nothing on the set corrected it.
//!
//! Every Dispatcharr response carries a `Date` header ([`plx_net::net::last_response_date`]), and
//! the request door ([`super::client::send`]) records the difference between it and the device
//! clock. [`now`] is the device clock while the two agree within [`SKEW_TOLERANCE_S`], and the
//! device clock moved by that difference when they do not. The tolerance is wide on purpose: a
//! request's own latency and the one-second resolution of `Date` are noise, and a correct device
//! clock is left alone.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// How far the device clock may be from the server's before the server's is used.
pub const SKEW_TOLERANCE_S: i64 = 300;

/// The server's clock minus the device's, from the most recent dated response.
static OFFSET: AtomicI64 = AtomicI64::new(0);
/// Whether any dated response has been seen.
static KNOWN: AtomicBool = AtomicBool::new(false);
/// Whether the correction is in use, so the change is logged once rather than per request.
static APPLIED: AtomicBool = AtomicBool::new(false);

/// The television's own wall clock, in Unix seconds.
pub fn device_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// "Now" in Unix seconds: the device clock, corrected by the server's when they disagree.
pub fn now() -> i64 {
    let offset = KNOWN.load(Ordering::Acquire).then(|| OFFSET.load(Ordering::Acquire));
    corrected(device_now(), offset)
}

/// Record a server `Date` (Unix seconds) read just now.
pub(super) fn observe(server: i64) {
    let offset = server - device_now();
    OFFSET.store(offset, Ordering::Release);
    KNOWN.store(true, Ordering::Release);
    let skewed = offset.abs() > SKEW_TOLERANCE_S;
    if APPLIED.swap(skewed, Ordering::AcqRel) != skewed {
        plx_base::eventlog::log(&if skewed {
            format!("dispatcharr: the television's clock is {offset}s from the server's; using the server's")
        } else {
            "dispatcharr: the television's clock agrees with the server's again".to_owned()
        });
    }
}

/// The policy: `device` moved by `offset` only when an offset is known and exceeds the tolerance.
fn corrected(device: i64, offset: Option<i64>) -> i64 {
    match offset {
        Some(o) if o.abs() > SKEW_TOLERANCE_S => device + o,
        _ => device,
    }
}

/// Forget every observation (a test's starting state).
#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    KNOWN.store(false, Ordering::Release);
    OFFSET.store(0, Ordering::Release);
    APPLIED.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::{corrected, SKEW_TOLERANCE_S};

    #[test]
    fn a_device_clock_within_the_tolerance_is_left_alone() {
        assert_eq!(corrected(1_000_000, None), 1_000_000, "nothing observed yet");
        assert_eq!(corrected(1_000_000, Some(SKEW_TOLERANCE_S)), 1_000_000);
        assert_eq!(corrected(1_000_000, Some(-30)), 1_000_000);
    }

    #[test]
    fn a_device_clock_years_off_follows_the_server() {
        // The dev set, 2026-10-17: device 2021-01-01T00:07:52Z, server 2026-10-17T08:25:21Z.
        let (device, server) = (1_609_459_672, 1_792_225_521);
        assert_eq!(corrected(device, Some(server - device)), server);
        assert_eq!(corrected(device + 60, Some(server - device)), server + 60, "and keeps running");
        assert_eq!(corrected(2_000_000_000, Some(-400)), 2_000_000_000 - 400, "a clock running ahead too");
    }
}

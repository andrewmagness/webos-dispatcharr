use super::*;

/// End-to-end delivery observations. The source and HLS estimators are deliberately separate:
/// a source probe and an HLS segment exercise different PMS work, but each can seed the other
/// only as an explicitly labelled weak prior, never as an interchangeable measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapacityEstimate {
    pub fast_kbps: u32,
    pub slow_kbps: u32,
    pub uncertainty_pm: u32,
    pub samples: u32,
}

impl CapacityEstimate {
    pub fn update(&mut self, observation: CapacityObservation) {
        // **AN ABANDONED TRANSFER IS NOT A CAPACITY OBSERVATION IN EITHER DIRECTION.**
        //
        // The rule is not a safety margin; it is what the observation MEANS. A fetch is abandoned
        // because its projected remainder did not fit the reserve — the event is evidence that the
        // link is INSUFFICIENT. Its bytes, meanwhile, are the receive buffer's opening burst
        // draining, measured over a few hundred microseconds, so as an estimate of SUSTAINED
        // capacity they are biased upward by construction. Reading them as "the link is fast"
        // inverts the meaning of the very event that produced them.
        //
        // Marking the sample incomplete was not enough, and the device showed exactly why. That
        // only raised `uncertainty_pm`, while the rate still entered — and a prefix four times the
        // history trips `is_regime_change` below, which RESTARTS the estimate at the prefix's own
        // value. So each abort reset the estimate upward: measured on `pipe_abr_down_outrun` with
        // the shaper holding **500 kbps**, the estimate walked 5 632 -> 28 744 -> 101 078 kbps
        // across successive aborts, and a 50% uncertainty discount on 101 Mbit/s is still two
        // orders of magnitude above the truth. Every downshift then chose an unaffordable target,
        // overran, aborted, and fed the estimate again.
        //
        // A slow prefix is censored by the same deadline as a fast one.  It proves that THIS
        // acquisition did not finish inside its runway; it does not identify whether the missing
        // service was path capacity, server production, pacing, startup, or a transient read.  Its
        // consumer handles that deadline event transactionally.  Turning either sign into a point
        // estimate is the category error behind the observed 16 -> 2 Mbit/s collapse.
        if !observation.completed {
            return;
        }
        // **A measurement a factor of four away from the history, in EITHER direction, is not the
        // same link.** Averaging across a regime change describes a link that never existed, and
        // the failure is not symmetric in cost: measured on the television 2026-08-25, an Original
        // recovery probe taken at 3,952 kbps while the shaped leg was still in force pinned the
        // estimate so hard that the next probe's 28,116 kbps blended to 9,993 — below the 10,800
        // requirement — and Auto never returned to Original at all. Two probes, seven times apart,
        // one verdict, and it was the wrong one. So a jump that large restarts the estimate at the
        // new value with a single sample's confidence, which is exactly what it is.
        if self.samples > 0 && observation.is_regime_change(self) {
            *self = Self::from_prior(observation.kbps);
            self.samples = 1;
            return;
        }
        let old_slow = self.slow_kbps;
        let old_fast = self.fast_kbps;
        let weight = observation.weight();
        self.slow_kbps = weighted_mean(old_slow, observation.kbps, weight, 8);
        self.fast_kbps = weighted_mean(old_fast, observation.kbps, observation.weight().min(2), 4);
        if self.samples == 0 {
            self.slow_kbps = observation.kbps;
            self.fast_kbps = observation.kbps;
            // **One measurement is one measurement.** A first sample starts at the maximum
            // discount and earns confidence as later samples AGREE with it — which is the whole of
            // "two successful probes" as a property of the estimate rather than a counter: a probe
            // at twice the requirement clears it alone, a marginal one has to be confirmed.
            self.uncertainty_pm = MAX_UNCERTAINTY_PM;
        } else {
            let spread = observation.kbps.abs_diff(self.slow_kbps);
            let relative = if self.slow_kbps == 0 {
                1_000
            } else {
                (u64::from(spread) * 1_000 / u64::from(self.slow_kbps)).min(1_000) as u32
            };
            // The floor falls as agreeing samples accumulate, and never to zero: a link that has
            // behaved for ten segments can still change in the eleventh.
            let sample_uncertainty = match self.samples {
                1 => 300,
                _ => 200,
            };
            self.uncertainty_pm = relative.max(sample_uncertainty);
            if observation.kbps < self.slow_kbps {
                let downside = self.slow_kbps - observation.kbps;
                self.uncertainty_pm = self
                    .uncertainty_pm
                    .max((downside.saturating_mul(2_000) / self.slow_kbps.max(1)).min(800));
            }
        }
        self.samples = self.samples.saturating_add(1);
    }

    /// Conservative budget used by both bootstrap and steady-state selection. Uncertainty is a
    /// discount, not a bonus: high dispersion means the lower part of the history, not its mean,
    /// is what a new encoder must survive.
    pub fn conservative_kbps(&self) -> u32 {
        let uncertainty = u64::from(self.uncertainty_pm.min(500));
        let discount = 1_000_u64.saturating_sub(uncertainty);
        (u64::from(self.slow_kbps) * discount / 1_000).min(u64::from(u32::MAX)) as u32
    }

    /// A sudden low fast estimate invalidates a high slow estimate. The slow value remains as a
    /// weak prior so recovery is possible, but a new candidate must fit the observed regime.
    ///
    /// **`saturating_mul`, matching [`CapacityObservation::is_collapse`]'s own arithmetic.** The
    /// bare `measured_kbps * 4` this replaced wraps above ~1.07 Gbit/s, and that is reachable: an
    /// **865 Gbit/s** reading is on record from this television, which is exactly why
    /// `clamped_to_evidence` further down this file exists. The two halves of the split are what
    /// make it worth fixing rather than filing — `overflow-checks` is ON under `cargo test`, so the
    /// host PANICS, and OFF in release, so the set WRAPS to a small number and declares a collapse
    /// on the fastest link it has ever seen. Neither is a rounding error.
    pub fn collapse(&mut self, measured_kbps: u32) {
        if self.fast_kbps > 0 && measured_kbps.saturating_mul(4) < self.fast_kbps {
            self.slow_kbps = measured_kbps.max(self.slow_kbps / 4);
            self.uncertainty_pm = 400;
        }
        self.fast_kbps = measured_kbps;
    }

    /// **Keep the number, throw away the confidence.** The estimate's value survives as a starting
    /// guess while its uncertainty goes to the maximum discount and its sample count collapses to
    /// one, so the very next real measurement dominates it.
    ///
    /// Three callers, three different reasons the history stopped describing the present, and they
    /// are listed here because "why is confidence being thrown away" is the question a reader of a
    /// surprising decision asks first:
    ///
    /// * **A bootstrap source probe seeding steady-state HLS.** Different request, different PMS
    ///   work, same link — evidence, but not an interchangeable measurement (see
    ///   [`bootstrap`]).
    /// * **A path change.** Local to Remote, Remote to Relay, a different server address: the
    ///   measurements were honest about a route that is no longer the one in use.
    /// * **A long pause.** A rate measured before a ten-minute pause describes a network that has
    ///   had ten minutes to change.
    pub fn demote_to_prior(&mut self) {
        if self.samples == 0 {
            return;
        }
        self.samples = 1;
        self.fast_kbps = self.slow_kbps;
        self.uncertainty_pm = self.uncertainty_pm.max(MAX_UNCERTAINTY_PM);
    }

    /// Age the estimate over a wall-clock gap in which nothing was measured. Below one half-life
    /// this is a graded widening of uncertainty; past four it is [`Self::demote_to_prior`],
    /// because at that point the estimate is a memory rather than a measurement.
    pub fn age_ms(&mut self, elapsed_ms: u64, policy: &AbrPolicy) {
        let half_life = u64::from(policy.stale_half_life_ms.max(1));
        if self.samples == 0 || elapsed_ms < half_life {
            return;
        }
        if elapsed_ms >= half_life.saturating_mul(4) {
            self.demote_to_prior();
            return;
        }
        // Each half-life closes half the remaining distance to the maximum discount: one gives
        // 250, two 375, three 437 — and the fourth is the demotion above.
        let halvings = u32::try_from(elapsed_ms / half_life)
            .unwrap_or(u32::MAX)
            .min(16);
        let widened = MAX_UNCERTAINTY_PM - (MAX_UNCERTAINTY_PM >> halvings);
        self.uncertainty_pm = self.uncertainty_pm.max(widened);
    }

    /// One measurement standing in for a history — a bootstrap probe, or a rate carried across a
    /// mode switch. Deliberately born at maximum uncertainty: it is a place to start, not a fact
    /// about the next ten minutes.
    /// **Rebuild an estimate from a snapshot of its own four fields** (I8) — the seek path, where
    /// the engine is destroyed and a fresh `Controller` is built on the other side.
    ///
    /// It is NOT `from_prior`, and the difference is the whole increment: `from_prior` pins
    /// `uncertainty_pm` at its cap and claims one sample, which is the correct reading of a
    /// bootstrap PROBE and a false one for an estimate that has just watched a link for a minute.
    /// Carrying the uncertainty and the sample count is what stops the ladder re-ramping.
    ///
    /// `samples == 0` is not an estimate and returns `None`; so is a zero rate, which is what an
    /// unwritten snapshot reads as.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_snapshot(
        slow_kbps: u32,
        fast_kbps: u32,
        uncertainty_pm: u32,
        samples: u32,
    ) -> Option<Self> {
        (samples > 0 && slow_kbps > 0).then_some(Self {
            fast_kbps,
            slow_kbps,
            uncertainty_pm: uncertainty_pm.min(MAX_UNCERTAINTY_PM),
            samples,
        })
    }

    pub fn from_prior(kbps: u32) -> Self {
        Self {
            fast_kbps: kbps,
            slow_kbps: kbps,
            uncertainty_pm: MAX_UNCERTAINTY_PM,
            samples: 1,
        }
    }
}

/// How much a single observation is allowed to move the estimate. The distinction is the plan's
/// "weight observations by quality", and it exists because throughput is a RATE: a 40 KiB read
/// that finished in 3 ms honestly reports 100 Mbit/s and proves nothing about the next second,
/// while two megabytes over 400 ms has actually held the link open long enough to be a claim about
/// sustained capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationQuality {
    /// Truncated, tiny, or over too short an interval to have left TCP's opening burst.
    Weak,
    /// A complete transfer of real size.
    Normal,
    /// Complete, megabyte-scale, and long enough to be a sustained rate.
    Strong,
}

/// One bounded delivery observation. Transfer duration, bytes and completion all matter: a tiny
/// partial read can honestly report a high instantaneous rate while proving nothing about
/// sustained capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacityObservation {
    pub kbps: u32,
    pub bytes: u64,
    pub active_us: u64,
    pub completed: bool,
}

impl CapacityObservation {
    /// **The interval floor applies to every tier, not only to `Strong`.** The two axes answer
    /// different questions and were being asked in the wrong order: the DURATION decides whether a
    /// rate measurement means anything at all, and the SIZE grades how much to trust one that
    /// does. `Strong` tested both and `Normal` tested size alone, so a transfer that was large but
    /// far too brief to measure — the exact case `clamped_to_evidence` exists for — was classified
    /// `Normal` and skipped the clamp.
    ///
    /// The consequence is not a slightly noisy estimate. Measured on the host simulator over a
    /// shaped 20 Mbit/s link (2026-08-28, leg D): after a link collapse and recovery the
    /// controller reached rung 2000 and never proposed another upshift for the rest of the run —
    /// no dwell, no reject block, no transaction attempted at all. A 2 s segment at that rung is
    /// ~500 KB, which crosses `NORMAL_OBSERVATION_BYTES` on size while arriving in ~200 us, so
    /// `network_kbps` read tens of millions of kbps unclamped. Consecutive samples read
    /// `18074, 18434921, 271729, 23303152, ...` kbps; each extreme reading inflated `fast_kbps`,
    /// the next honest one was then more than a factor of four below it, and `is_collapse` fired.
    /// In the retired order-statistic controller that was also the only acquisition-window reset:
    /// six resets kept it below 4 of the 19 samples then required for an upshift. The live finite
    /// bag no longer has that gate, but it still resets at actuator commits and at this explicit
    /// delivery change point so pre-collapse surplus cannot subsidize the new regime. The
    /// impossible rate also poisons bootstrap, Original comparison and diagnostics, so the
    /// validity floor remains load-bearing.
    ///
    /// **No new quantity is introduced.** `STRONG_OBSERVATION_US` is promoted from one half of the
    /// `Strong` test to the validity floor it always described, and `Strong` keeps its own meaning
    /// as "valid AND megabyte-scale". Below the floor a sample is `Weak`, which is what
    /// `ObservationQuality::Weak`'s own doc has always said — "truncated, tiny, **or over too
    /// short an interval to have left TCP's opening burst**" — and which routes it through
    /// `clamped_to_evidence`, so it claims `WEAK_SAMPLE_HEADROOM` times the rung it was measured
    /// on rather than a fabricated ceiling. That is the geometric ramp the clamp was designed
    /// around: the rung climbs, its segments get bigger and slower, and they become measurable.
    /// The cost of that ramp is real and is recorded beside the census assertion in `sim.rs`.
    pub fn quality(self) -> ObservationQuality {
        if !self.completed || self.active_us < MEASURABLE_OBSERVATION_US {
            return ObservationQuality::Weak;
        }
        if self.bytes >= STRONG_OBSERVATION_BYTES {
            ObservationQuality::Strong
        } else if self.bytes >= NORMAL_OBSERVATION_BYTES {
            ObservationQuality::Normal
        } else {
            ObservationQuality::Weak
        }
    }

    pub fn weight(self) -> u32 {
        match self.quality() {
            ObservationQuality::Weak => 1,
            ObservationQuality::Normal => 2,
            ObservationQuality::Strong => 3,
        }
    }

    /// A factor-of-four gap from the SLOW estimate in either direction — the test
    /// [`CapacityEstimate::update`] restarts on. The downward half overlaps
    /// [`Self::is_collapse`] deliberately: that one pins the FAST estimate the moment a collapse is
    /// seen (the controller's fast-down path reads it), while this one decides whether the history
    /// is still describing the present at all.
    pub fn is_regime_change(self, prior: &CapacityEstimate) -> bool {
        if !self.completed || prior.slow_kbps == 0 || self.kbps == 0 {
            return false;
        }
        self.kbps.saturating_mul(REGIME_FACTOR) < prior.slow_kbps
            || prior.slow_kbps.saturating_mul(REGIME_FACTOR) < self.kbps
    }
}

/// How far a measurement has to be from the history before the history is treated as describing a
/// different link. Four is deliberately coarse: ordinary variance on a healthy link is well inside
/// it, so this fires on a shaped leg starting or ending, not on jitter.
pub const REGIME_FACTOR: u32 = 4;

/// The largest discount [`CapacityEstimate::conservative_kbps`] will apply, and therefore the
/// value a demoted prior carries: half the estimate. A cap is needed at all because the discount
/// multiplies — an uncapped one would drive a volatile link's budget to zero and park Auto on the
/// emergency floor for the rest of the film.
pub const MAX_UNCERTAINTY_PM: u32 = 500;
pub const STRONG_OBSERVATION_BYTES: u64 = 1_048_576;
/// **The interval below which a transfer reports latency rather than capacity**, and therefore the
/// floor on a rate measurement being admissible at all. It was `STRONG_OBSERVATION_US` and was
/// asked only alongside `STRONG_OBSERVATION_BYTES`; `CapacityObservation::quality` now asks it
/// first, for every tier, which is what `ObservationQuality::Weak`'s doc always claimed. The value
/// is unchanged — this is a promotion, not a new threshold.
pub const MEASURABLE_OBSERVATION_US: u64 = 250_000;
pub const NORMAL_OBSERVATION_BYTES: u64 = 256 * 1024;

pub fn weighted_mean(old: u32, new: u32, weight: u32, denominator: u64) -> u32 {
    if old == 0 {
        return new;
    }
    let new_weight = u64::from(weight.min(u32::try_from(denominator).unwrap_or(u32::MAX)));
    ((u64::from(old) * (denominator - new_weight) + u64::from(new) * new_weight) / denominator)
        .min(u64::from(u32::MAX)) as u32
}


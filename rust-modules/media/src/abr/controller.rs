use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // retired reason codes remain in the diagnostics wire enum
pub enum HlsReason {
    SafeBudgetIncrease,
    UnsafeCurrentState,
    ProductionConstraint,
    BufferConstraint,
    /// **The terminal case: the downshift trigger fired and there is no rung below.** R12 — "a
    /// predicate with no action in the region that motivates it."
    ///
    /// It is not a new behaviour. `self.current.below()` returns `self.current` at the ladder
    /// floor, so the proposal was already skipped and `Stay` was already the answer; there is no
    /// other answer, because the escape this trigger exists to take does not exist at the bottom
    /// of the ladder. What was missing is that it said nothing. The line read `decision=stay
    /// reason=None`, identical to a healthy segment, and telling the two apart meant
    /// cross-referencing `current=` against the ladder floor by hand — on the one state where the
    /// controller has exhausted everything it can do and the picture is about to stop.
    LadderFloor,
    /// Retired HLS reason retained in the diagnostics wire vocabulary. HLS eviction now uses the
    /// exact finite-bag conservation/runway test; starvation horizons remain on the Original and
    /// utility telemetry paths.
    StarvationHorizon,
    /// Every feasible higher rung has a failure certificate that the current exact exploration
    /// surplus and live delivery distribution cannot release, or one transaction exhausted the
    /// common budget available to every quality excitation without producing an ordinal response
    /// endpoint. No wall-clock dwell exists; only strictly more measured surplus, confidence-
    /// separated service evidence, or a new controller can change the frontier.
    RejectBackoff,
    /// The finite bag at the current operating point is not sustainable, or the playable reserve
    /// contains no surplus above its exact rollback runway.  A smaller response can never veto a
    /// larger request by pretending to measure unused path capacity; this reason says only that
    /// there is currently no physically disposable reserve with which to perform that request.
    EvidenceWindow,
    /// A fetch was abandoned at an observed terminal reserve boundary or at its transaction's
    /// explicit deadline. Its prefix is censored: it says that this acquisition did not finish
    /// before that boundary, not that the prefix rate is the path capacity. Roll back to the last
    /// working actuator (or one rung when there is no transaction to undo) without feeding that
    /// prefix to either estimator.
    DeadlineRollback,
    /// **The reserve is not knowable on this sample**, so there is nothing to decide against —
    /// `buffered_ms()` is `None` for exactly one situation, an A/V session whose audio lane has
    /// produced no timestamp since the open or the seek.
    ///
    /// It is the one Stay that is the ABSENCE of a policy rather than the application of one, and
    /// it is worth a code because it is otherwise indistinguishable on the line from a healthy
    /// segment: the estimators have all taken the sample, so every other field looks normal.
    ReserveUnknown,
    /// No technically feasible rung exists above the current actuator.
    AtBestRung,
    /// **The upshift is admissible and does not pay for the picture change it would make.**
    ///
    /// Every committed rung change is a visible quality change to the person watching, and this
    /// playback has already made some. `AbrPolicy::visible_switch_penalty` prices the next one
    /// against them and halves that price every `visible_switch_decay_ms`, so the constraint is
    /// history rather than a cooldown counter: the first move is free, and one taken while three
    /// others are still recent has to buy a step of picture worth more than they cost. It is a
    /// Stay that names the VIEWER as the binding constraint rather than the link, which is why it
    /// is not folded into `RejectBackoff` — nothing about the network refused anything here.
    SwitchCost,
    /// The current actuator is already the largest request, but PMS returned a response whose
    /// decoded geometry is provably below that request/source.  It remains refreshable as the
    /// same actuator after strictly stronger completed-service evidence; the request ceiling is
    /// not promoted into a claim about delivered quality.
    ResponseLimited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub rung: Rung,
    pub direction: Direction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Stay,
    Prime(Proposal),
}


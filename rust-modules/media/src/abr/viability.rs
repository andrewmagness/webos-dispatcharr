use super::*;

/// The two weights [`risk_score`] sums. Named so that [`RISK_SCORE_MAX`] can be their sum rather
/// than a fourth number that has to be edited alongside them: the const's whole purpose is that the
/// panel cannot go on dividing by a stale denominator after a coefficient moves, and while the
/// coefficients were literals it did not achieve that — moving one still meant editing the 90 by
/// hand, which is the edit it exists to make unnecessary.
const RISK_NET: i64 = 40;
const RISK_BUFFER: i64 = 30;

/// The largest value [`risk_score`] can return. It is the DENOMINATOR the read-out renders with,
/// and it is here rather than as a literal at the render site.
pub const RISK_SCORE_MAX: u32 = (RISK_NET + RISK_BUFFER) as u32;

/// Historical unconditional second-segment budget retained for the archived plant and tests.
///
/// The live path does not call this helper. A boundary object which satisfies
/// `A <= D && B_post >= A` decides immediately; only a funded setup-bearing object with `A > D`
/// conditionally buys one ordinary observation from freshly credited reserve. This helper preserves
/// the former unconditional `min(D, reserve)` calculation solely for compatibility fixtures.
#[allow(dead_code)]
pub fn candidate_prime_budget(
    media_duration: std::time::Duration,
    _policy: &AbrPolicy,
    reserve: std::time::Duration,
) -> std::time::Duration {
    // Real time is the physical boundary: a JIT encoder whose completed steady segment costs more
    // wall time than the media it contributes drains reserve indefinitely.  `1000 pm` is a unit
    // identity, not a product margin, so express it directly as the duration itself.
    media_duration.min(reserve)
}

/// Historical `E_tx` compatibility readout retained for archived transaction-ledger tests.
///
/// Live exploration has no fixed post-transaction time debt: its initial phase spends the exact
/// playhead-funded surplus, a conditional setup-bearing continuation is funded only after real
/// media credit, and failure release requires strictly more actually executed disposable reserve.
/// Returning zero prevents an old caller from reintroducing a second time charge after those
/// physical clocks have already accounted for the transaction.
#[allow(dead_code)] // compatibility seam for historical transaction-ledger tests
pub fn upshift_transaction_cost(
    _media_duration: std::time::Duration,
    _policy: &AbrPolicy,
) -> std::time::Duration {
    // Retained as a compatibility/read-out seam. Exploration no longer has a time debt: its
    // transaction is bounded by spendable reserve and another attempt is released by strictly
    // more spendable reserve, not by waiting a computed interval.
    std::time::Duration::ZERO
}


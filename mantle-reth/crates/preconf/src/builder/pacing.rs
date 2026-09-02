//! How fast the build loop is allowed to admit pool transactions.
//!
//! One block's worth of gas, spread across the ticks still expected to fit
//! before the slot deadline, and released a batch at a time. Two shapes of it:
//! the sweep pacer preconf has always used, and the per-slice budget slicing
//! needs. The loop's pool arm sees only [`AdmissionPacer`] and does not know
//! which one it is talking to.
//!
//! Deliberately free of reth types and of wall-clock reads — the caller
//! measures the time and passes it in. Everything here is arithmetic over
//! `u64` and [`Duration`], which is what makes it unit-testable without a real
//! payload build, and what should let it survive a reth version jump untouched.

use std::time::Duration;

use crate::flashblocks::{Reservation, SlicePacer, SliceSchedule};

/// Derived schedule for the adaptive-N pool quota — see `build_payload`
/// Stage 3 setup. Extracted as a pure function so the (`time_drift`,
/// `sweep_interval`, `slot_duration`, `block_gas_limit`) →
/// (`ticks_remaining`, `gas_per_batch`, `first_offset`, `build_delay_ms`)
/// mapping is unit-testable without a real payload build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PoolQuotaSchedule {
    /// Time-until-slot-deadline used for the derivation, clamped to
    /// `[sweep_interval, slot_duration]`.
    pub(super) time_drift: Duration,
    /// Number of quota ticks fitting in `time_drift` (rounded up so a
    /// residual remainder gets its own tick). Always ≥ 1.
    pub(super) ticks_remaining: u64,
    /// Per-tick pool gas share — `block_gas_limit / ticks_remaining`.
    pub(super) gas_per_batch: u64,
    /// First tick offset — aligns subsequent ticks to `sweep_interval`
    /// boundaries within the slot. Equal to `sweep_interval` when the
    /// slot already sits on a boundary.
    pub(super) first_offset: Duration,
    /// Delay from the target slot start to now, in milliseconds.
    /// Useful for observability / alerting.
    pub(super) build_delay_ms: u64,
}

/// Compute the adaptive-N pool gas schedule from wall-clock inputs.
///
/// `time_drift_or_fallback` should be the caller's already-saturated
/// remaining-time-to-slot-deadline (falling back to `sweep_interval`
/// for late-FCU / clock-skew cases). This helper is deterministic
/// modulo integer arithmetic — no wall-clock reads inside.
pub(super) fn derive_pool_quota_schedule(
    time_drift_or_fallback: Duration,
    sweep_interval: Duration,
    slot_duration: Duration,
    block_gas_limit: u64,
) -> PoolQuotaSchedule {
    let time_drift = time_drift_or_fallback.min(slot_duration);
    let interval_ms = sweep_interval.as_millis().max(1) as u64;
    let drift_ms = time_drift.as_millis() as u64;
    let ticks_remaining = drift_ms.div_ceil(interval_ms).max(1);
    let gas_per_batch = block_gas_limit / ticks_remaining;
    let first_offset_ms = drift_ms.checked_rem(interval_ms).unwrap_or(0);
    let first_offset =
        if first_offset_ms == 0 { sweep_interval } else { Duration::from_millis(first_offset_ms) };
    let build_delay_ms = slot_duration.saturating_sub(time_drift).as_millis() as u64;
    PoolQuotaSchedule { time_drift, ticks_remaining, gas_per_batch, first_offset, build_delay_ms }
}

/// Adaptive-N pool-admission pacer — owns all pool-arm gas-pacing state for
/// one build. Groups the running consumption (`used`), the time-proportional
/// ceiling (`quota`, bumped one `per_batch` per sweep tick, capped at the
/// block gas limit) and the increment, so the select! loop's pool arm reads a
/// single object instead of scattered locals + a `LoopState` counter.
///
/// Distinct from `ExecutionInfo::cumulative_gas_used` (the all-source block
/// total): `used` tracks **only** the pool best-tx arm, so pacing is not
/// perturbed by preconf-tx or deposit gas.
#[derive(Debug)]
pub(super) struct PoolPacer {
    /// Gas admitted by the pool best-tx arm so far this build.
    used: u64,
    /// Current admission ceiling — pool txs admit while `used < quota`.
    quota: u64,
    /// Per-sweep-tick quota increment (`PoolQuotaSchedule::gas_per_batch`).
    per_batch: u64,
    /// Hard cap the quota is clamped to (the block gas limit).
    block_gas_limit: u64,
}

impl PoolPacer {
    /// Start with a drained quota (`0`) — the pool arm cannot admit until the
    /// first sweep tick raises the ceiling by `per_batch`.
    const fn new(per_batch: u64, block_gas_limit: u64) -> Self {
        Self { used: 0, quota: 0, per_batch, block_gas_limit }
    }

    /// Whether the pool arm may admit another tx under the current ceiling.
    const fn can_admit(&self) -> bool {
        self.used < self.quota
    }

    /// Record `delta` gas consumed by a just-admitted pool best-tx.
    fn record(&mut self, delta: u64) {
        self.used = self.used.saturating_add(delta);
    }

    /// Raise the admission ceiling by one batch on a sweep tick, clamped to
    /// the block gas limit.
    fn tick(&mut self) {
        self.quota = self.quota.saturating_add(self.per_batch).min(self.block_gas_limit);
    }
}

/// How many slice allowances should be open once `elapsed` has passed since
/// the first tick's grid point.
///
/// One per grid point already reached. Counting where the clock is rather than
/// how many ticks were delivered is what keeps a skipped tick from taking its
/// allowance with it: the block's budget is a function of the time left in the
/// slot, and time passes whether or not the loop got round to a tick.
pub(super) const fn allowances_due(elapsed: Duration, interval_ms: u128) -> u128 {
    1 + elapsed.as_millis() / interval_ms
}

/// Which budget the pool arm answers to.
///
/// The two are the same Adaptive-N arithmetic, but the slice pacer adds the
/// DA dimensions, charges at the declared gas limit and refunds the
/// difference, and opens holding one batch rather than none. Those are
/// improvements, and applying them with slicing switched off would make
/// turning the feature off a different code path rather than a rollback — so
/// each mode keeps its own.
#[derive(Debug)]
pub(super) enum AdmissionPacer {
    /// Slicing off: the pacer preconf has always used, untouched.
    Sweep(PoolPacer),
    /// Slicing on: per-slice budget across gas and both DA dimensions.
    Slice(SlicePacer),
}

/// A charge taken against [`AdmissionPacer`], to be settled or cancelled once
/// the transaction it covers has run.
#[derive(Debug)]
#[must_use = "a reservation has already been charged and must be resolved"]
pub(super) enum PacerTicket {
    Sweep,
    Slice(Reservation),
}

impl AdmissionPacer {
    /// The per-slice budget, for a build that publishes slices.
    pub(super) fn slicing(schedule: &SliceSchedule) -> Self {
        Self::Slice(SlicePacer::new(schedule))
    }

    /// The budget preconf has always used, for a build that does not.
    pub(super) const fn sweeping(per_batch: u64, block_gas_limit: u64) -> Self {
        Self::Sweep(PoolPacer::new(per_batch, block_gas_limit))
    }

    /// Whether the arm is worth polling at all.
    pub(super) fn has_headroom(&self) -> bool {
        match self {
            Self::Sweep(pacer) => pacer.can_admit(),
            Self::Slice(pacer) => pacer.has_headroom(),
        }
    }

    /// Open the next slice's allowance.
    pub(super) fn tick(&mut self) {
        match self {
            Self::Sweep(pacer) => pacer.tick(),
            Self::Slice(pacer) => pacer.tick(),
        }
    }

    /// Charge a transaction before running it. `None` means it does not fit.
    pub(super) fn reserve(&mut self, declared_gas: u64, da_bytes: u64) -> Option<PacerTicket> {
        match self {
            // The sweep pacer gates on the ceiling alone, never on the size of
            // the transaction in hand, so admission is already decided by
            // `has_headroom`.
            Self::Sweep(_) => Some(PacerTicket::Sweep),
            Self::Slice(pacer) => pacer.reserve(declared_gas, da_bytes).map(PacerTicket::Slice),
        }
    }

    /// Resolve a charge for a transaction that made it into the block.
    pub(super) fn settle(&mut self, ticket: PacerTicket, actual_gas: u64) {
        match (self, ticket) {
            (Self::Sweep(pacer), PacerTicket::Sweep) => pacer.record(actual_gas),
            (Self::Slice(pacer), PacerTicket::Slice(reservation)) => {
                pacer.settle(reservation, actual_gas);
            }
            // The mode is fixed for the whole build, so a ticket cannot reach a
            // pacer that did not issue it. Loud in tests, but not fatal in
            // production: dropping the charge under-fills a slice, which is a
            // far smaller failure than losing the block.
            (_, _) => debug_assert!(false, "a ticket outlived the pacer that issued it"),
        }
    }

    /// Resolve a charge for a transaction that did not make it into the block.
    pub(super) fn cancel(&mut self, ticket: PacerTicket) {
        match (self, ticket) {
            (Self::Sweep(_), PacerTicket::Sweep) => {}
            (Self::Slice(pacer), PacerTicket::Slice(reservation)) => pacer.cancel(reservation),
            // See `settle`.
            (_, _) => debug_assert!(false, "a ticket outlived the pacer that issued it"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flashblocks::{SliceLimits, derive_slice_schedule};

    const TEST_SLOT: Duration = Duration::from_millis(2000);
    const TEST_INTERVAL: Duration = Duration::from_millis(200);
    const TEST_BLOCK_GAS: u64 = 30_000_000;

    // ============ AdmissionPacer ============
    //
    // The pool arm answers to one of two budgets depending on whether slicing
    // is on. These pin that each mode keeps its own behaviour, because the
    // whole point of the split is that turning slicing off changes nothing.

    fn slice_schedule(leeway: Duration) -> SliceSchedule {
        derive_slice_schedule(
            TEST_SLOT,
            leeway,
            TEST_INTERVAL,
            TEST_SLOT,
            SliceLimits {
                block_gas_limit: TEST_BLOCK_GAS,
                block_da_limit: None,
                da_footprint_gas_scalar: None,
            },
        )
    }

    fn slice_pacer() -> AdmissionPacer {
        AdmissionPacer::slicing(&slice_schedule(Duration::from_millis(50)))
    }

    fn sweep_pacer() -> AdmissionPacer {
        AdmissionPacer::sweeping(3_000_000, TEST_BLOCK_GAS)
    }

    /// The sweep pacer gates on its ceiling alone and never on the size of the
    /// transaction in hand — exactly as it did before slicing existed, so a
    /// build with flashblocks off admits the same transactions in the same
    /// order.
    #[test]
    fn the_sweep_pacer_admits_without_regard_to_transaction_size() {
        let mut pacer = sweep_pacer();
        assert!(!pacer.has_headroom(), "starts drained");
        pacer.tick();

        let ticket = pacer.reserve(u64::MAX, u64::MAX).expect("size is not consulted");

        pacer.settle(ticket, 21_000);
        assert!(pacer.has_headroom(), "21k of a 3M allowance leaves room");
    }

    /// The slice pacer opens holding a batch, because the first slice is
    /// published before any tick has fired.
    #[test]
    fn the_slice_pacer_opens_with_an_allowance() {
        assert!(slice_pacer().has_headroom());
    }

    #[test]
    fn the_slice_pacer_refuses_what_does_not_fit_and_charges_nothing_for_it() {
        let mut pacer = slice_pacer();

        assert!(pacer.reserve(TEST_BLOCK_GAS, 0).is_none(), "a whole block does not fit a slice");

        let ticket = pacer.reserve(3_000_000, 0).expect("a batch does fit");
        pacer.settle(ticket, 3_000_000);
    }

    /// Charging at the declared limit would strand the rest of the slice if
    /// the refund did not come back.
    #[test]
    fn settling_returns_the_gas_a_transaction_declared_but_did_not_burn() {
        let mut pacer = slice_pacer();
        let ticket = pacer.reserve(3_000_000, 0).expect("fits");

        pacer.settle(ticket, 21_000);

        let next = pacer.reserve(2_900_000, 0);
        assert!(next.is_some(), "the unburnt gas is available to the next transaction");
        pacer.cancel(next.expect("just checked"));
    }

    /// A transaction that fails to execute must leave the budget as it found
    /// it, or a slice full of failures would admit nothing.
    #[test]
    fn cancelling_returns_the_whole_charge() {
        let mut pacer = slice_pacer();
        let ticket = pacer.reserve(3_000_000, 0).expect("fits");

        pacer.cancel(ticket);

        let again = pacer.reserve(3_000_000, 0);
        assert!(again.is_some(), "the slice is untouched");
        pacer.cancel(again.expect("just checked"));
    }

    #[test]
    fn a_tick_opens_the_next_slices_allowance_in_either_mode() {
        let mut sweep = sweep_pacer();
        let mut slice = slice_pacer();
        let opening = slice.reserve(3_000_000, 0).expect("the opening batch");
        slice.settle(opening, 3_000_000);
        assert!(!slice.has_headroom(), "drained");

        sweep.tick();
        slice.tick();

        assert!(sweep.has_headroom());
        assert!(slice.has_headroom());
    }

    // ============ allowances_due ============
    //
    // Slicing sets `MissedTickBehavior::Skip`, so a tick that arrives late
    // replaces the ones it missed rather than following them. The allowance
    // those ticks carried must not vanish with them: the budget is a function
    // of elapsed slot time, not of how many ticks the loop got round to.

    const INTERVAL_MS: u128 = 200;

    #[test]
    fn the_first_tick_opens_exactly_one_allowance() {
        assert_eq!(allowances_due(Duration::ZERO, INTERVAL_MS), 1);
    }

    #[test]
    fn a_tick_inside_its_own_window_opens_no_extra_allowance() {
        assert_eq!(allowances_due(Duration::from_millis(199), INTERVAL_MS), 1);
    }

    /// A tick delivered one interval late stands in for the one that was
    /// skipped, so it opens that allowance too.
    #[test]
    fn a_tick_one_interval_late_opens_the_skipped_allowance_as_well() {
        assert_eq!(allowances_due(Duration::from_millis(200), INTERVAL_MS), 2);
        assert_eq!(allowances_due(Duration::from_millis(500), INTERVAL_MS), 3);
    }

    /// The budget follows the clock, not the tick count: a run that lost most
    /// of its ticks has the same allowance open, by the same point in the slot,
    /// as one that got every single one.
    #[test]
    fn skipping_ticks_neither_loses_nor_duplicates_allowances() {
        let schedule = slice_schedule(Duration::ZERO);
        // Mirrors the tick arm: each delivered tick opens every allowance up to
        // where the clock now is, and never one twice.
        let opened_after = |delivered: &[u32]| {
            let mut opened = 0u128;
            let mut ticks = 0u128;
            for &grid_point in delivered {
                let due = allowances_due(TEST_INTERVAL * grid_point, INTERVAL_MS);
                for _ in opened..due {
                    ticks += 1;
                }
                opened = due;
            }
            ticks
        };

        let every_tick: Vec<u32> = (0..10).collect();
        let mostly_skipped = [0, 3, 9];

        assert_eq!(schedule.tick_count, 10);
        assert_eq!(opened_after(&every_tick), 10, "one allowance per grid point");
        assert_eq!(
            opened_after(&mostly_skipped),
            10,
            "three ticks reaching the same grid point open the same budget",
        );
    }

    // ============ derive_pool_quota_schedule ============
    //
    // Pure-function math tests for the adaptive-N pool quota schedule.
    // These are wall-clock free — the caller pre-computes `time_drift`,
    // so the helper is deterministic.

    /// No delay: full slot remaining. Schedule matches the "no-delay"
    /// case documented in the state-machine comment — 10 ticks × 3M
    /// each, first tick aligned to `sweep_interval`.
    #[test]
    fn quota_schedule_full_slot_produces_ten_ticks_of_three_million_each() {
        let s = derive_pool_quota_schedule(TEST_SLOT, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.ticks_remaining, 10);
        assert_eq!(s.gas_per_batch, 3_000_000);
        // Aligned drift → first tick equals full interval.
        assert_eq!(s.first_offset, TEST_INTERVAL);
        assert_eq!(s.build_delay_ms, 0);
        assert_eq!(s.time_drift, TEST_SLOT);
    }

    /// Delay 1s (1s remaining). Adaptive-N shrinks to 5 ticks × 6M each
    /// — pool still fills the whole block over the remaining window.
    #[test]
    fn quota_schedule_one_second_delay_produces_five_ticks_of_six_million_each() {
        let drift = Duration::from_millis(1000);
        let s = derive_pool_quota_schedule(drift, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.ticks_remaining, 5);
        assert_eq!(s.gas_per_batch, 6_000_000);
        assert_eq!(s.first_offset, TEST_INTERVAL); // 1000 % 200 == 0 → align to interval
        assert_eq!(s.build_delay_ms, 1000);
    }

    /// Non-aligned drift: `first_offset` shrinks to the remainder so
    /// every subsequent tick lands on an interval boundary within the
    /// slot. 900ms remaining → first tick after 100ms, then every
    /// 200ms → 5 ticks total: [100, 300, 500, 700, 900].
    #[test]
    fn quota_schedule_non_aligned_drift_uses_remainder_as_first_offset() {
        let drift = Duration::from_millis(900);
        let s = derive_pool_quota_schedule(drift, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.first_offset, Duration::from_millis(100));
        // ceil(900/200) = 5 ticks; each tick admits 6M.
        assert_eq!(s.ticks_remaining, 5);
        assert_eq!(s.gas_per_batch, 6_000_000);
        assert_eq!(s.build_delay_ms, 1100);
    }

    /// Extreme delay: less than one interval remaining. Schedule still
    /// admits one tick with the full block budget — a "single-shot"
    /// pool sweep at the end of the slot rather than degenerating to
    /// zero pool admission.
    #[test]
    fn quota_schedule_sub_interval_drift_yields_one_tick_full_budget() {
        let drift = Duration::from_millis(120);
        let s = derive_pool_quota_schedule(drift, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.ticks_remaining, 1);
        assert_eq!(s.gas_per_batch, TEST_BLOCK_GAS);
        // 120 % 200 = 120 → first tick after 120ms.
        assert_eq!(s.first_offset, Duration::from_millis(120));
        assert_eq!(s.build_delay_ms, 1880);
    }

    /// Late FCU / clock skew: caller has already fallen back to
    /// `sweep_interval` (its typical fallback). Schedule handles it
    /// gracefully — one tick, full budget, offset = interval.
    #[test]
    fn quota_schedule_fallback_to_sweep_interval_is_valid() {
        let s = derive_pool_quota_schedule(TEST_INTERVAL, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.ticks_remaining, 1);
        assert_eq!(s.gas_per_batch, TEST_BLOCK_GAS);
        assert_eq!(s.first_offset, TEST_INTERVAL);
    }

    /// Drift exceeds `slot_duration` (misconfigured attrs.timestamp far
    /// in the future). Clamped to `slot_duration` — no unbounded quota.
    #[test]
    fn quota_schedule_over_long_drift_clamps_to_slot_duration() {
        let drift = Duration::from_secs(60);
        let s = derive_pool_quota_schedule(drift, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.time_drift, TEST_SLOT);
        // Same as full-slot case.
        assert_eq!(s.ticks_remaining, 10);
        assert_eq!(s.gas_per_batch, 3_000_000);
    }

    /// Sum-invariant: `ticks_remaining × gas_per_batch` ≤
    /// `block_gas_limit` (integer division floor). Pool never
    /// over-admits by design; slight under-admission (up to
    /// `ticks_remaining - 1` gas due to floor) is acceptable and
    /// bounded.
    #[test]
    fn quota_schedule_total_admission_never_exceeds_block_gas() {
        for drift_ms in [100u64, 200, 500, 900, 1000, 1500, 1900, 2000] {
            let s = derive_pool_quota_schedule(
                Duration::from_millis(drift_ms),
                TEST_INTERVAL,
                TEST_SLOT,
                TEST_BLOCK_GAS,
            );
            let total_admitted = s.ticks_remaining.saturating_mul(s.gas_per_batch);
            assert!(
                total_admitted <= TEST_BLOCK_GAS,
                "drift_ms={drift_ms}: total {total_admitted} exceeds block gas {TEST_BLOCK_GAS}",
            );
            // Under-admission bound: at most (ticks_remaining - 1) gas
            // lost to floor rounding.
            let under = TEST_BLOCK_GAS - total_admitted;
            assert!(
                under < s.ticks_remaining,
                "drift_ms={drift_ms}: under-admission {under} exceeds ticks {ticks}",
                ticks = s.ticks_remaining,
            );
        }
    }

    // ============ PoolPacer (Adaptive-N pool admission pacing) ============

    /// Quota starts drained (0): no admission until the first sweep tick
    /// raises the ceiling by `per_batch`.
    #[test]
    fn pool_pacer_starts_drained_then_first_tick_opens_admission() {
        let mut p = PoolPacer::new(1_000, 5_000);
        assert!(!p.can_admit(), "quota starts at 0 → cannot admit before first tick");
        p.tick();
        assert!(p.can_admit(), "after one tick quota=1000 > used=0 → can admit");
    }

    /// `record` accumulates consumption; admission gates on `used < quota`
    /// (strict — boundary `used == quota` cannot admit, matching the old
    /// `pool_gas_used < pool_quota` guard).
    #[test]
    fn pool_pacer_record_accumulates_and_gates_admission() {
        let mut p = PoolPacer::new(1_000, 5_000);
        p.tick(); // quota = 1000
        p.record(600);
        assert!(p.can_admit(), "used 600 < quota 1000");
        p.record(400);
        assert!(!p.can_admit(), "used 1000 == quota 1000 → cannot admit (strict <)");
    }

    /// Ticks raise the ceiling by `per_batch` but never past the block gas
    /// limit.
    #[test]
    fn pool_pacer_tick_clamps_to_block_gas_limit() {
        let mut p = PoolPacer::new(4_000, 5_000);
        p.tick();
        assert_eq!(p.quota, 4_000);
        p.tick(); // 8000 → clamp to 5000
        assert_eq!(p.quota, 5_000);
        p.tick(); // stays clamped
        assert_eq!(p.quota, 5_000);
    }

    /// `record` saturates rather than overflowing.
    #[test]
    fn pool_pacer_record_saturates() {
        let mut p = PoolPacer::new(1, 1);
        p.record(u64::MAX);
        p.record(u64::MAX);
        assert_eq!(p.used, u64::MAX);
    }
}

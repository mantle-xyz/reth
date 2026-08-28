//! Per-slice budget: how much of a block one flashblock may carry.
//!
//! The block's limits are divided by however many slices are still expected
//! to fit before the slot deadline, and the resulting allowance accumulates
//! tick by tick. Deliberately free of wall-clock reads and of any reth type:
//! the caller measures the time left, everything after that is arithmetic.

use std::time::Duration;

/// The block-level limits a slice budget is carved out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceLimits {
    /// Gas ceiling for the whole block.
    pub block_gas_limit: u64,
    /// Data-availability bytes for the whole block, when configured.
    pub block_da_limit: Option<u64>,
    /// Post-Jovian footprint scalar. Footprint is charged in gas — DA bytes
    /// times this scalar — and shares the block gas ceiling.
    pub da_footprint_gas_scalar: Option<u16>,
}

/// How a block's limits are spread across the slices still to come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceSchedule {
    /// The block-level limits this schedule divides.
    pub limits: SliceLimits,
    /// Time left to build in, after leeway and clamping.
    pub drift: Duration,
    /// Slices the budget is divided by. Not a cap on how many slices get
    /// published: if the build started early, more ticks will fire than this,
    /// and those extra ones carry no per-slice limit.
    pub tick_count: u64,
    /// Gas each tick adds to the allowance.
    pub gas_per_batch: u64,
    /// DA bytes each tick adds, when a block DA limit is configured.
    pub da_per_batch: Option<u64>,
    /// Footprint gas each tick adds, when the scalar is active.
    pub da_footprint_gas_per_batch: Option<u64>,
    /// Delay before the first tick. Absorbs the remainder so every later tick
    /// lands on the interval grid, at the cost of a first window that may be
    /// shorter than one interval while still carrying a whole batch.
    pub first_offset: Duration,
    /// The deadline had already passed, so there was nothing to divide. The
    /// schedule degrades to a single tick carrying the whole block; callers
    /// should count this rather than let it pass unnoticed.
    pub drift_was_exhausted: bool,
}

/// Derive the slice budget from the time left in the slot.
///
/// `time_to_deadline` is the caller's measurement of how long until the
/// attributes' timestamp; `leeway` is how much of that to hold back so the
/// budgeted grid finishes before the deadline rather than on it.
pub fn derive_slice_schedule(
    time_to_deadline: Duration,
    leeway: Duration,
    interval: Duration,
    slot_duration: Duration,
    limits: SliceLimits,
) -> SliceSchedule {
    // Clamping to one slot keeps a wrong `attrs.timestamp` — clock skew, bad
    // config — from stretching the budget window without bound.
    let drift = time_to_deadline.saturating_sub(leeway).min(slot_duration);

    let interval_ms = interval.as_millis().max(1) as u64;
    let drift_ms = drift.as_millis() as u64;

    // Rounding up gives the sub-interval tail its own tick instead of
    // dropping it; the floor of one keeps a passed deadline buildable.
    let tick_count = drift_ms.div_ceil(interval_ms).max(1);

    // The remainder goes to the first tick so every later one lands on the
    // interval grid. An evenly dividing drift has no remainder to absorb and
    // takes a whole interval instead — except when the window has no length
    // at all, where waiting an interval would push the only tick past the
    // deadline it was created to serve.
    let remainder_ms = drift_ms % interval_ms;
    let first_offset = match remainder_ms {
        0 if drift.is_zero() => Duration::ZERO,
        0 => interval,
        remainder => Duration::from_millis(remainder),
    };

    SliceSchedule {
        limits,
        drift,
        tick_count,
        gas_per_batch: limits.block_gas_limit / tick_count,
        da_per_batch: limits.block_da_limit.map(|limit| limit / tick_count),
        da_footprint_gas_per_batch: limits
            .da_footprint_gas_scalar
            .map(|_| limits.block_gas_limit / tick_count),
        first_offset,
        drift_was_exhausted: drift.is_zero(),
    }
}

/// One dimension's running allowance: what has been spent, what has been
/// released so far, and the hard block-level stop.
#[derive(Debug, Clone, Copy)]
struct Budget {
    used: u64,
    ceiling: u64,
    per_batch: u64,
    block_limit: u64,
}

impl Budget {
    /// Opens holding one batch: the first slice is built before any tick has
    /// fired, so starting empty would leave it with nothing to spend and push
    /// every later slice one batch behind.
    const fn new(per_batch: u64, block_limit: u64) -> Self {
        let ceiling = if per_batch < block_limit { per_batch } else { block_limit };
        Self { used: 0, ceiling, per_batch, block_limit }
    }

    fn tick(&mut self) {
        self.ceiling = self.ceiling.saturating_add(self.per_batch).min(self.block_limit);
    }

    fn fits(&self, amount: u64) -> bool {
        self.used.saturating_add(amount) <= self.ceiling
    }

    fn record(&mut self, amount: u64) {
        self.used = self.used.saturating_add(amount);
    }

    fn refund(&mut self, amount: u64) {
        self.used = self.used.saturating_sub(amount);
    }
}

/// A charge already applied to a [`SlicePacer`], to be resolved once the
/// transaction it covers has run.
///
/// Gas is charged at the declared limit, since what a transaction actually
/// burns is not known until it has executed. Resolving it with
/// [`SlicePacer::settle`] hands back the difference;
/// [`SlicePacer::cancel`] hands back everything.
///
/// Losing one without resolving it leaves the budget charged at the declared
/// amount — under-filling a slice rather than over-filling it — and trips a
/// debug assertion so it does not go unnoticed in tests.
#[derive(Debug)]
#[must_use = "a reservation has already been charged and must be settled or cancelled"]
pub struct Reservation {
    declared_gas: u64,
    da_bytes: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // Only complain when this drop is the story. Asserting while the
        // thread is already unwinding turns any unrelated test failure into
        // a double panic, which aborts the whole process instead of failing
        // one case.
        if std::thread::panicking() {
            return;
        }
        debug_assert!(
            false,
            "reservation dropped without settle or cancel; the slice budget stays charged \
             at the declared {} gas",
            self.declared_gas
        );
    }
}

/// Gates what a slice may still take on, across gas and the two DA
/// dimensions.
///
/// Allowance accumulates rather than resetting per slice, so whatever one
/// slice leaves unused is available to the next.
#[derive(Debug)]
pub struct SlicePacer {
    gas: Budget,
    da: Option<Budget>,
    footprint: Option<Budget>,
    footprint_scalar: u64,
}

impl SlicePacer {
    /// Opens with one batch of allowance already available, so the slice
    /// published at the first tick carries a full share rather than nothing.
    pub fn new(schedule: &SliceSchedule) -> Self {
        let limits = schedule.limits;
        Self {
            gas: Budget::new(schedule.gas_per_batch, limits.block_gas_limit),
            da: schedule
                .da_per_batch
                .zip(limits.block_da_limit)
                .map(|(per_batch, block_limit)| Budget::new(per_batch, block_limit)),
            // Footprint is charged in gas and shares the block gas ceiling.
            footprint: schedule
                .da_footprint_gas_per_batch
                .map(|per_batch| Budget::new(per_batch, limits.block_gas_limit)),
            footprint_scalar: limits.da_footprint_gas_scalar.unwrap_or(0) as u64,
        }
    }

    /// Open the next slice's allowance by one more batch, up to the block
    /// limit.
    ///
    /// The tick count a schedule derives is a divisor for the budget, not a
    /// limit on how many slices a block may publish: a build that started
    /// early outruns its own schedule. Those extra slices are not starved,
    /// because by then the accumulated allowance has already reached the
    /// block limit and there is no per-slice ceiling left to apply.
    pub fn tick(&mut self) {
        self.gas.tick();
        if let Some(da) = self.da.as_mut() {
            da.tick();
        }
        if let Some(footprint) = self.footprint.as_mut() {
            footprint.tick();
        }
    }

    /// Whether any allowance is left at all.
    ///
    /// A cheap guard for callers that would otherwise spin: it says the
    /// budget is worth trying, not that any particular transaction fits.
    /// [`reserve`](Self::reserve) is the actual gate.
    pub fn has_headroom(&self) -> bool {
        self.gas.used < self.gas.ceiling
    }

    /// Charge a transaction against the allowance, at its declared gas limit.
    ///
    /// `None` when it does not fit — nothing is charged in that case.
    pub fn reserve(&mut self, declared_gas: u64, da_bytes: u64) -> Option<Reservation> {
        let footprint_gas = da_bytes.saturating_mul(self.footprint_scalar);

        let fits = self.gas.fits(declared_gas) &&
            self.da.as_ref().is_none_or(|budget| budget.fits(da_bytes)) &&
            self.footprint.as_ref().is_none_or(|budget| budget.fits(footprint_gas));
        if !fits {
            return None;
        }

        self.gas.record(declared_gas);
        if let Some(da) = self.da.as_mut() {
            da.record(da_bytes);
        }
        if let Some(footprint) = self.footprint.as_mut() {
            footprint.record(footprint_gas);
        }

        Some(Reservation { declared_gas, da_bytes })
    }

    /// Resolve a reservation for a transaction that made it into the block,
    /// handing back the gas it declared but did not burn.
    ///
    /// The DA charge stands: DA is measured from the encoding, so it was
    /// already exact when reserved.
    pub fn settle(&mut self, reservation: Reservation, actual_gas: u64) {
        let refund = reservation.declared_gas.saturating_sub(actual_gas);
        std::mem::forget(reservation);

        self.gas.refund(refund);
    }

    /// Resolve a reservation for a transaction that did not make it into the
    /// block, handing back everything it was charged.
    pub fn cancel(&mut self, reservation: Reservation) {
        let (declared_gas, da_bytes) = (reservation.declared_gas, reservation.da_bytes);
        std::mem::forget(reservation);

        self.gas.refund(declared_gas);
        if let Some(da) = self.da.as_mut() {
            da.refund(da_bytes);
        }
        if let Some(footprint) = self.footprint.as_mut() {
            footprint.refund(da_bytes.saturating_mul(self.footprint_scalar));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{SliceLimits, SlicePacer, derive_slice_schedule};

    const SLOT: Duration = Duration::from_secs(2);
    const INTERVAL: Duration = Duration::from_millis(200);
    const BLOCK_GAS: u64 = 30_000_000;

    fn gas_only() -> SliceLimits {
        SliceLimits {
            block_gas_limit: BLOCK_GAS,
            block_da_limit: None,
            da_footprint_gas_scalar: None,
        }
    }

    fn schedule(time_to_deadline: Duration, leeway: Duration) -> super::SliceSchedule {
        derive_slice_schedule(time_to_deadline, leeway, INTERVAL, SLOT, gas_only())
    }

    fn pacer() -> SlicePacer {
        SlicePacer::new(&schedule(SLOT, Duration::from_millis(50)))
    }

    /// Reserve and settle at the declared cost — the shape most tests want.
    fn spend(pacer: &mut SlicePacer, gas: u64, da_bytes: u64) -> bool {
        match pacer.reserve(gas, da_bytes) {
            Some(reservation) => {
                pacer.settle(reservation, gas);
                true
            }
            None => false,
        }
    }

    // ── schedule derivation ──────────────────────────────────────────────

    #[test]
    fn normal_start_yields_ten_ticks_of_three_million() {
        let s = schedule(SLOT, Duration::from_millis(75));

        assert_eq!(s.drift, Duration::from_millis(1925));
        assert_eq!(s.tick_count, 10);
        assert_eq!(s.gas_per_batch, 3_000_000);
        assert_eq!(s.first_offset, Duration::from_millis(125));
    }

    #[test]
    fn early_start_clamps_drift_to_slot_duration() {
        let s = schedule(Duration::from_millis(2400), Duration::from_millis(75));

        assert_eq!(s.drift, SLOT, "2325ms of headroom must clamp to one slot");
        assert_eq!(s.tick_count, 10);
        assert_eq!(s.gas_per_batch, 3_000_000);
        assert_eq!(s.first_offset, INTERVAL, "a zero remainder takes a whole interval");
    }

    #[test]
    fn late_start_halves_the_tick_count_and_doubles_the_batch() {
        let s = schedule(Duration::from_millis(950), Duration::from_millis(75));

        assert_eq!(s.drift, Duration::from_millis(875));
        assert_eq!(s.tick_count, 5, "the sub-interval tail still gets its own tick");
        assert_eq!(s.gas_per_batch, 6_000_000);
        assert_eq!(s.first_offset, Duration::from_millis(75));
    }

    #[test]
    fn the_default_leeway_shifts_the_grid_without_changing_the_tick_count() {
        let s = schedule(SLOT, Duration::from_millis(50));

        assert_eq!(s.tick_count, 10);
        assert_eq!(s.gas_per_batch, 3_000_000);
        assert_eq!(s.first_offset, Duration::from_millis(150));
    }

    #[test]
    fn a_deadline_already_past_still_yields_one_tick() {
        let s = schedule(Duration::ZERO, Duration::from_millis(50));

        assert_eq!(s.tick_count, 1);
        assert_eq!(s.gas_per_batch, BLOCK_GAS, "one tick means the whole block at once");
        assert!(s.drift_was_exhausted, "the caller needs to be able to count this");
        assert_eq!(
            s.first_offset,
            Duration::ZERO,
            "a zero-length window must open its one tick immediately, not an interval later"
        );
    }

    #[test]
    fn an_evenly_dividing_drift_still_takes_a_whole_first_interval() {
        let s = schedule(Duration::from_millis(250), Duration::from_millis(50));

        assert_eq!(s.drift, INTERVAL);
        assert_eq!(s.tick_count, 1);
        assert_eq!(s.first_offset, INTERVAL);
        assert!(!s.drift_was_exhausted);
    }

    // ── reserve / settle accounting ──────────────────────────────────────

    /// The first slice is built before any tick has fired, so it has to open
    /// holding a batch already. Starting drained would push every slice one
    /// batch behind and leave the last one reachable only in the leeway
    /// window.
    #[test]
    fn the_first_slice_opens_with_one_batch_available() {
        let mut pacer = pacer();

        assert!(pacer.reserve(3_000_001, 0).is_none());
        let reservation = pacer.reserve(3_000_000, 0).expect("one batch is available up front");
        pacer.settle(reservation, 3_000_000);
    }

    #[test]
    fn a_reservation_charges_the_declared_cost_up_front() {
        let mut pacer = pacer();

        let reservation = pacer.reserve(3_000_000, 0).expect("the opening batch fits");

        // Charged already: nothing else fits until this one settles.
        assert!(pacer.reserve(1, 0).is_none());
        pacer.settle(reservation, 3_000_000);
    }

    #[test]
    fn a_reservation_that_does_not_fit_is_refused() {
        let mut pacer = pacer();

        assert!(pacer.reserve(3_000_001, 0).is_none());
    }

    /// Gas is reserved at the declared limit and refunded down to what the
    /// transaction actually burned.
    #[test]
    fn settling_refunds_the_unused_gas() {
        let mut pacer = pacer();
        let reservation = pacer.reserve(3_000_000, 0).expect("fits");

        pacer.settle(reservation, 1_000_000);

        assert!(spend(&mut pacer, 2_000_000, 0), "the unburned 2M must come back");
        assert!(!spend(&mut pacer, 1, 0));
    }

    /// A transaction that never makes it into the block gives everything back.
    #[test]
    fn cancelling_refunds_the_whole_reservation() {
        let mut pacer = pacer();
        let reservation = pacer.reserve(3_000_000, 0).expect("fits");

        pacer.cancel(reservation);

        assert!(spend(&mut pacer, 3_000_000, 0));
    }

    /// The failure the ticket exists to prevent: losing a reservation without
    /// resolving it leaves the budget charged at the declared amount, which is
    /// the safe direction, and trips loudly where assertions are on.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "reservation dropped")]
    fn dropping_a_reservation_without_resolving_it_trips() {
        let mut pacer = pacer();
        pacer.tick();

        drop(pacer.reserve(3_000_000, 0).expect("fits"));
    }

    #[test]
    fn headroom_reports_whether_the_arm_is_worth_polling() {
        let mut pacer = pacer();
        assert!(pacer.has_headroom(), "the opening batch is spendable straight away");

        assert!(spend(&mut pacer, 3_000_000, 0));
        assert!(!pacer.has_headroom(), "batch spent");

        pacer.tick();
        assert!(pacer.has_headroom(), "the next tick reopens it");
    }

    // ── ceiling behaviour ────────────────────────────────────────────────

    #[test]
    fn the_ceiling_accumulates_and_carries_unused_budget() {
        let mut pacer = pacer();

        for _ in 0..3 {
            pacer.tick();
        }

        assert!(
            spend(&mut pacer, 12_000_000, 0),
            "the opening batch plus three untouched ticks are all available"
        );
        assert!(!spend(&mut pacer, 1, 0));
    }

    /// Also covers the past-budget branch with realistic limits: twenty ticks
    /// against a ten-tick schedule, and the block cap still holds.
    #[test]
    fn the_ceiling_never_exceeds_the_block_gas_limit() {
        let mut pacer = pacer();

        for _ in 0..20 {
            pacer.tick();
        }

        assert!(spend(&mut pacer, BLOCK_GAS, 0));
        assert!(!spend(&mut pacer, 1, 0));
    }

    /// The tick count divides the budget; it does not limit how many slices a
    /// block may publish. A build that started early keeps ticking past it,
    /// and those slices carry only the block-level limit.
    #[test]
    fn a_build_that_outruns_its_schedule_is_not_capped() {
        let s = schedule(SLOT, Duration::from_millis(50));
        let mut pacer = SlicePacer::new(&s);

        for _ in 0..=s.tick_count {
            pacer.tick();
        }

        assert!(spend(&mut pacer, BLOCK_GAS, 0), "the whole block is reachable");
        assert!(!spend(&mut pacer, 1, 0), "and the block limit still holds");
    }

    // ── the DA dimensions ────────────────────────────────────────────────

    #[test]
    fn da_bytes_are_paced_alongside_gas() {
        let limits = SliceLimits {
            block_gas_limit: BLOCK_GAS,
            block_da_limit: Some(1_000_000),
            da_footprint_gas_scalar: None,
        };
        let s = derive_slice_schedule(SLOT, Duration::from_millis(50), INTERVAL, SLOT, limits);
        assert_eq!(s.da_per_batch, Some(100_000));

        let mut pacer = SlicePacer::new(&s);
        pacer.tick();

        assert!(!spend(&mut pacer, 0, 200_001), "DA has its own ceiling, independent of gas");
        assert!(spend(&mut pacer, 0, 200_000), "the opening batch plus one tick");
    }

    /// DA bytes are known before execution, so settling refunds gas only —
    /// the DA charge stands.
    #[test]
    fn settling_does_not_refund_da_bytes() {
        let limits = SliceLimits {
            block_gas_limit: BLOCK_GAS,
            block_da_limit: Some(1_000_000),
            da_footprint_gas_scalar: None,
        };
        let mut pacer = SlicePacer::new(&derive_slice_schedule(
            SLOT,
            Duration::from_millis(50),
            INTERVAL,
            SLOT,
            limits,
        ));
        let reservation = pacer.reserve(1_000_000, 100_000).expect("fits");

        pacer.settle(reservation, 0);

        assert!(!spend(&mut pacer, 0, 1), "the DA it wrote is still written");
        assert!(spend(&mut pacer, 3_000_000, 0), "but the gas came back");
    }

    #[test]
    fn the_da_footprint_is_paced_in_gas_units() {
        let limits = SliceLimits {
            block_gas_limit: BLOCK_GAS,
            block_da_limit: None,
            da_footprint_gas_scalar: Some(100),
        };
        let s = derive_slice_schedule(SLOT, Duration::from_millis(50), INTERVAL, SLOT, limits);
        assert_eq!(s.da_footprint_gas_per_batch, Some(3_000_000));

        let mut pacer = SlicePacer::new(&s);

        assert!(!spend(&mut pacer, 0, 30_001));
        assert!(spend(&mut pacer, 0, 30_000), "30_000 bytes x 100 = 3M footprint gas");
    }

    #[test]
    fn an_absent_da_limit_leaves_that_dimension_unpaced() {
        let mut pacer = pacer();
        pacer.tick();

        assert!(spend(&mut pacer, 0, u64::MAX), "no DA limit configured means no DA ceiling");
    }

    #[test]
    fn total_admission_never_exceeds_the_block_gas_limit() {
        for deadline_ms in [0, 1, 75, 199, 200, 875, 950, 1925, 2000, 2400, 10_000] {
            for leeway_ms in [0, 50, 75, 199, 500] {
                let s =
                    schedule(Duration::from_millis(deadline_ms), Duration::from_millis(leeway_ms));
                let mut pacer = SlicePacer::new(&s);
                let mut spent = 0u64;

                for _ in 0..s.tick_count {
                    pacer.tick();
                    let mut step = BLOCK_GAS;
                    while step > 0 {
                        if spend(&mut pacer, step, 0) {
                            spent += step;
                        } else {
                            step /= 2;
                        }
                    }
                }

                assert!(
                    spent <= BLOCK_GAS,
                    "deadline={deadline_ms}ms leeway={leeway_ms}ms spent {spent}"
                );
            }
        }
    }
}

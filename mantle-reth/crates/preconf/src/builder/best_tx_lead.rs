//! Holding a pool snapshot back by whole slice boundaries.
//!
//! Preconf transactions never pass through this snapshot — they are dispatched
//! off the fifo as they arrive — so delaying it widens the gap between the two
//! channels without touching preconf latency.
//!
//! The build-start snapshot is exempt: it carries transactions that predate the
//! block and have had their head start already.

use std::collections::VecDeque;

/// A pool snapshot pipeline that releases each refresh `lead` boundaries late.
#[derive(Debug)]
pub(super) struct BestTxLead<I> {
    /// Boundaries a refreshed snapshot waits before it is served.
    lead: usize,
    /// Refreshed snapshots still waiting, oldest first.
    waiting: VecDeque<I>,
    /// The snapshot the pool arm reads from, if any.
    active: Option<I>,
}

impl<I> BestTxLead<I> {
    /// `initial` is the build-start snapshot, served immediately; `None` on a
    /// derivation build, where the pool arm is off.
    pub(super) fn new(lead: u64, initial: Option<I>) -> Self {
        Self { lead: lead as usize, waiting: VecDeque::new(), active: initial }
    }

    /// The snapshot the pool arm should read from.
    pub(super) const fn active_mut(&mut self) -> Option<&mut I> {
        self.active.as_mut()
    }

    /// Whether the pool arm has anything to read. Separate from
    /// [`Self::active_mut`] so the `select!` guard takes no mutable borrow.
    pub(super) const fn is_serving(&self) -> bool {
        self.active.is_some()
    }

    /// Takes a boundary snapshot. It serves once `lead` further boundaries have
    /// passed; until then the previous one keeps serving.
    pub(super) fn refresh(&mut self, snapshot: I) {
        self.waiting.push_back(snapshot);
        if self.waiting.len() > self.lead {
            self.active = self.waiting.pop_front();
        }
    }

    /// Drops the active snapshot; the ones waiting out their lead are untouched.
    pub(super) fn drain_active(&mut self) {
        self.active = None;
    }
}

#[cfg(test)]
mod tests {
    use super::BestTxLead;

    #[test]
    fn lead_zero_serves_a_refreshed_snapshot_immediately() {
        let mut lead = BestTxLead::new(0, Some(0u32));

        lead.refresh(1);

        assert_eq!(lead.active_mut().copied(), Some(1), "no lead means no wait");
    }

    #[test]
    fn lead_one_holds_a_refreshed_snapshot_for_one_boundary() {
        let mut lead = BestTxLead::new(1, Some(0u32));

        lead.refresh(1);
        assert_eq!(
            lead.active_mut().copied(),
            Some(0),
            "the first refresh is still waiting out its boundary"
        );

        lead.refresh(2);
        assert_eq!(lead.active_mut().copied(), Some(1), "it matures at the next boundary");
    }

    #[test]
    fn the_served_snapshot_trails_the_newest_by_the_lead() {
        let mut lead = BestTxLead::new(2, Some(0u32));

        for snapshot in 1..=5 {
            lead.refresh(snapshot);
        }

        assert_eq!(lead.active_mut().copied(), Some(3));
    }

    /// A derivation build starts with no snapshot at all.
    #[test]
    fn an_absent_initial_snapshot_stays_absent_until_one_matures() {
        let mut lead = BestTxLead::new(1, None::<u32>);

        lead.refresh(1);
        assert_eq!(lead.active_mut().copied(), None);

        lead.refresh(2);
        assert_eq!(lead.active_mut().copied(), Some(1));
    }

    /// Draining must stop the arm, or it spins on an iterator that will never
    /// yield again.
    #[test]
    fn a_drained_snapshot_stops_serving_until_the_next_matures() {
        let mut lead = BestTxLead::new(1, Some(0u32));
        lead.refresh(1);

        lead.drain_active();
        assert_eq!(lead.active_mut().copied(), None, "the drained one is gone");

        lead.refresh(2);
        assert_eq!(lead.active_mut().copied(), Some(1), "the one that was waiting still arrives");
    }

    #[test]
    fn is_serving_reports_whether_the_pool_arm_has_anything_to_read() {
        let mut lead = BestTxLead::new(1, None::<u32>);
        assert!(!lead.is_serving(), "a derivation build starts with nothing");

        lead.refresh(1);
        assert!(!lead.is_serving(), "a snapshot waiting out its lead is not readable yet");

        lead.refresh(2);
        assert!(lead.is_serving());

        lead.drain_active();
        assert!(!lead.is_serving());
    }
}

//! What the build loop has executed so far, and how much of it has been
//! accounted for.

use std::{
    collections::{HashMap, HashSet},
    ops::{Deref, DerefMut},
};

use alloy_consensus::transaction::Recovered;
use alloy_primitives::{Address, TxHash};
use reth_optimism_payload_builder::{
    block_size::BlockSizeBudget as OpBlockSizeBudget, builder::ExecutionInfo as OpExecutionInfo,
};
use reth_optimism_primitives::OpTransaction;
use reth_primitives_traits::SignedTransaction;

use crate::{ClaimKind, journal::JournalEntry, preconf_tx_set::PreconfTxSet};

/// Whether this transaction's nonce field is its sender's account nonce.
///
/// Deposits and the post-execution transaction are not. Both advance the
/// sender's account nonce when they execute, but neither carries the
/// resulting value: `TxDeposit::nonce()` returns a hard `0`, and the
/// post-execution transaction is synthesised with `Address::ZERO` as its
/// signer. They are counted rather than read — recording the `0` they report
/// would put a sender sitting at nonce 50 down as next-at-1.
pub(crate) fn carries_its_senders_nonce<T: OpTransaction>(tx: &T) -> bool {
    !tx.is_deposit() && tx.as_post_exec().is_none()
}

/// Record a transaction the block executor has committed: into the block's
/// running tally, and into the account view the admission path reads.
///
/// Both together, because an account view that lags the block by one
/// transaction is a nonce gap refused to a transaction that was in order.
/// Every execution goes through here for that reason; nothing in the type
/// system enforces it.
pub(crate) fn record_executed<T: SignedTransaction + OpTransaction>(
    info: &mut ExecutionInfo<T>,
    fifo: &PreconfTxSet,
    tx: Recovered<T>,
    journalable: bool,
) {
    let signer = tx.signer();
    if carries_its_senders_nonce(tx.inner()) {
        fifo.observe_executed(signer, tx.nonce());
    } else {
        fifo.observe_nonceless(signer);
    }

    if journalable {
        info.record_journalable(tx);
    } else {
        info.record(tx);
    }
}

/// The upstream execution counters, plus what slicing a block needs on top.
///
/// Transactions are kept whole rather than as encodings: assembling a slice
/// header reuses the same block assembler the final seal does, and that roots
/// consensus transactions. The wire encoding is taken from them on demand.
///
/// Receipts are not kept here at all. The block executor already accumulates
/// them and hands them out through `BlockExecutor::receipts`, so a second copy
/// would only be a pair that can fall out of step.
#[derive(Debug)]
pub struct ExecutionInfo<T> {
    /// The upstream counters. The one place block totals live — do not shadow
    /// gas or DA with a second tally here.
    pub inner: OpExecutionInfo,

    executed: Vec<Recovered<T>>,
    executed_sender_nonces: HashMap<Address, u64>,
    published_upto: usize,
    pruned_upto: usize,
    /// Positions in `executed` of the transactions that need a journal record,
    /// in execution order. Positions rather than copies: the transaction is
    /// already held once, and a second copy is a pair that can fall out of step.
    journalable: Vec<usize>,
    /// How far into `journalable` the journal has been handed records.
    journaled_upto: usize,
    /// Distinct pool hashes skipped for RLP headroom in this build, across snapshots.
    block_rlp_full_skipped: HashSet<TxHash>,
}

impl<T> Default for ExecutionInfo<T> {
    fn default() -> Self {
        Self {
            inner: OpExecutionInfo::default(),
            executed: Vec::new(),
            executed_sender_nonces: HashMap::new(),
            published_upto: 0,
            pruned_upto: 0,
            journalable: Vec::new(),
            journaled_upto: 0,
            block_rlp_full_skipped: HashSet::new(),
        }
    }
}

impl<T> ExecutionInfo<T> {
    /// Build with a constructed block-size budget.
    ///
    /// `Default` leaves the budget disabled, which is correct for a pre-Osaka
    /// or non-Mantle chain but silently removes the size bound everywhere
    /// else. Callers on the build path must go through here.
    pub fn with_block_size(block_size: OpBlockSizeBudget) -> Self {
        Self { inner: OpExecutionInfo { block_size, ..OpExecutionInfo::new() }, ..Self::default() }
    }
}

impl<T> ExecutionInfo<T> {
    /// Check the pool candidate against this block's RLP budget and record skips.
    ///
    /// A too-large transaction cannot fit any block. A full block can clear
    /// next slot; count each skipped hash once here even when slice snapshots
    /// repeatedly offer it. The admission verdict itself is never cached.
    pub(crate) fn pool_rlp_admission(
        &mut self,
        hash: TxHash,
        tx_rlp_length: usize,
        sdm_refund_possible: bool,
    ) -> reth_optimism_payload_builder::block_size::RawSizeAdmission {
        use reth_optimism_payload_builder::block_size::RawSizeAdmission;
        let admission = self.block_size.admission(tx_rlp_length, sdm_refund_possible);
        match admission {
            RawSizeAdmission::TooLarge => {
                metrics::counter!("preconf.build.tx_exceeds_block_rlp_total").increment(1);
            }
            RawSizeAdmission::BlockFull if self.block_rlp_full_skipped.insert(hash) => {
                metrics::counter!("preconf.build.block_rlp_full_skipped_total").increment(1);
            }
            RawSizeAdmission::BlockFull | RawSizeAdmission::Fits => {}
        }
        admission
    }
}

impl<T> Deref for ExecutionInfo<T> {
    type Target = OpExecutionInfo;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for ExecutionInfo<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T: SignedTransaction> ExecutionInfo<T> {
    /// Record a transaction the block executor has committed.
    pub fn record(&mut self, tx: Recovered<T>) {
        let nonce = tx.nonce();
        self.executed_sender_nonces
            .entry(tx.signer())
            .and_modify(|highest| *highest = (*highest).max(nonce))
            .or_insert(nonce);
        self.executed.push(tx);
    }

    /// Record a transaction that also has to survive a restart.
    ///
    /// Only pool transactions do. A deposit arrives with the attributes on
    /// every rebuild, a preconf commitment is journaled when its receipt goes
    /// out, and the post-execution transaction is produced by the executor
    /// rather than sent by anyone — putting any of them back in the pool on
    /// restart would be wrong.
    pub fn record_journalable(&mut self, tx: Recovered<T>) {
        self.journalable.push(self.executed.len());
        self.record(tx);
    }

    /// Everything executed this block, in execution order.
    pub fn executed(&self) -> &[Recovered<T>] {
        &self.executed
    }

    /// Everything `record_journalable` marked since the last call, as
    /// [`JournalEntry`] records — and moves past them in the same step.
    ///
    /// One operation rather than a read and an advance, because there is no
    /// correct way to do one without the other: records left behind are written
    /// twice, records skipped are never written at all. Repeating a handover is
    /// the journal's own business — it keeps what a write could not place and
    /// carries it forward — so a record leaves here exactly once, whether or not
    /// the disk took it.
    ///
    /// Separate from the publish cursor because the two diverge: a slice dropped
    /// by the cancel guard was handed over all the same.
    ///
    /// Only the slice path calls this, so with slicing off nothing drains what
    /// `record_journalable` marked — see the pool arm in
    /// `builder::payload_builder` for what that costs and why it stands.
    pub fn take_journal_records(&mut self, block_height: u64) -> Vec<JournalEntry> {
        let taken = &self.journalable[self.journaled_upto..];
        let records: Vec<JournalEntry> = taken
            .iter()
            .map(|&at| {
                let tx = &self.executed[at];
                JournalEntry::for_executed(*tx.tx_hash(), tx, block_height, ClaimKind::Announced)
            })
            .collect();
        self.journaled_upto = self.journalable.len();
        records
    }

    /// The highest nonce executed this block per sender.
    pub const fn executed_sender_nonces(&self) -> &HashMap<Address, u64> {
        &self.executed_sender_nonces
    }

    /// What has been executed since the last slice was published.
    pub fn pending_slice(&self) -> &[Recovered<T>] {
        &self.executed[self.published_upto..]
    }

    /// Mark everything executed so far as published.
    pub fn advance_publish_cursor(&mut self) {
        self.published_upto = self.executed.len();
    }

    /// How much of the block has already been published.
    pub const fn published_upto(&self) -> usize {
        self.published_upto
    }

    /// What has been executed but not yet accounted for to the pool.
    ///
    /// Tracked separately from publishing: a slice can be assembled and then
    /// dropped, but the transactions in it were executed either way and the
    /// pool still has to be told.
    pub fn pending_prune(&self) -> &[Recovered<T>] {
        &self.executed[self.pruned_upto..]
    }

    /// Mark everything executed so far as accounted for to the pool.
    pub fn advance_prune_cursor(&mut self) {
        self.pruned_upto = self.executed.len();
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Signed, Transaction as _, TxLegacy, transaction::Recovered};
    use alloy_primitives::{Address, B256, Sealable, Signature, TxKind, U256};
    use op_alloy_consensus::{OpTxEnvelope, TxDeposit};

    use super::{ExecutionInfo, OpBlockSizeBudget, OpExecutionInfo};

    type Info = ExecutionInfo<OpTxEnvelope>;
    #[test]
    fn rlp_full_skips_count_each_hash_once_per_build() {
        use alloy_primitives::B256;
        use metrics_exporter_prometheus::PrometheusBuilder;
        use reth_optimism_payload_builder::block_size::{
            BLOCK_SIZE_PACKING_TARGET, RawSizeAdmission,
        };
        let recorder = PrometheusBuilder::new().build_recorder();
        let metrics = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let mut info = Info::with_block_size(OpBlockSizeBudget::new(true, true, 42, false));
            let a = B256::repeat_byte(1);
            let b = B256::repeat_byte(2);
            assert_eq!(info.pool_rlp_admission(a, 100, false), RawSizeAdmission::Fits);
            info.block_size.record_transaction(BLOCK_SIZE_PACKING_TARGET, false);
            for _slice in 0..4 {
                assert_eq!(info.pool_rlp_admission(a, 100, false), RawSizeAdmission::BlockFull);
                assert_eq!(info.pool_rlp_admission(b, 100, false), RawSizeAdmission::BlockFull);
            }
            assert!(
                metrics
                    .render()
                    .lines()
                    .any(|l| l == "preconf_build_block_rlp_full_skipped_total 2"),
                "repeated snapshots must count two distinct hashes"
            );
            let mut next = Info::with_block_size(OpBlockSizeBudget::new(true, true, 43, false));
            assert_eq!(next.pool_rlp_admission(a, 100, false), RawSizeAdmission::Fits);
            next.block_size.record_transaction(BLOCK_SIZE_PACKING_TARGET, false);
            assert_eq!(next.pool_rlp_admission(a, 100, false), RawSizeAdmission::BlockFull);
            assert!(
                metrics
                    .render()
                    .lines()
                    .any(|l| l == "preconf_build_block_rlp_full_skipped_total 3"),
                "a new build must count the hash again"
            );
        });
    }

    fn sender(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    /// A legacy transaction is enough: nothing here reads more than the nonce
    /// and the recovered signer.
    fn tx(sender: Address, nonce: u64) -> Recovered<OpTxEnvelope> {
        let inner = TxLegacy {
            chain_id: Some(1),
            nonce,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Default::default(),
        };
        let signature = Signature::new(U256::from(1), U256::from(1), false);
        let envelope = OpTxEnvelope::Legacy(Signed::new_unhashed(inner, signature));
        Recovered::new_unchecked(envelope, sender)
    }

    /// `Default` yields a disabled budget, so a build path that constructs its
    /// `ExecutionInfo` that way has no size bound at all while still compiling
    /// and passing its tests. `with_block_size` is the way past that, and it
    /// has to carry the budget it was handed rather than deciding on one
    /// itself — so both directions are pinned. Dropping the argument fails the
    /// enabled case; substituting a budget of its own making fails whichever
    /// of the two it did not pick.
    #[test]
    fn with_block_size_carries_the_budget_it_was_handed() {
        let enabled = OpBlockSizeBudget::new(true, true, 42, false);
        let disabled = OpBlockSizeBudget::new(false, true, 42, false);
        let (enabled_length, disabled_length) =
            (enabled.estimated_block_rlp_length(), disabled.estimated_block_rlp_length());

        let info = Info::with_block_size(enabled);

        assert_eq!(info.block_size.estimated_block_rlp_length(), enabled_length);
        assert_eq!(
            Info::with_block_size(disabled).block_size.estimated_block_rlp_length(),
            disabled_length,
            "a disabled budget is carried through just as faithfully",
        );
        assert_eq!(info.cumulative_gas_used, 0, "and nothing else comes along with it");
        assert!(info.executed().is_empty());
    }

    /// What the build's Stage 4 does to the budget when it appends an SDM
    /// post-exec refund transaction: the per-transaction reserve the earlier
    /// arms set aside is dropped and the real item takes its place, leaving the
    /// budget holding exactly the block's transactions.
    ///
    /// Covered here rather than against a real build because the Mantle
    /// builder installs `alloy_op_evm::post_exec::NullRefundPolicy`, which
    /// refunds nothing: Stage 4 accumulates no entries and appends no
    /// transaction, so the shipped node never reaches the swap. A policy that
    /// does refund exists in the tree — `FixedRefundPolicy` in
    /// `op-reth/crates/sdm-fixture-node` — but that crate is test-only, kept
    /// out of production images, and swaps the stock payload service rather
    /// than this builder.
    ///
    /// The integration suite pins that premise
    /// (`sdm_production_appends_no_post_exec_transaction_under_the_shipped_refund_policy`);
    /// this pins what the swap must do once a refund policy makes it reachable.
    #[test]
    fn the_post_exec_swap_leaves_the_budget_holding_the_real_block() {
        use alloy_rlp::Encodable as _;
        use op_alloy_consensus::{SDMGasEntry, build_post_exec_tx};

        // The real item, so the comparison against the reserve is the one the
        // build makes rather than a stand-in for it.
        let post_exec: OpTxEnvelope =
            build_post_exec_tx(42, vec![SDMGasEntry { index: 1, gas_refund: 1 }])
                .seal_slow()
                .into();
        let post_exec_item = post_exec.length();

        let mut info = Info::with_block_size(OpBlockSizeBudget::new(true, true, 42, true));
        info.block_size.record_transaction(200, true);
        info.block_size.record_transaction(100, true);
        let reserved = info.block_size.estimated_block_rlp_length();

        info.block_size.record_post_exec(post_exec_item);

        // The same three items with no reserve anywhere — which is what the
        // sealed block is. Skipping the swap leaves the reserve in place and
        // the post-exec bytes out, and lands on neither side of this.
        let mut sealed = OpBlockSizeBudget::new(true, true, 42, false);
        for item in [200, 100, post_exec_item] {
            sealed.record_transaction(item, false);
        }
        assert_eq!(
            info.block_size.estimated_block_rlp_length(),
            sealed.estimated_block_rlp_length()
        );
        assert!(
            info.block_size.estimated_block_rlp_length() < reserved,
            "a maximum-width reserve is wider than the refund that replaces it"
        );
    }

    #[test]
    fn transactions_are_recorded_in_execution_order() {
        let mut info = Info::default();

        info.record(tx(sender(0xaa), 5));
        info.record(tx(sender(0xbb), 0));

        assert_eq!(info.executed().len(), 2);
        assert_eq!(info.executed()[1].signer(), sender(0xbb));
    }

    #[test]
    fn the_pending_slice_holds_only_what_followed_the_cursor() {
        let mut info = Info::default();
        for nonce in 0..3 {
            info.record(tx(sender(0xaa), nonce));
        }
        info.advance_publish_cursor();
        info.record(tx(sender(0xaa), 3));
        info.record(tx(sender(0xaa), 4));

        let pending = info.pending_slice();

        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].nonce(), 3);
    }

    /// Publishing a slice twice must not republish its transactions; the
    /// cursor is what prevents it.
    #[test]
    fn advancing_the_cursor_without_new_work_yields_an_empty_slice() {
        let mut info = Info::default();
        info.record(tx(sender(0xaa), 0));
        info.advance_publish_cursor();

        info.advance_publish_cursor();

        assert!(info.pending_slice().is_empty());
    }

    /// Publishing and pool accounting advance independently: a slice that is
    /// assembled and then dropped still executed its transactions.
    #[test]
    fn the_publish_and_prune_cursors_move_independently() {
        let mut info = Info::default();
        info.record(tx(sender(0xaa), 0));

        info.advance_publish_cursor();

        assert!(info.pending_slice().is_empty(), "published");
        assert_eq!(info.pending_prune().len(), 1, "but the pool has not been told yet");

        info.advance_prune_cursor();
        assert!(info.pending_prune().is_empty());
    }

    #[test]
    fn a_fresh_info_has_nothing_pending() {
        let info = Info::default();

        assert!(info.pending_slice().is_empty());
        assert_eq!(info.published_upto(), 0);
    }

    /// Only pool transactions need a journal record. Deposits arrive with the
    /// attributes on every rebuild, preconf commitments are journaled when
    /// their receipt goes out, and the post-execution transaction is not a
    /// user transaction at all — journaling any of them would put something
    /// back in the pool that does not belong there.
    #[test]
    fn only_transactions_recorded_as_journalable_are_queued_for_the_journal() {
        let mut info = Info::default();

        info.record(tx(sender(0xaa), 0));
        info.record_journalable(tx(sender(0xbb), 1));
        info.record(tx(sender(0xcc), 2));

        let records = info.take_journal_records(7);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].hash, *info.executed()[1].tx_hash());
        assert_eq!(records[0].block_height, 7);
        assert_eq!(info.executed().len(), 3, "all three still executed, in order");
    }

    /// A record comes out for each transaction `record_journalable` marked,
    /// in execution order, and for no other.
    #[test]
    fn the_records_are_the_journalable_transactions_in_execution_order() {
        let mut info = Info::default();
        info.record(tx(sender(0xaa), 0));
        info.record_journalable(tx(sender(0xbb), 1));
        info.record_journalable(tx(sender(0xcc), 2));

        let records = info.take_journal_records(7);

        // Only what `record_journalable` marked, in execution order.
        assert_eq!(
            records.iter().map(|r| r.hash).collect::<Vec<_>>(),
            vec![*tx(sender(0xbb), 1).tx_hash(), *tx(sender(0xcc), 2).tx_hash()],
        );
    }

    /// The journal cursor tracks what has been handed over, the publish cursor
    /// what reached subscribers. A slice dropped by the cancel guard was handed
    /// to the journal all the same, so its transactions must not be offered
    /// again when the next slice carries them.
    #[test]
    fn a_dropped_slice_does_not_journal_its_transactions_twice() {
        let mut info = Info::default();
        info.record_journalable(tx(sender(0xaa), 0));

        // The slice was journaled, then dropped before it went out.
        assert_eq!(info.take_journal_records(7).len(), 1);

        assert!(info.take_journal_records(7).is_empty(), "already handed over");
        assert_eq!(info.pending_slice().len(), 1, "but never published");
    }

    #[test]
    fn the_journal_cursor_keeps_the_order_transactions_executed_in() {
        let mut info = Info::default();
        info.record_journalable(tx(sender(0xaa), 0));
        info.record(tx(sender(0xbb), 0));
        info.take_journal_records(7);
        info.record_journalable(tx(sender(0xaa), 1));
        info.record_journalable(tx(sender(0xaa), 2));

        let taken: Vec<_> = info.take_journal_records(7).iter().map(|r| r.hash).collect();
        let expected: Vec<_> = info.executed()[2..].iter().map(|tx| *tx.tx_hash()).collect();

        assert_eq!(taken, expected, "in execution order, and only the new ones");
    }

    /// The pool is told a sender's nonce has moved on. Only the highest one
    /// executed this block is meaningful, and transactions do not have to
    /// arrive in nonce order.
    #[test]
    fn a_sender_keeps_the_highest_nonce_executed_this_block() {
        let mut info = Info::default();
        let alice = sender(0xaa);

        info.record(tx(alice, 5));
        info.record(tx(alice, 7));
        info.record(tx(alice, 6));

        assert_eq!(info.executed_sender_nonces().get(&alice), Some(&7));
    }

    #[test]
    fn senders_are_tracked_independently() {
        let mut info = Info::default();

        info.record(tx(sender(0xaa), 5));
        info.record(tx(sender(0xbb), 2));

        assert_eq!(info.executed_sender_nonces().get(&sender(0xaa)), Some(&5));
        assert_eq!(info.executed_sender_nonces().get(&sender(0xbb)), Some(&2));
    }

    /// The ordinary case: the nonce field means what it says.
    #[test]
    fn an_ordinary_transaction_carries_its_senders_nonce() {
        assert!(super::carries_its_senders_nonce(tx(sender(0xaa), 5).inner()));
    }

    /// A deposit reports `0` however far along its sender actually is, so the
    /// account view must count it rather than read it. Were this to return
    /// `true`, a sender at nonce 50 receiving a deposit would be recorded as
    /// next-at-1 and every transaction it sent would read as a nonce gap.
    #[test]
    fn a_deposit_does_not_carry_its_senders_nonce() {
        let deposit = OpTxEnvelope::Deposit(
            TxDeposit {
                source_hash: B256::repeat_byte(0x11),
                from: sender(0xaa),
                to: TxKind::Call(Address::ZERO),
                mint: 0,
                value: U256::ZERO,
                gas_limit: 21_000,
                is_system_transaction: false,
                input: Default::default(),
                eth_value: U256::ZERO,
                eth_tx_value: None,
            }
            .seal_slow(),
        );

        assert_eq!(deposit.nonce(), 0, "the premise: it reports zero, not the real value");
        assert!(!super::carries_its_senders_nonce(&deposit));
    }

    /// The upstream counters stay the one place block totals live, reachable
    /// without a second name for them.
    #[test]
    fn the_upstream_counters_are_reachable_through_the_wrapper() {
        let mut info = Info {
            inner: OpExecutionInfo {
                cumulative_gas_used: 21_000,
                cumulative_da_bytes_used: 128,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(info.cumulative_gas_used, 21_000, "readable without naming `inner`");
        info.cumulative_gas_used += 21_000;
        assert_eq!(info.inner.cumulative_gas_used, 42_000, "and writes land on the one tally");
    }
}

#[cfg(test)]
mod slice_state_invariants {
    //! Reading the bundle mid-block, without disturbing the block being built.
    //!
    //! Assembling a slice header needs the bundle, and the bundle is only
    //! populated by draining the transition state into it. Doing that midway
    //! through a block has to leave no trace, or the state diff that eventually
    //! gets persisted is not the one the block actually produced.

    use alloy_primitives::{Address, U256};
    use reth_revm::{
        DatabaseCommit, State,
        db::{EmptyDB, states::bundle_state::BundleRetention},
        state::{Account, AccountInfo, AccountStatus},
    };

    fn state_with_one_touched_account(address: Address) -> State<EmptyDB> {
        let mut state =
            State::builder().with_database(EmptyDB::default()).with_bundle_update().build();
        state.load_cache_account(address).expect("an empty database yields an empty account");

        // revm 41 made `Account::original_info` private, so the struct can no
        // longer be built field-by-field here.
        let mut account = Account::default();
        account.info = AccountInfo { balance: U256::from(100), nonce: 1, ..Default::default() };
        account.status = AccountStatus::Touched;
        state.commit([(address, account)].into_iter().collect());

        state
    }

    /// Restoring the transition state is not enough on its own: merging is
    /// what appends a revert block, and a second merge appends another. Left
    /// alone, a block sliced ten times would persist ten revert blocks where
    /// it produced one.
    #[test]
    fn merging_twice_appends_a_second_revert_block() {
        let address = Address::repeat_byte(0xaa);
        let mut state = state_with_one_touched_account(address);

        let transitions = state.transition_state.clone();
        state.merge_transitions(BundleRetention::Reverts);
        assert_eq!(state.bundle_state.reverts.len(), 1);

        state.transition_state = transitions;
        state.merge_transitions(BundleRetention::Reverts);

        assert_eq!(state.bundle_state.reverts.len(), 2, "the extra block has to be undone");
    }

    /// And putting the revert blocks back does not undo it either: the first
    /// merge is what captured the revert, so re-merging the restored
    /// transitions against a bundle that already holds them yields an *empty*
    /// block. Truncate that away and the real revert is gone with it.
    ///
    /// Which is why slicing reads no bundle at all. The only thing the slice
    /// header wants from it is one predeploy's storage, and that is readable
    /// from the live cache without merging anything.
    #[test]
    fn putting_the_revert_blocks_back_does_not_undo_the_merge() {
        let address = Address::repeat_byte(0xaa);
        let mut sliced = state_with_one_touched_account(address);
        let mut once = state_with_one_touched_account(address);
        once.merge_transitions(BundleRetention::Reverts);

        let transitions = sliced.transition_state.clone();
        let reverts_before = sliced.bundle_state.reverts.len();
        sliced.merge_transitions(BundleRetention::Reverts);
        sliced.transition_state = transitions;
        sliced.bundle_state.reverts.truncate(reverts_before);
        sliced.merge_transitions(BundleRetention::Reverts);

        assert_eq!(sliced.bundle_state.state, once.bundle_state.state, "accounts do survive");
        assert_ne!(
            sliced.bundle_state.reverts, once.bundle_state.reverts,
            "but the revert the block needs to be unwound by is lost",
        );
    }
}

//! Telling the transaction pool what a slice just executed.
//!
//! Sealing a block is what normally tells the pool its transactions are gone.
//! Slicing publishes long before that, so between slices the pool still holds
//! everything already executed and still believes those senders sit at their
//! pre-block nonce. Left alone it hands the next slice transactions that have
//! already run, and parks the ones that should follow them.

use alloy_primitives::{Address, TxHash, U256};
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::SignedTransaction;
use reth_transaction_pool::TransactionPoolExt;
use tracing::warn;

use crate::builder::ExecutionInfo;

/// Sender balances, as the pool would otherwise read them from state.
///
/// Narrow on purpose: the only thing pool maintenance needs from state is a
/// balance, and keeping it to that makes the ordering rules below testable
/// against a real pool without standing up a provider.
pub trait SenderBalances {
    /// Balance of `address`, or `None` when it cannot be read.
    fn balance_of(&self, address: Address) -> Option<U256>;
}

/// What the pool was told at one slice boundary.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PoolMaintenance {
    /// Executed transactions the pool was told about.
    pub pruned: usize,
    /// Senders whose nonce was corrected.
    pub accounts_advanced: usize,
    /// Senders advanced without a readable balance, and so parked.
    pub balances_unavailable: usize,
}

/// Bring the pool up to date with what has been executed, then leave it ready
/// for a fresh best-transaction iterator.
///
/// Two steps, and the order is not interchangeable:
///
/// 1. Remove what ran, temporarily parking its descendants. This prevents a higher nonce from
///    remaining pending while a stale lower nonce is removed later: the pool's candidate index only
///    advances to the immediate successor when removing its head.
/// 2. Correct the sender nonces and balances, promoting their eligible successors in nonce order.
///    Cover every sender executed this block rather than only this slice. Removal drops the
///    sender's record once nothing of theirs remains, so account updates must follow removal.
///
/// Removal emits discarded notifications for the executed transactions. Unlike pruning them as
/// mined, it also parks descendants before deletion; the account update restores eligibility
/// before the caller creates its next iterator.
///
/// Step 2 runs even when the slice executed nothing. Admitting a transaction
/// overwrites its sender's record with the nonce the validator saw, which
/// mid-block is the stale pre-block one — so anything that arrived since the
/// last boundary is sitting parked, and re-asserting is what frees it. Skipping
/// the step on an empty slice would strand exactly those transactions.
///
/// The caller must take its new iterator straight after this returns: the
/// correction only holds until the next arrival overwrites it.
pub fn maintain_pool_at_slice_boundary<Pool, Balances, T>(
    pool: &Pool,
    balances: &Balances,
    info: &mut ExecutionInfo<T>,
) -> PoolMaintenance
where
    Pool: TransactionPoolExt,
    Balances: SenderBalances,
    T: SignedTransaction,
{
    // Both pool calls take the pool's write lock, and `update_accounts` walks
    // every transaction in the pool, so this grows with the pool as well as
    // with the senders below.
    let started = std::time::Instant::now();
    let executed: Vec<TxHash> = info.pending_prune().iter().map(|tx| *tx.tx_hash()).collect();
    let pruned = executed.len();
    if pruned > 0 {
        pool.remove_transactions(executed);
        info.advance_prune_cursor();
    }

    let mut balances_unavailable = 0;
    let changed: Vec<ChangedAccount> = info
        .executed_sender_nonces()
        .iter()
        .map(|(&address, &highest_nonce)| {
            let balance = balances.balance_of(address).unwrap_or_else(|| {
                balances_unavailable += 1;
                warn!(
                    target: "mantle::preconf::flashblocks",
                    %address,
                    "sender balance unreadable; parking them until the block seals",
                );
                // Guessing upwards would let an unfunded transaction into the
                // block. Parking the sender costs at most a slice.
                U256::ZERO
            });
            // The pool tracks the next nonce it expects, which is one past the
            // highest executed.
            ChangedAccount { address, nonce: highest_nonce.saturating_add(1), balance }
        })
        .collect();

    let accounts_advanced = changed.len();
    if accounts_advanced > 0 {
        pool.update_accounts(changed);
    }

    metrics::histogram!("flashblocks.pool_maintenance_duration_ms")
        .record(started.elapsed().as_secs_f64() * 1000.0);
    // Every sender executed so far in the block, not just this slice, so it
    // grows towards the end of a block.
    metrics::histogram!("flashblocks.pool_maintenance_accounts").record(accounts_advanced as f64);
    metrics::counter!("flashblocks.pool_maintenance_balance_unreadable_total")
        .increment(balances_unavailable as u64);

    PoolMaintenance { pruned, accounts_advanced, balances_unavailable }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_consensus::{Signed, TxLegacy, transaction::Recovered};
    use alloy_primitives::{Address, Signature, TxHash, TxKind, U256};
    use op_alloy_consensus::OpTxEnvelope;
    use reth_execution_types::ChangedAccount;
    use reth_transaction_pool::{
        PoolTransaction, TransactionOrigin, TransactionPool, TransactionPoolExt,
        test_utils::{MockTransaction, TestPool, testing_pool},
    };

    use super::{SenderBalances, maintain_pool_at_slice_boundary};
    use crate::builder::ExecutionInfo;

    type Info = ExecutionInfo<OpTxEnvelope>;

    /// Balances the pool would otherwise read from state.
    struct Balances(HashMap<Address, U256>);

    impl Balances {
        fn funded(senders: impl IntoIterator<Item = Address>) -> Self {
            Self(senders.into_iter().map(|sender| (sender, U256::from(u128::MAX))).collect())
        }

        /// Every lookup fails, standing in for a state read that errored.
        fn unreadable() -> Self {
            Self(HashMap::new())
        }
    }

    impl SenderBalances for Balances {
        fn balance_of(&self, address: Address) -> Option<U256> {
            self.0.get(&address).copied()
        }
    }

    /// Note the pool transaction as executed by the build loop.
    ///
    /// The mock pool transaction is not a consensus transaction, so this
    /// mirrors it into one carrying the same hash, sender and nonce — which
    /// is all pool maintenance reads.
    fn note_executed(info: &mut Info, tx: &MockTransaction) {
        let inner = TxLegacy {
            chain_id: Some(1),
            nonce: *tx.get_nonce(),
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Default::default(),
        };
        let signature = Signature::new(U256::from(1), U256::from(1), false);
        let envelope = OpTxEnvelope::Legacy(Signed::new_unchecked(inner, signature, *tx.hash()));
        info.record(Recovered::new_unchecked(envelope, tx.sender()));
    }

    async fn submit(pool: &TestPool, tx: MockTransaction) -> TxHash {
        pool.add_transaction(TransactionOrigin::Local, tx).await.expect("the pool accepts it").hash
    }

    fn tx(sender: Address, nonce: u64) -> MockTransaction {
        MockTransaction::legacy().with_sender(sender).with_nonce(nonce).with_gas_price(100)
    }

    fn is_pending(pool: &TestPool, hash: TxHash) -> bool {
        pool.pending_transactions().iter().any(|tx| *tx.hash() == hash)
    }

    fn is_queued(pool: &TestPool, hash: TxHash) -> bool {
        pool.queued_transactions().iter().any(|tx| *tx.hash() == hash)
    }

    /// A new build can execute a successor before the pool receives its
    /// parent's canonical notification. Slice maintenance must still retain
    /// the lowest unexecuted transaction as the sender's candidate.
    #[tokio::test]
    async fn a_delayed_parent_notification_does_not_strand_pending_successors() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let transactions: Vec<_> = (0..7).map(|nonce| tx(alice, nonce)).collect();
        let mut hashes = Vec::new();
        for transaction in &transactions[..6] {
            hashes.push(submit(&pool, transaction.clone()).await);
        }

        // Nonce 0 is already in the parent, but its pool notification is late.
        // The current build skips it as nonce-too-low and executes nonce 1.
        let mut info = Info::default();
        note_executed(&mut info, &transactions[1]);
        let outcome = maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);
        assert_eq!(outcome.pruned, 1);
        assert!(!is_pending(&pool, hashes[0]));
        assert!(!is_pending(&pool, hashes[1]));
        for hash in &hashes[2..] {
            assert!(is_pending(&pool, *hash));
        }

        let new_arrival = submit(&pool, transactions[6].clone()).await;
        assert!(is_queued(&pool, new_arrival));
        maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);
        assert!(is_pending(&pool, new_arrival));
        let selected = pool.best_transactions().next().expect("pending transactions exist");
        assert_eq!(
            *selected.transaction.get_nonce(),
            2,
            "nonces 2 through 5 remain pending and must precede the new arrival at nonce 6",
        );

        // Once nonce 1 lands, later account notifications and empty builds
        // must still choose nonce 2 without requiring another submission.
        for _ in 0..3 {
            pool.update_accounts(vec![ChangedAccount {
                address: alice,
                nonce: 2,
                balance: U256::from(u128::MAX),
            }]);
            maintain_pool_at_slice_boundary(
                &pool,
                &Balances::funded([alice]),
                &mut Info::default(),
            );
            let next = pool.best_transactions().next().expect("pending transactions exist");
            assert_eq!(*next.transaction.get_nonce(), 2);
        }
    }

    /// Removing executed transactions parks their successors, and the following
    /// account update must make those successors packable before returning.
    #[tokio::test]
    async fn maintenance_restores_the_successors_of_an_executed_transaction() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let first = tx(alice, 0);
        submit(&pool, first.clone()).await;
        let second = submit(&pool, tx(alice, 1)).await;
        let third = submit(&pool, tx(alice, 2)).await;

        let mut info = Info::default();
        note_executed(&mut info, &first);
        maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);

        assert!(is_pending(&pool, second), "the next nonce stays packable");
        assert!(is_pending(&pool, third));
    }

    /// Removal alone parks successors, so maintenance must follow it with an
    /// account update before taking the next best-transaction iterator.
    #[tokio::test]
    async fn removal_without_an_account_update_parks_successors() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let first = tx(alice, 0);
        submit(&pool, first.clone()).await;
        let second = submit(&pool, tx(alice, 1)).await;

        pool.remove_transactions(vec![*first.hash()]);

        assert!(is_queued(&pool, second), "removal is not the same as having been mined");
    }

    /// The whole point of maintaining the pool mid-block: a transaction that
    /// arrives after its predecessor executed is packable by the next slice
    /// rather than having to wait for the block to seal.
    ///
    /// It is parked on arrival — admitting it overwrites the sender's record
    /// with the stale pre-block nonce the validator saw — and freed by the
    /// following boundary.
    #[tokio::test]
    async fn a_successor_submitted_between_slices_is_freed_by_the_next_boundary() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let first = tx(alice, 0);
        submit(&pool, first.clone()).await;
        let mut info = Info::default();
        note_executed(&mut info, &first);
        maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);

        let second = submit(&pool, tx(alice, 1)).await;
        assert!(is_queued(&pool, second), "arrival stamps it with the stale nonce");

        maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);

        assert!(
            is_pending(&pool, second),
            "the next boundary frees it, so it makes this block rather than the next",
        );
    }

    /// A slice can execute nothing — an empty pool, or everything rejected —
    /// and the boundary still has to re-assert nonces, because transactions
    /// that arrived meanwhile are parked and only this frees them.
    #[tokio::test]
    async fn a_boundary_that_pruned_nothing_still_frees_new_arrivals() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let first = tx(alice, 0);
        submit(&pool, first.clone()).await;
        let mut info = Info::default();
        note_executed(&mut info, &first);
        maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);
        let second = submit(&pool, tx(alice, 1)).await;

        let outcome = maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);

        assert_eq!(outcome.pruned, 0, "nothing new executed");
        assert_eq!(outcome.accounts_advanced, 1, "but the nonce is re-asserted anyway");
        assert!(is_pending(&pool, second));
    }

    /// Removal clears the pool's record of a sender once nothing of theirs is
    /// left, so advancing the nonce first writes it somewhere that is about to
    /// be discarded.
    #[tokio::test]
    async fn advancing_the_nonce_before_removal_loses_it() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let first = tx(alice, 0);
        submit(&pool, first.clone()).await;

        // The wrong order, spelled out rather than reachable through the API.
        pool.update_accounts(vec![ChangedAccount {
            address: alice,
            nonce: 1,
            balance: U256::from(u128::MAX),
        }]);
        pool.remove_transactions(vec![*first.hash()]);

        let second = submit(&pool, tx(alice, 1)).await;

        assert!(is_queued(&pool, second), "the nonce written before removal did not survive it",);
    }

    /// A balance that cannot be read must not be guessed upwards: parking the
    /// sender until the next block costs a slice, letting an unfunded
    /// transaction through costs a failed block.
    #[tokio::test]
    async fn a_sender_whose_balance_is_unreadable_is_advanced_with_nothing() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let first = tx(alice, 0);
        submit(&pool, first.clone()).await;

        let mut info = Info::default();
        note_executed(&mut info, &first);
        let outcome = maintain_pool_at_slice_boundary(&pool, &Balances::unreadable(), &mut info);

        assert_eq!(outcome.balances_unavailable, 1);
        let second = submit(&pool, tx(alice, 1)).await;
        assert!(is_queued(&pool, second), "a sender with no known funds is parked");
    }

    #[tokio::test]
    async fn a_slice_that_executed_nothing_leaves_the_pool_alone() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let only = submit(&pool, tx(alice, 0)).await;
        let mut info = Info::default();

        let outcome = maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);

        assert_eq!(outcome.pruned, 0);
        assert_eq!(outcome.accounts_advanced, 0);
        assert!(is_pending(&pool, only));
    }

    /// Maintenance covers what the last slice executed, so running it twice
    /// must not remove the same executed transactions again.
    #[tokio::test]
    async fn maintenance_covers_each_transaction_once() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let first = tx(alice, 0);
        submit(&pool, first.clone()).await;
        let mut info = Info::default();
        note_executed(&mut info, &first);

        let first_pass =
            maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);
        let second_pass =
            maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);

        assert_eq!(first_pass.pruned, 1);
        assert_eq!(second_pass.pruned, 0, "the second slice executed nothing new");
    }

    /// Every boundary records how long it took and how many senders it
    /// re-asserted (one balance read each), and counts the senders it had to
    /// park because their balance could not be read.
    #[tokio::test]
    async fn a_boundary_records_its_duration_its_senders_and_unreadable_balances() {
        use metrics_exporter_prometheus::PrometheusBuilder;
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _local = metrics::set_default_local_recorder(&recorder);

        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let bob = Address::repeat_byte(0xbb);
        let alice_first = tx(alice, 0);
        let bob_first = tx(bob, 0);
        submit(&pool, alice_first.clone()).await;
        submit(&pool, bob_first.clone()).await;
        let mut info = Info::default();
        note_executed(&mut info, &alice_first);
        note_executed(&mut info, &bob_first);

        // Only alice's balance is readable.
        maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice]), &mut info);

        let rendered = handle.render();
        for line in [
            "flashblocks_pool_maintenance_duration_ms_count 1",
            "flashblocks_pool_maintenance_accounts_count 1",
            "flashblocks_pool_maintenance_accounts_sum 2",
            "flashblocks_pool_maintenance_balance_unreadable_total 1",
        ] {
            assert!(rendered.lines().any(|l| l == line), "expected `{line}`:\n{rendered}");
        }
    }

    /// Nonces accumulate across the block, so a sender executed in an earlier
    /// slice keeps its corrected nonce even when a later slice does not touch
    /// it.
    #[tokio::test]
    async fn a_sender_executed_in_an_earlier_slice_keeps_its_corrected_nonce() {
        let pool = testing_pool();
        let alice = Address::repeat_byte(0xaa);
        let bob = Address::repeat_byte(0xbb);
        let alice_first = tx(alice, 0);
        submit(&pool, alice_first.clone()).await;
        let bob_first = tx(bob, 0);
        submit(&pool, bob_first.clone()).await;

        let mut info = Info::default();
        note_executed(&mut info, &alice_first);
        maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice, bob]), &mut info);

        // Alice is untouched by the next slice, which executes only bob.
        let alice_next = submit(&pool, tx(alice, 1)).await;
        note_executed(&mut info, &bob_first);
        let second =
            maintain_pool_at_slice_boundary(&pool, &Balances::funded([alice, bob]), &mut info);

        assert_eq!(second.accounts_advanced, 2, "alice is re-asserted alongside bob");
        assert!(is_pending(&pool, alice_next));
    }
}

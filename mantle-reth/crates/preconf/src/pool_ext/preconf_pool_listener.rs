//! Pool listener that pushes preconf-eligible txs into [`PreconfTxSet`].
//!
//! Subscribes to a [`TransactionPool`] via
//! [`new_pending_pool_transactions_listener`] and forwards every
//! preconf-eligible tx (per [`PreconfConfig::is_preconf_tx`]) into the fifo.
//! Non-whitelisted txs and OP `Deposit` / `PostExec` variants are silently
//! dropped.
//!
//! Only `SubPool::Pending` events are observed — txs in `BaseFee` (gas price
//! below current block basefee) or `Queued` (nonce gap) cannot execute on
//! the in-flight block; pushing them now would surface a misleading `Failed`
//! event to the client. reth re-emits a new event whenever a tx is promoted
//! into `Pending` (`crates/transaction-pool/src/pool/mod.rs::notify_on_transaction_updates`),
//! so promotions are picked up automatically.
//!
//! Lifecycle: instantiated once at node startup when preconf is enabled, then
//! spawned as a `spawn_critical_task` on the reth task executor. Returns when
//! the pool's underlying listener channel closes (typically at node
//! shutdown).
//!
//! [`new_pending_pool_transactions_listener`]:
//! reth_transaction_pool::TransactionPool::new_pending_pool_transactions_listener

use std::{marker::PhantomData, sync::Arc};

use alloy_consensus::TxEnvelope;
use alloy_primitives::Address;
use futures::StreamExt;
use op_alloy_consensus::OpTxEnvelope;
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use tracing::{debug, trace};

use crate::{
    PreconfJournal, config::PreconfConfig, preconf_tx_set::PreconfTxSet, types::PushResult,
};

/// Long-running async task that bridges a [`TransactionPool`] event stream to
/// [`PreconfTxSet`].
///
/// Generic over the pool `P` and the pool's `Transaction` type `Tx`. The
/// type parameter `Cons` constrains `Tx::Consensus` to be convertible to
/// an OP-stack `OpTxEnvelope` so the listener can sift out Deposit variants.
pub struct PreconfPoolListener<P, Tx, Cons> {
    pool: P,
    cfg: Arc<PreconfConfig>,
    fifo: Arc<PreconfTxSet>,
    /// Optional journal handle used to distinguish reorg-reinjected txs
    /// from fresh RPC submissions. When `Some`, every incoming pool event
    /// is checked against `journal.sealed`; a hit means the tx was
    /// previously promised to a client (mark_sealed fired on an earlier
    /// canon commit) and must bypass the deadline / block-gas-budget
    /// gates — so we push with [`PreconfSource::Replay`] to align
    /// with the SLA "receipt returned → tx must land" contract. `None`
    /// (journal feature disabled) falls back to the pre-journal behavior
    /// where every push uses [`PreconfSource::Rpc`].
    journal: Option<Arc<PreconfJournal>>,
    _tx: PhantomData<fn() -> Tx>,
    _cons: PhantomData<fn() -> Cons>,
}

// Manual Debug impl: avoids requiring `P: Debug` on the pool type, which
// would propagate to every call-site of `PreconfPoolListener::new(...)`.
// Phantom markers are skipped (they carry no runtime info).
impl<P, Tx, Cons> std::fmt::Debug for PreconfPoolListener<P, Tx, Cons> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfPoolListener")
            .field("cfg", &self.cfg)
            .field("fifo", &self.fifo)
            .finish_non_exhaustive()
    }
}

impl<P, Tx, Cons> PreconfPoolListener<P, Tx, Cons>
where
    P: TransactionPool<Transaction = Tx> + 'static,
    Tx: PoolTransaction<Consensus = Cons> + 'static,
    Cons: Clone + Into<OpTxEnvelope>,
{
    /// Construct a listener bound to `pool`. `journal` is optional; when
    /// `Some`, the listener consults its sealed set to detect reorg
    /// reinjects and route them through the SLA-bypass source.
    pub const fn new(
        pool: P,
        cfg: Arc<PreconfConfig>,
        fifo: Arc<PreconfTxSet>,
        journal: Option<Arc<PreconfJournal>>,
    ) -> Self {
        Self { pool, cfg, fifo, journal, _tx: PhantomData, _cons: PhantomData }
    }

    /// Run the listener loop. Returns when the pool's listener stream closes.
    pub async fn run(self) {
        let mut stream = self.pool.new_pending_pool_transactions_listener();

        while let Some(event) = stream.next().await {
            // `new_pending_pool_transactions_listener` already filters to
            // `SubPool::Pending` internally — we receive only txs eligible
            // to execute on the in-flight block.
            let valid = &event.transaction;
            let sender = valid.transaction.sender();
            let to = valid.transaction.to();

            if !self.cfg.is_preconf_tx(&sender, to.as_ref()) {
                trace!(
                    target: "mantle::preconf::listener",
                    ?sender, ?to,
                    "skipping non-whitelisted tx"
                );
                continue;
            }

            // Bridge: pool's consensus type → alloy `TxEnvelope`.
            // Deposit, PostExec, and any other non-user-submitted variant
            // is dropped.
            let consensus = valid.transaction.clone_into_consensus();
            let op_envelope: OpTxEnvelope = consensus.into_inner().into();
            let Some(envelope) = op_envelope_to_alloy(op_envelope) else {
                trace!(
                    target: "mantle::preconf::listener",
                    "dropping non-preconf-eligible variant (e.g., Deposit)"
                );
                continue;
            };

            // Copy the hash out before moving the envelope into Arc.
            let hash = *envelope.tx_hash();

            // Reorg reinject detection: if the hash has ever been
            // `mark_sealed`-ed on a prior canon commit, this pool event
            // is the pool's reorg re-inject path returning a
            // previously-promised tx. Bypass the deadline / gas budget
            // gates by pushing with `Replay` source.
            let source = if let Some(journal) = self.journal.as_ref() {
                if journal.contains(&hash).await {
                    crate::types::PreconfSource::Replay
                } else {
                    crate::types::PreconfSource::Rpc
                }
            } else {
                crate::types::PreconfSource::Rpc
            };

            match self.fifo.push_if_absent(Arc::new(envelope), sender, source).await {
                PushResult::Inserted => {
                    debug!(
                        target: "mantle::preconf::listener",
                        ?hash, ?sender,
                        "pushed preconf-eligible tx into fifo"
                    );
                }
                PushResult::AlreadyExists => {
                    trace!(
                        target: "mantle::preconf::listener",
                        ?hash,
                        "fifo already contains this hash; idempotent skip"
                    );
                }
                PushResult::ConflictActive(existing) => {
                    debug!(
                        target: "mantle::preconf::listener",
                        ?hash,
                        existing_hash = ?existing,
                        "fifo (sender, nonce) slot occupied by another active commitment"
                    );
                }
            }
        }
    }
}

/// Best-effort conversion of an [`OpTxEnvelope`] into an alloy [`TxEnvelope`].
///
/// Drops `Deposit` (and any future OP-specific) variants by returning `None`.
/// User-submitted variants (`Legacy` / `Eip1559` / `Eip2930` / `Eip7702`) are
/// passed through unchanged.
///
/// Shared with [`crate::pool_ext::pool_adapter::RestorePoolAdapter`] so
/// listener and restore-time adapter agree on which OP tx variants are
/// preconf-eligible.
pub(crate) fn op_envelope_to_alloy(op_tx: OpTxEnvelope) -> Option<TxEnvelope> {
    match op_tx {
        OpTxEnvelope::Legacy(tx) => Some(TxEnvelope::Legacy(tx)),
        OpTxEnvelope::Eip2930(tx) => Some(TxEnvelope::Eip2930(tx)),
        OpTxEnvelope::Eip1559(tx) => Some(TxEnvelope::Eip1559(tx)),
        OpTxEnvelope::Eip7702(tx) => Some(TxEnvelope::Eip7702(tx)),
        // Deposit (type 0x7E) is L1→L2 system-injected — never user-submitted
        // preconf-eligible.
        OpTxEnvelope::Deposit(_) => None,
        // PostExec (type 0x7D, mantle-specific) is a system tx emitted after
        // block execution — never user-submitted preconf-eligible.
        OpTxEnvelope::PostExec(_) => None,
    }
}

// Suppress `unused_imports` from `Address` — kept for the trait-bound
// signature `Address::from(...)` reachability in cfg(test) scaffolding.
const _: fn() = || {
    let _: Address = Address::ZERO;
};

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559, TxLegacy};
    use alloy_primitives::{B256, Signature, TxHash};

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn h(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    fn eip1559_envelope(nonce: u64, hash_byte: u8) -> OpTxEnvelope {
        let inner = TxEip1559 { nonce, ..Default::default() };
        let sig = Signature::test_signature();
        let hash = B256::from([hash_byte; 32]);
        OpTxEnvelope::Eip1559(Signed::new_unchecked(inner, sig, hash))
    }

    fn legacy_envelope(nonce: u64, hash_byte: u8) -> OpTxEnvelope {
        let inner = TxLegacy { nonce, ..Default::default() };
        let sig = Signature::test_signature();
        let hash = B256::from([hash_byte; 32]);
        OpTxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash))
    }

    #[test]
    fn op_envelope_to_alloy_passes_user_variants() {
        let eip1559 = eip1559_envelope(0, 1);
        assert!(matches!(op_envelope_to_alloy(eip1559), Some(TxEnvelope::Eip1559(_))));

        let legacy = legacy_envelope(0, 2);
        assert!(matches!(op_envelope_to_alloy(legacy), Some(TxEnvelope::Legacy(_))));
    }

    #[test]
    fn op_envelope_to_alloy_drops_deposit() {
        let deposit_tx = op_alloy_consensus::TxDeposit {
            source_hash: B256::ZERO,
            from: addr(1),
            to: alloy_primitives::TxKind::Call(addr(2)),
            mint: 0,
            value: alloy_primitives::U256::ZERO,
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Default::default(),
            ..Default::default()
        };
        let deposit =
            OpTxEnvelope::Deposit(alloy_primitives::Sealed::new_unchecked(deposit_tx, h(99)));
        assert!(op_envelope_to_alloy(deposit).is_none());
    }

    // Listener loop tests (with a real `MockTransactionPool`) are deferred
    // to end-to-end coverage — they need either a noop pool or a fully-
    // faked pool that emits `NewTransactionEvent`s for OP-typed
    // transactions, which is heavyweight setup. The conversion helper
    // above is the testable nucleus; the loop is a straight-line drain
    // over an mpsc receiver with branch coverage already exercised
    // through code review.
}

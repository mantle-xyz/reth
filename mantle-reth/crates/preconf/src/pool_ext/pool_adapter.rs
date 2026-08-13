//! Bridge from a live [`TransactionPool`] to the [`crate::RestorePool`]
//! trait so [`crate::restore_preconf_state`] can re-admit and decode
//! journal-persisted commitments at node startup.
//!
//! The adapter is generic over the same three type parameters as
//! [`crate::pool_ext::preconf_pool_listener::PreconfPoolListener`] and
//! shares its `op_envelope_to_alloy`
//! helper so the "which OP tx variants are preconf-eligible" decision
//! stays in one place.
//!
//! ## `add_envelope` semantics
//!
//! Returns `Ok(recovered)` in **both** cases:
//!
//! - The tx was newly admitted to the pool.
//! - The pool rejected admission with `AlreadyImported` (typically because reth's own local-tx
//!   backup restored the same tx from disk before this call).
//!
//! In either case the caller ([`crate::restore_preconf_state`]) needs
//! the decoded envelope + sender to `push_if_absent` into the fifo —
//! whether the pool already knows about the tx is orthogonal to the
//! fifo push.
//!
//! Only genuine pool errors (invalid signature, nonce mismatch on the
//! post-restart state, etc.) surface as `Err(reason)`. The restore
//! helper logs and skips those entries.

use std::{marker::PhantomData, sync::Arc};

use alloy_primitives::Address;
use async_trait::async_trait;
use op_alloy_consensus::OpTxEnvelope;
use reth_prune_types::PruneSegment;
use reth_rpc_eth_types::utils::recover_raw_transaction;
use reth_storage_api::{DatabaseProviderFactory, PruneCheckpointReader, TransactionsProvider};
use reth_transaction_pool::{
    PoolPooledTx, PoolTransaction, TransactionOrigin, TransactionPool, error::PoolErrorKind,
};

use super::preconf_pool_listener::op_envelope_to_alloy;
use crate::journal::{CommitmentChainView, OnChain, RestorePool, RestoreSkip, RestoredEnvelope};

/// Adapter that lets a live [`TransactionPool`] play the [`RestorePool`]
/// role during startup restore.
///
/// Generic over the pool `P`, the pool's `Transaction` type `Tx`, and
/// the tx's consensus form `Cons` — matching the layout used by
/// [`crate::pool_ext::preconf_pool_listener::PreconfPoolListener`].
/// Cheap to hold: single-arc pool + zero-sized phantom markers.
#[derive(Clone)]
pub struct RestorePoolAdapter<P, Tx, Cons>
where
    P: Clone,
{
    pool: P,
    _tx: PhantomData<fn() -> Tx>,
    _cons: PhantomData<fn() -> Cons>,
}

// Manual `Debug`: skips the pool (would force `P: Debug` on every
// call-site) and the phantom markers (which carry no runtime info).
impl<P, Tx, Cons> std::fmt::Debug for RestorePoolAdapter<P, Tx, Cons>
where
    P: Clone,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestorePoolAdapter").finish_non_exhaustive()
    }
}

impl<P, Tx, Cons> RestorePoolAdapter<P, Tx, Cons>
where
    P: Clone,
{
    /// Wrap a pool handle for use with [`crate::restore_preconf_state`].
    pub const fn new(pool: P) -> Self {
        Self { pool, _tx: PhantomData, _cons: PhantomData }
    }
}

#[async_trait]
impl<P, Tx, Cons> RestorePool for RestorePoolAdapter<P, Tx, Cons>
where
    P: TransactionPool<Transaction = Tx> + Clone + Send + Sync + 'static,
    Tx: PoolTransaction<Consensus = Cons> + Send + Sync + 'static,
    Cons: Clone + Into<OpTxEnvelope> + Send + Sync + 'static,
{
    async fn contains(&self, hash: &alloy_primitives::TxHash) -> bool {
        self.pool.get(hash).is_some()
    }

    fn remove_transactions(&self, hashes: Vec<alloy_primitives::TxHash>) {
        // reth's `remove_transactions` returns a `Vec<Arc<...>>` of the
        // removed pool txs; we discard it here. Absent hashes are a
        // no-op (empty return). Same-thread mutex; no `.await`.
        let _ = self.pool.remove_transactions(hashes);
    }

    fn recover_slot(&self, tx_rlp: &alloy_primitives::Bytes) -> Option<(Address, u64)> {
        let recovered = recover_raw_transaction::<PoolPooledTx<P>>(tx_rlp.as_ref()).ok()?;
        Some((recovered.signer(), alloy_consensus::Transaction::nonce(recovered.inner())))
    }

    async fn add_envelope(
        &self,
        tx_rlp: &alloy_primitives::Bytes,
    ) -> Result<RestoredEnvelope, RestoreSkip> {
        // Decode + recover — same pipeline the RPC handler uses.
        let recovered = recover_raw_transaction::<PoolPooledTx<P>>(tx_rlp.as_ref())
            .map_err(|e| RestoreSkip::Rejected(format!("decode/recover failed: {e}")))?;
        let sender = recovered.signer();

        // Extract the alloy `TxEnvelope` for fifo push. Deposit /
        // PostExec variants should never appear in the journal (only
        // preconf-RPC-submitted txs are persisted), but drop them
        // defensively — matches the listener's filter.
        let consensus = <Tx as PoolTransaction>::pooled_into_consensus(recovered.inner().clone());
        let op_env: OpTxEnvelope = consensus.into();
        let envelope = op_envelope_to_alloy(op_env).ok_or_else(|| {
            RestoreSkip::Rejected("non-preconf-eligible variant (Deposit / PostExec)".to_string())
        })?;

        // Attempt to admit. `AlreadyImported` is treated as benign: the
        // restore path needs the recovered envelope either way, and
        // whether the pool already held the tx is orthogonal. (It cannot
        // be reth's own local-tx backup loader that put it there —
        // `cli::node` runs restore before `spawn_maintenance_tasks`
        // spawns that loader — so this arm is defensive rather than
        // expected.)
        let pool_tx = <Tx as PoolTransaction>::from_pooled(recovered);
        match self.pool.add_transaction(TransactionOrigin::External, pool_tx).await {
            Ok(_) => {}
            Err(e) if matches!(e.kind, PoolErrorKind::AlreadyImported) => {}
            // The sender's nonce has moved past this transaction. That is
            // **not** the same as "this transaction is on chain", which is what
            // this arm used to conclude: `is_nonce_too_low` reduces to
            // `NonceNotConsistent { tx, state } => tx < state`, and the check
            // that produces it (`validate_sender_nonce`) compares the tx's nonce
            // against the *account's* nonce and never looks at the hash. A
            // different transaction on the same nonce yields a byte-identical
            // error.
            //
            // So don't conclude — ask the chain. The caller does that; all this
            // arm can honestly report is that the nonce is gone.
            Err(e) if matches!(&e.kind, PoolErrorKind::InvalidTransaction(err) if err.is_nonce_too_low()) =>
            {
                return Err(RestoreSkip::NonceConsumed(format!("{}", e.kind)));
            }
            Err(e) => return Err(RestoreSkip::Rejected(format!("pool rejected: {}", e.kind))),
        }

        Ok(RestoredEnvelope { envelope, from: sender })
    }
}

// Keep the `Arc` import reachable from downstream call-sites even
// though this module doesn't use it directly — some downstream
// callers construct `Arc<RestorePoolAdapter<...>>` and importing it
// through this module is convenient.
#[allow(dead_code)]
type _ArcHint<P, Tx, Cons> = Arc<RestorePoolAdapter<P, Tx, Cons>>;

/// Lets a node provider answer the restore path's chain question — whether a
/// commitment whose nonce is gone is the transaction that consumed it.
///
/// A wrapper rather than a blanket `impl<P: TransactionsProvider + ..>`: a
/// blanket impl would forbid every other implementation of
/// [`CommitmentChainView`], including the scripted stubs the restore tests need
/// (Rust coherence cannot prove a local type does *not* implement the bounds).
///
/// Both halves are plain provider reads and every node provider has them —
/// `FullProvider` requires `BlockReaderIdExt` (⊃ `BlockReader` ⊃
/// `TransactionsProvider`) and `PruneCheckpointReader`.
#[derive(Debug, Clone)]
pub struct ProviderChainView<P>(P);

impl<P> ProviderChainView<P> {
    /// Wrap a provider for use with [`crate::restore_preconf_state`].
    pub const fn new(provider: P) -> Self {
        Self(provider)
    }
}

impl<P> CommitmentChainView for ProviderChainView<P>
where
    // Exactly what `FullProvider` already
    // guarantees, so callers need no extra
    // where-clause. In particular
    // `PruneCheckpointReader` sits on the
    // *database* provider, not on the outer
    // handle — `BlockchainProvider` happens to
    // implement it directly, but a generic
    // `N::Provider` cannot rely on that.
    P: TransactionsProvider
        + DatabaseProviderFactory<Provider: PruneCheckpointReader>
        + Send
        + Sync,
{
    /// `Unknown` on a miss whenever the transaction-lookup segment has **ever**
    /// been pruned, without trying to relate the prune height to the
    /// transaction's: `JournalEntry::block_height` is the height predicted when
    /// the receipt went out and can drift, so it is not a sound basis for "the
    /// prune did not reach my block". A pruned index therefore makes every miss
    /// unknowable — which is why pruning it is incompatible with running preconf.
    ///
    /// Residue, stated rather than papered over: pruning that is *configured but
    /// has not run yet* leaves no checkpoint, so a miss is reported as `No`. That
    /// window closes the first time the pruner runs.
    fn commitment_on_chain(&self, hash: &alloy_primitives::TxHash) -> OnChain {
        // `_with_meta` rather than the plain lookup: the caller needs the block
        // number to start the retention clock, and it sits on the same trait, so
        // this costs no extra bound and no second query.
        match self.0.transaction_by_hash_with_meta(*hash) {
            Ok(Some((_, meta))) => OnChain::Yes { height: meta.block_number },
            Ok(None) => {
                // Only on a miss, and a miss only happens for an entry whose
                // nonce is already gone — so at most once per lost commitment,
                // once per process start.
                let pruned = self
                    .0
                    .database_provider_ro()
                    .and_then(|db| db.get_prune_checkpoint(PruneSegment::TransactionLookup));
                match pruned {
                    Ok(None) => OnChain::No,
                    // Pruned, or we cannot even tell whether it was pruned.
                    Ok(Some(_)) | Err(_) => OnChain::Unknown,
                }
            }
            Err(_) => OnChain::Unknown,
        }
    }
}

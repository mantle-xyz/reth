//! Turning journal bytes back into queue entries at startup.
//!
//! Two pieces, both for [`crate::restore_preconf_state`]:
//!
//! - [`RestoreDirect`] decodes a persisted commitment's wire bytes into the envelope + sender the
//!   fifo needs.
//! - [`ProviderChainView`] lets a node provider answer what restore asks of the chain: where a
//!   sender's nonce stands, and whether a commitment already landed.
//!
//! **Neither touches the transaction pool.** Restore used to re-admit every
//! commitment there and read the outcome off the pool's validator; since direct
//! admission a commitment never enters the pool at all, so what is left is
//! decoding plus two chain reads. [`RestoreDirect`] holds nothing — it is
//! generic over the pool's transaction type only because the journal stores wire
//! bytes and decoding them needs a type to decode *into*.
//!
//! It shares admission's `op_envelope_to_alloy` helper so the "which OP tx
//! variants are preconf-eligible" decision stays in one place.
//!
//! ## What `add_envelope` refuses
//!
//! `RestoreSkip::Rejected` for bytes that will not decode, and for the
//! `Deposit` / `PostExec` variants — which should never reach the journal, since only
//! preconf-RPC submissions are persisted, but are filtered anyway to match
//! admission's own gate. [`crate::restore_preconf_state`] logs and skips those
//! entries; everything else is handed back for `push_if_absent`.

use std::marker::PhantomData;

use alloy_primitives::Address;
use async_trait::async_trait;
use op_alloy_consensus::OpTxEnvelope;
use reth_prune_types::PruneSegment;
use reth_rpc_eth_types::utils::recover_raw_transaction;
use reth_storage_api::{
    DatabaseProviderFactory, PruneCheckpointReader, StateProvider, StateProviderFactory,
    TransactionsProvider,
};
use reth_transaction_pool::PoolTransaction;

use crate::{
    admission::op_envelope_to_alloy,
    journal::{CommitmentChainView, OnChain, RestoreSkip, RestoreSource, RestoredEnvelope},
};

/// Turns journal bytes back into something the queue can hold.
///
/// Holds nothing. What this replaces wrapped a live `TransactionPool`,
/// because restore used to re-admit every commitment there and read the
/// outcome off the pool's validator. Commitments do not enter the pool any
/// more, so all that is left of the job is decoding — and the questions the
/// pool used to answer as a side effect are now asked of the chain directly.
///
/// Still generic over the pool's `Transaction` type: the journal holds wire
/// bytes, and decoding them needs a pooled envelope type to decode *into*.
/// That is a type-level dependency, not a handle — nothing is held.
#[derive(Clone)]
pub struct RestoreDirect<Tx, Cons> {
    _tx: PhantomData<fn() -> Tx>,
    _cons: PhantomData<fn() -> Cons>,
}

impl<Tx, Cons> std::fmt::Debug for RestoreDirect<Tx, Cons> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestoreDirect").finish_non_exhaustive()
    }
}

impl<Tx, Cons> Default for RestoreDirect<Tx, Cons> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Tx, Cons> RestoreDirect<Tx, Cons> {
    /// For use with [`crate::restore_preconf_state`].
    pub const fn new() -> Self {
        Self { _tx: PhantomData, _cons: PhantomData }
    }
}

#[async_trait]
impl<Tx, Cons> RestoreSource for RestoreDirect<Tx, Cons>
where
    Tx: PoolTransaction<Consensus = Cons> + Send + Sync + 'static,
    Cons: Clone + Into<OpTxEnvelope> + Send + Sync + 'static,
{
    fn recover_slot(&self, tx_rlp: &alloy_primitives::Bytes) -> Option<(Address, u64)> {
        let recovered =
            recover_raw_transaction::<<Tx as PoolTransaction>::Pooled>(tx_rlp.as_ref()).ok()?;
        Some((recovered.signer(), alloy_consensus::Transaction::nonce(recovered.inner())))
    }

    async fn add_envelope(
        &self,
        tx_rlp: &alloy_primitives::Bytes,
    ) -> Result<RestoredEnvelope, RestoreSkip> {
        // Decode + recover — same pipeline admission uses.
        let recovered = recover_raw_transaction::<<Tx as PoolTransaction>::Pooled>(tx_rlp.as_ref())
            .map_err(|e| RestoreSkip::Rejected(format!("decode/recover failed: {e}")))?;
        let sender = recovered.signer();

        // Extract the alloy `TxEnvelope` for fifo push. Deposit /
        // PostExec variants should never appear in the journal (only
        // preconf-RPC-submitted txs are persisted), but drop them
        // defensively — matches admission's own filter.
        let consensus = <Tx as PoolTransaction>::pooled_into_consensus(recovered.inner().clone());
        let op_env: OpTxEnvelope = consensus.into();
        let envelope = op_envelope_to_alloy(op_env).ok_or_else(|| {
            RestoreSkip::Rejected("non-preconf-eligible variant (Deposit / PostExec)".to_string())
        })?;

        Ok(RestoredEnvelope { envelope, from: sender })
    }
}

/// Lets a node provider answer the restore path's chain question — whether a
/// commitment whose nonce is gone is the transaction that consumed it.
///
/// A wrapper rather than a blanket impl, which would forbid the scripted stubs
/// the restore tests need (coherence cannot prove a local type does *not*
/// implement the bounds).
///
/// Both halves are plain provider reads and every node provider has them —
/// `FullProvider` requires `BlockReaderIdExt` (⊃ `BlockReader` ⊃
/// `TransactionsProvider`) on the handle itself, and `PruneCheckpointReader` on
/// its `DatabaseProviderFactory::Provider`; see the where-clause below.
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
    // Exactly what `FullProvider` guarantees, so
    // callers need no extra where-clause of
    // their own. See the type docs.
    P: TransactionsProvider
        + DatabaseProviderFactory<Provider: PruneCheckpointReader>
        + StateProviderFactory
        + Send
        + Sync,
{
    /// `Unknown` on any miss once the transaction-lookup segment has **ever** been pruned:
    /// `JournalEntry::block_height` is only a prediction, so "the prune missed it" cannot be
    /// decided. Pruning is thus incompatible with preconf; until it first runs, a miss reads `No`.
    fn account_nonce(&self, sender: &Address) -> Option<u64> {
        self.0.latest().ok()?.account_nonce(sender).ok().flatten()
    }

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

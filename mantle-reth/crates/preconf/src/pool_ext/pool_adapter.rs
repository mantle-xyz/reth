//! Bridge from a live [`TransactionPool`] to the [`crate::RestorePool`]
//! trait so [`crate::restore_preconf_state`] can re-admit and decode
//! journal-persisted commitments at node startup.
//!
//! The adapter is generic over the same three type parameters as
//! [`crate::pool_ext::preconf_pool_listener::PreconfPoolListener`] and
//! shares its [`op_envelope_to_alloy`](super::preconf_pool_listener::op_envelope_to_alloy)
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

use async_trait::async_trait;
use op_alloy_consensus::OpTxEnvelope;
use reth_rpc_eth_types::utils::recover_raw_transaction;
use reth_transaction_pool::{
    PoolPooledTx, PoolTransaction, TransactionOrigin, TransactionPool, error::PoolErrorKind,
};

use super::preconf_pool_listener::op_envelope_to_alloy;
use crate::journal::{RestorePool, RestoredEnvelope};

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

    async fn add_envelope(
        &self,
        tx_rlp: &alloy_primitives::Bytes,
    ) -> Result<RestoredEnvelope, String> {
        // Decode + recover — same pipeline the RPC handler uses.
        let recovered = recover_raw_transaction::<PoolPooledTx<P>>(tx_rlp.as_ref())
            .map_err(|e| format!("decode/recover failed: {e}"))?;
        let sender = recovered.signer();

        // Extract the alloy `TxEnvelope` for fifo push. Deposit /
        // PostExec variants should never appear in the journal (only
        // preconf-RPC-submitted txs are persisted), but drop them
        // defensively — matches the listener's filter.
        let consensus = <Tx as PoolTransaction>::pooled_into_consensus(recovered.inner().clone());
        let op_env: OpTxEnvelope = consensus.into();
        let envelope = op_envelope_to_alloy(op_env)
            .ok_or_else(|| "non-preconf-eligible variant (Deposit / PostExec)".to_string())?;

        // Attempt to admit. `AlreadyImported` is expected and benign:
        // reth's own local-tx backup may have restored the same tx
        // from disk before we ran. The restore path still needs the
        // recovered envelope regardless.
        let pool_tx = <Tx as PoolTransaction>::from_pooled(recovered);
        match self.pool.add_transaction(TransactionOrigin::External, pool_tx).await {
            Ok(_) => {}
            Err(e) if matches!(e.kind, PoolErrorKind::AlreadyImported) => {}
            Err(e) => return Err(format!("pool rejected: {}", e.kind)),
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

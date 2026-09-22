//! The preconf path's front door.
//!
//! One call takes a raw transaction from "bytes off the wire" to "queued, and
//! the builder has been told". Everything that can refuse it refuses here,
//! synchronously, with a reason — which is the point. The route this replaces
//! reached the queue through the transaction pool and a listener, so a
//! transaction the pool parked rather than rejected produced no refusal at
//! all: the client waited out the whole timeout to be told nothing happened.
//!
//! ## Order
//!
//! Cheapest judgement first, and two placements are deliberate rather than
//! incidental:
//!
//! * the **verdict claim** comes before the validator, because a verdict has to be frozen before
//!   any step that can fail expensively — otherwise every failure path owes a rollback;
//! * the **queue's own rules** come last, because they are the only ones needing its lock, and
//!   holding it for a request already doomed is time no other request can use.
//!
//! ## What it does not have
//!
//! No transaction pool. Not an omission — the preconf path answers nonce,
//! balance and duplicate questions from its own queue and from chain state,
//! and each of the three tempting reads of the pool was considered and
//! refused. A pool handle here would make that invariant unenforceable, so
//! there is nothing to hold one.
//!
//! The validator chain is shared with the pool rather than duplicated: both
//! paths must give one answer to "is this transaction valid", and the chain's
//! own head-tracking is driven once, by pool maintenance, for both.

use std::sync::Arc;

use alloy_consensus::{BlockHeader, Transaction, TxEnvelope, TxType};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use op_alloy_consensus::OpTxEnvelope;
use reth_rpc_eth_types::utils::recover_raw_transaction;
use reth_storage_api::{BlockReaderIdExt, StateProvider, StateProviderFactory};
use reth_transaction_pool::{
    PoolTransaction, TransactionOrigin, TransactionValidationOutcome, TransactionValidator,
    error::InvalidPoolTransactionError,
};
use tokio::sync::oneshot;
use tracing::{debug, trace};

use crate::{
    PreconfClassifier, PreconfConfig, PreconfTxSet,
    classifier::PreconfClaimError,
    preconf_tx_set::{AdmitRequest, Admitted, Capacity},
    types::{PreconfError, PreconfReceipt, PreconfSource},
};

/// The transaction types the preconf path accepts.
///
/// A list rather than a set of exclusions, so a type added upstream is
/// refused until someone decides otherwise. Set-code transactions are the
/// reason it exists: one of them re-points an account's code, which changes
/// what every transaction queued behind it would do — a coupling this queue
/// has no way to account for. Blob transactions are refused for the plainer
/// reason that an L2 has nowhere to put them.
///
/// It bars *establishing* a delegation, and nothing else. Calling an account
/// that already has one, or sending from one, is an ordinary transaction and
/// goes through untouched.
const fn accepts(ty: TxType) -> bool {
    matches!(ty, TxType::Legacy | TxType::Eip2930 | TxType::Eip1559)
}

/// The type byte of an EIP-2718 envelope, without decoding it.
///
/// Read ahead of the decode, and it has to be: the pooled type this node
/// builds with has no blob variant, so a blob transaction fails to decode at
/// all and the caller would hear "malformed" about something perfectly well
/// formed that simply is not accepted here. A type is not a parse error.
///
/// Per EIP-2718 a leading byte of `0x00..=0x7f` is the type; anything above
/// is the RLP list prefix of a legacy transaction.
fn peek_tx_type(bytes: &[u8]) -> Option<u8> {
    match bytes.first()? {
        &ty @ 0x00..=0x7f => Some(ty),
        _ => Some(TxType::Legacy as u8),
    }
}

/// Best-effort conversion of an [`OpTxEnvelope`] into an alloy [`TxEnvelope`].
///
/// Drops `Deposit` (and any future OP-specific) variants by returning `None`.
/// User-submitted variants (`Legacy` / `Eip1559` / `Eip2930` / `Eip7702`) are
/// passed through unchanged.
///
/// Shared with [`crate::pool_ext::pool_adapter::RestoreDirect`] so admission
/// and the restore-time adapter agree on which OP tx variants are
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

/// Turns a validator refusal into something the client can act on.
///
/// The route this replaces flattened every one of these into a single string
/// with a `pool rejected:` prefix, which told a caller nothing about whether
/// to retry, to top up, or to give up. The reason itself is preserved either
/// way; what is added is a variant to match on.
fn refusal(err: &InvalidPoolTransactionError) -> PreconfError {
    PreconfError::PoolRejected(err.to_string())
}

/// An admitted transaction, and what the caller still needs to know about
/// it.
///
/// The RPC handler waits on the client's channel after this returns, and that
/// wait is keyed on the transaction's identity — which admission has already
/// worked out and the caller would otherwise decode a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmittedTx {
    /// Whether this created an entry or revived one.
    pub outcome: Admitted,
    /// Transaction hash.
    pub hash: alloy_primitives::TxHash,
    /// Recovered sender.
    pub sender: Address,
    /// Sender nonce.
    pub nonce: u64,
}

/// Admission as the RPC handler sees it.
///
/// Object-safe on purpose. The concrete type carries the validator chain and
/// the state provider as parameters, and the handler is built in a later
/// node-builder phase than the one where those exist — so what crosses
/// between them is this, not the type.
#[async_trait::async_trait]
pub trait DynAdmission: Send + Sync + std::fmt::Debug {
    /// See [`PreconfAdmission::admit`].
    async fn admit(
        &self,
        bytes: Bytes,
        origin_instant: std::time::Instant,
        responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
    ) -> Result<AdmittedTx, PreconfError>;
}

#[async_trait::async_trait]
impl<V, Pr> DynAdmission for PreconfAdmission<V, Pr>
where
    V: TransactionValidator + std::fmt::Debug + 'static,
    V::Transaction: PoolTransaction<Pooled: Decodable2718>,
    OpTxEnvelope: From<<V::Transaction as PoolTransaction>::Consensus>,
    Pr: StateProviderFactory + BlockReaderIdExt + std::fmt::Debug + 'static,
{
    async fn admit(
        &self,
        bytes: Bytes,
        origin_instant: std::time::Instant,
        responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
    ) -> Result<AdmittedTx, PreconfError> {
        Self::admit(self, &bytes, origin_instant, responder).await
    }
}

/// Decides whether a transaction may join the preconf queue, and puts it
/// there if so.
///
/// Holds no pool; see the module docs.
pub struct PreconfAdmission<V, Pr> {
    validator: V,
    provider: Pr,
    fifo: Arc<PreconfTxSet>,
    classifier: Arc<PreconfClassifier>,
    cfg: Arc<PreconfConfig>,
    /// What the queue may hold, taken from the node's own pool configuration
    /// so that tuning one tunes both.
    capacity: Capacity,
}

impl<V, Pr> std::fmt::Debug for PreconfAdmission<V, Pr> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfAdmission")
            .field("cfg", &self.cfg)
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl<V, Pr> PreconfAdmission<V, Pr> {
    /// Bind admission to the validator chain, chain state and queue it works
    /// against.
    pub const fn new(
        validator: V,
        provider: Pr,
        fifo: Arc<PreconfTxSet>,
        classifier: Arc<PreconfClassifier>,
        cfg: Arc<PreconfConfig>,
        capacity: Capacity,
    ) -> Self {
        Self { validator, provider, fifo, classifier, cfg, capacity }
    }
}

impl<V, Pr> PreconfAdmission<V, Pr>
where
    V: TransactionValidator + 'static,
    V::Transaction: PoolTransaction<Pooled: Decodable2718>,
    // A conversion rather than an equality, matching how the node states the
    // same relation: for any OP-stack primitives the two coincide, but the
    // bound the compiler can see is the `From` impl.
    OpTxEnvelope: From<<V::Transaction as PoolTransaction>::Consensus>,
    Pr: StateProviderFactory + BlockReaderIdExt + 'static,
{
    /// Decide on `bytes`, and queue them if they pass.
    ///
    /// `responder` is the client's channel; it goes into the entry under the
    /// queue's own lock, so the builder cannot reach an entry with nobody to
    /// answer. The instant beside it is when the request arrived, which is the
    /// clock the dispatch deadline measures — not when this call happened to
    /// run.
    pub async fn admit(
        &self,
        bytes: &Bytes,
        origin_instant: std::time::Instant,
        responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
    ) -> Result<AdmittedTx, PreconfError> {
        // Before the decode — see `peek_tx_type`.
        let ty = peek_tx_type(bytes)
            .ok_or_else(|| PreconfError::Internal("empty transaction payload".to_string()))?;
        if !TxType::try_from(ty).is_ok_and(accepts) {
            return Err(PreconfError::UnsupportedTxType { ty });
        }

        let recovered =
            recover_raw_transaction::<<V::Transaction as PoolTransaction>::Pooled>(bytes)
                .map_err(|e| PreconfError::Internal(e.to_string()))?;
        let pool_tx = <V::Transaction as PoolTransaction>::from_pooled(recovered);

        let sender = pool_tx.sender();
        let hash = *pool_tx.hash();
        let nonce = pool_tx.nonce();
        let gas_limit = Transaction::gas_limit(&pool_tx);
        let to = match pool_tx.kind() {
            TxKind::Call(to) => Some(to),
            TxKind::Create => None,
        };

        // Not the binding decision — `claim_preconf` below consults the same
        // allowlists and freezes what it finds. This is here so a sender who
        // was never allowlisted is turned away before a state read and a
        // validator round trip.
        if !self.classifier.preview_eligibility(&sender, to.as_ref()) {
            trace!(target: "mantle::preconf::admission", ?sender, ?to, ?hash, "not allowlisted");
            return Err(PreconfError::NotPreconfEligible);
        }

        if gas_limit > self.cfg.preconf_max_gas_per_tx {
            return Err(PreconfError::PreconfGasLimitExceeded {
                gas_limit,
                max: self.cfg.preconf_max_gas_per_tx,
            });
        }

        // **Where eligibility is decided.** The deciding fact — that the
        // client asked for a preconfirmation rather than sending an ordinary
        // transaction — exists nowhere else; one layer down the two are
        // indistinguishable.
        // Previewed as allowlisted a moment ago, so a refusal here means
        // governance moved in between. Same answer either way.
        if let Err(PreconfClaimError::NotAllowlisted) =
            self.classifier.claim_preconf(hash, &sender, to.as_ref())
        {
            return Err(PreconfError::NotPreconfEligible);
        }

        // From here every failure owes the verdict back, or the sender's nonce
        // stays claimed by a transaction that is not going anywhere.
        match self.decide(pool_tx, sender, hash, origin_instant, responder).await {
            Ok(outcome) => Ok(AdmittedTx { outcome, hash, sender, nonce }),
            Err(err) => {
                // Kept under its original name so existing dashboards follow
                // the judgement to where it moved. What it counts is narrower
                // now: the queue answers from chain state plus its own
                // contents, so a gap is the client's own doing rather than
                // possibly ours.
                if matches!(err, PreconfError::NonceGap { .. }) {
                    metrics::counter!("preconf.rpc.nonce_gap_rejected_total").increment(1);
                }
                // Whether the record may actually go is not this call site's
                // to work out — a commitment already acknowledged keeps it.
                self.classifier.release_preconf_claim(&hash);
                Err(err)
            }
        }
    }

    /// Everything after the verdict is frozen: the validator, then the queue.
    ///
    /// Split out so the caller has one place to release the verdict from,
    /// rather than a release on every branch that can fail.
    async fn decide(
        &self,
        pool_tx: V::Transaction,
        sender: Address,
        hash: alloy_primitives::TxHash,
        origin_instant: std::time::Instant,
        responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
    ) -> Result<Admitted, PreconfError> {
        // The same chain the pool puts its own transactions through, so the
        // two paths cannot disagree about what is valid.
        let cost = pool_tx.cost().saturating_add(pool_tx.extra_balance_cost());
        // The type gate ran first, so this is one of the three it lets
        // through and the conversion cannot fail.
        let op_envelope = OpTxEnvelope::from(pool_tx.clone_into_consensus().into_inner());
        let Some(envelope) = op_envelope_to_alloy(op_envelope) else {
            return Err(PreconfError::Internal("unsupported envelope past the type gate".into()));
        };
        let bytecode_hash =
            match self.validator.validate_transaction(TransactionOrigin::External, pool_tx).await {
                TransactionValidationOutcome::Valid { bytecode_hash, .. } => bytecode_hash,
                TransactionValidationOutcome::Invalid(_, err) => {
                    debug!(target: "mantle::preconf::admission", ?hash, %err, "validator refused");
                    return Err(refusal(&err));
                }
                TransactionValidationOutcome::Error(_, err) => {
                    return Err(PreconfError::Internal(err.to_string()));
                }
            };

        // Read once, from the state the queue's own baseline is measured
        // against.
        let state = self.provider.latest().map_err(|e| PreconfError::Internal(e.to_string()))?;
        let chain_nonce = state
            .account_nonce(&sender)
            .map_err(|e| PreconfError::Internal(e.to_string()))?
            .unwrap_or(0);
        let chain_balance = state
            .account_balance(&sender)
            .map_err(|e| PreconfError::Internal(e.to_string()))?
            .unwrap_or_default();

        // The floor a transaction has to clear. While a build is running the
        // queue knows it exactly; between builds the chain's own tip is the
        // closest honest answer, and it is what makes the floor track the
        // chain rather than whatever the last build happened to leave behind.
        let chain_base_fee = self
            .provider
            .latest_header()
            .map_err(|e| PreconfError::Internal(e.to_string()))?
            .and_then(|header| header.base_fee_per_gas())
            .unwrap_or_default();

        self.fifo
            .admit(
                &self.classifier,
                AdmitRequest {
                    tx: Arc::new(envelope),
                    from: sender,
                    source: PreconfSource::Rpc,
                    responder: Some((origin_instant, responder)),
                    chain_nonce,
                    chain_balance,
                    chain_base_fee,
                    cost: U256::from(cost),
                    bytecode_hash,
                },
                self.capacity,
            )
            .await
    }
}

/// The queue's ceilings, as the node's pool is configured.
///
/// Read from the live `PoolConfig` rather than restated, so `--txpool.*`
/// moves both paths together and neither can drift into being the looser one.
pub fn capacity_from_pool_config(
    cfg: &reth_transaction_pool::PoolConfig,
    preconf: &PreconfConfig,
) -> Capacity {
    Capacity {
        max_txs: cfg.pending_limit.max_txs,
        max_size: cfg.pending_limit.max_size,
        max_account_slots: cfg.max_account_slots,
        max_inflight_delegated: cfg.max_inflight_delegated_slot_limit,
        // Derived, not configured directly, so retuning the per-block budget
        // carries the queue ceiling with it.
        max_queued_gas: preconf
            .preconf_max_gas_per_block
            .saturating_mul(preconf.preconf_queue_gas_blocks),
    }
}

#[cfg(test)]
mod verdict_release_tests {
    //! What a refused admission owes back.
    //!
    //! Freezing the verdict happens before the validator runs, so every path
    //! that fails after it has to hand the record back — otherwise the hash
    //! keeps an eligible verdict for a transaction that is going nowhere, and
    //! a verdict is immutable for the life of the transaction. The sender
    //! could never get those bytes preconfirmed again.
    //!
    //! One exception, and it is the whole reason the release is not
    //! unconditional: a commitment whose receipt has already gone out keeps
    //! its record and its nonce.

    use super::*;
    use crate::classifier::{DEFAULT_VERDICT_CACHE_CAP, Verdict};
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{B256, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use reth_optimism_primitives::OpBlock;
    use reth_optimism_txpool::OpPooledTransaction;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::error::InvalidPoolTransactionError;
    use std::collections::HashSet;

    const RECIPIENT: Address = Address::new([0x42; 20]);

    /// Refuses everything, which is the only branch these tests are about.
    #[derive(Debug)]
    struct AlwaysRefuses;

    impl TransactionValidator for AlwaysRefuses {
        type Transaction = OpPooledTransaction;
        type Block = OpBlock;

        async fn validate_transaction(
            &self,
            _origin: TransactionOrigin,
            transaction: Self::Transaction,
        ) -> TransactionValidationOutcome<Self::Transaction> {
            TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::Underpriced,
            )
        }
    }

    struct Fixture {
        admission: PreconfAdmission<AlwaysRefuses, MockEthProvider>,
        classifier: Arc<PreconfClassifier>,
        fifo: Arc<PreconfTxSet>,
        signer: PrivateKeySigner,
    }

    fn fixture() -> Fixture {
        let signer =
            PrivateKeySigner::from_bytes(&B256::from([0x11; 32])).expect("valid secp256k1 scalar");
        let classifier = Arc::new(PreconfClassifier::new(
            false,
            std::time::Duration::from_secs(3600),
            DEFAULT_VERDICT_CACHE_CAP,
        ));
        classifier.update_whitelist(
            [(signer.address(), RECIPIENT)].into_iter().collect(),
            HashSet::default(),
            HashSet::default(),
        );

        let provider = MockEthProvider::default();
        provider.add_account(signer.address(), ExtendedAccount::new(0, U256::from(1u64)));

        let fifo = Arc::new(PreconfTxSet::new(16));
        let cfg = PreconfConfig {
            enabled: true,
            preconf_max_gas_per_tx: 1_000_000,
            ..Default::default()
        };
        let admission = PreconfAdmission::new(
            AlwaysRefuses,
            provider,
            fifo.clone(),
            classifier.clone(),
            Arc::new(cfg),
            Capacity {
                max_txs: 16,
                max_size: 1 << 20,
                max_account_slots: 16,
                max_inflight_delegated: 1,
                max_queued_gas: u64::MAX,
            },
        );
        Fixture { admission, classifier, fifo, signer }
    }

    /// Signed for real: the sender is recovered cryptographically, so a
    /// fabricated signature would not survive the decode.
    fn signed_raw(signer: &PrivateKeySigner, nonce: u64) -> (Bytes, B256) {
        let tx = TxEip1559 {
            chain_id: 10,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(RECIPIENT),
            value: U256::from(1u64),
            ..Default::default()
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("in-memory signer");
        let signed = tx.into_signed(signature);
        let hash = *signed.hash();
        (TxEnvelope::Eip1559(signed).encoded_2718().into(), hash)
    }

    #[tokio::test]
    async fn a_refusal_hands_back_the_verdict_it_froze() {
        let f = fixture();
        let (raw, hash) = signed_raw(&f.signer, 0);
        let (resp, _rx) = oneshot::channel();

        f.admission
            .admit(&raw, std::time::Instant::now(), resp)
            .await
            .expect_err("the validator refuses everything");

        assert_eq!(f.classifier.verdict(&hash), None, "the frozen verdict must be released");
        assert!(!f.fifo.contains(&hash).await, "and nothing may be left queued");
    }

    /// The exception. Reachable shape: the transaction was applied and its
    /// receipt returned, but its block is not canonical yet — so nothing has
    /// set `committed_height` to protect the record. A same-hash resubmit in
    /// that window is re-validated and can fail on account state alone, since
    /// Mantle recomputes the L1 and operator fees every time.
    ///
    /// The assertions key on `is_promised`, not on the verdict variant:
    /// `mark_promised` sets the flag without rewriting the verdict, so the
    /// ordinary flow leaves `Eligible` + promised — exactly the records that
    /// must survive.
    #[tokio::test]
    async fn a_refusal_must_not_drop_a_commitment_already_promised() {
        let f = fixture();
        let (raw, hash) = signed_raw(&f.signer, 0);
        let sender = f.signer.address();
        let (resp, _rx) = oneshot::channel();

        assert_eq!(f.classifier.claim_preconf(hash, &sender, Some(&RECIPIENT)), Ok(()));
        assert_eq!(f.classifier.mark_promised(hash, &sender, 0, 0), Ok(()));
        assert_eq!(
            f.classifier.verdict(&hash),
            Some(Verdict::Eligible),
            "precondition: the flag is set without rewriting the verdict",
        );

        f.admission
            .admit(&raw, std::time::Instant::now(), resp)
            .await
            .expect_err("the validator refuses the resubmit");

        assert!(f.classifier.is_promised(&hash), "the commitment record must survive");
        assert_eq!(
            f.classifier.slot_owner(&sender, 0),
            Some(hash),
            "and so must the nonce it was promised against",
        );
    }

    /// The type gate runs before the verdict is frozen, so there is nothing
    /// to hand back — and nothing must be left behind either.
    #[tokio::test]
    async fn a_type_refused_transaction_never_froze_anything() {
        let f = fixture();
        let (raw, hash) = signed_raw(&f.signer, 0);
        let (resp, _rx) = oneshot::channel();

        // Same bytes, but presented as a type the path does not take.
        assert!(!accepts(TxType::Eip7702));
        f.admission.admit(&raw, std::time::Instant::now(), resp).await.expect_err("refused");

        assert_eq!(f.classifier.verdict(&hash), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is what bars establishing a delegation. Using an account that
    /// already has one is an ordinary transaction of an ordinary type, so it
    /// is not on this list's business at all.
    #[test]
    fn only_the_three_ordinary_types_are_accepted() {
        assert!(accepts(TxType::Legacy));
        assert!(accepts(TxType::Eip2930));
        assert!(accepts(TxType::Eip1559));
        assert!(!accepts(TxType::Eip4844));
        assert!(!accepts(TxType::Eip7702));
    }

    /// The three ordinary types cross over; the two system types have no
    /// counterpart and must not be smuggled through as one.
    #[test]
    fn the_conversion_passes_user_types_and_drops_system_ones() {
        use alloy_consensus::{Signed, TxEip1559, TxLegacy};
        use alloy_primitives::{B256, Sealed, Signature};

        let sig = Signature::test_signature();
        let eip1559 = OpTxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559::default(),
            sig,
            B256::repeat_byte(1),
        ));
        assert!(matches!(op_envelope_to_alloy(eip1559), Some(TxEnvelope::Eip1559(_))));

        let legacy = OpTxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy::default(),
            sig,
            B256::repeat_byte(2),
        ));
        assert!(matches!(op_envelope_to_alloy(legacy), Some(TxEnvelope::Legacy(_))));

        let deposit = OpTxEnvelope::Deposit(Sealed::new_unchecked(
            op_alloy_consensus::TxDeposit {
                from: Address::repeat_byte(1),
                to: TxKind::Call(Address::repeat_byte(2)),
                gas_limit: 21_000,
                ..Default::default()
            },
            B256::repeat_byte(99),
        ));
        assert!(op_envelope_to_alloy(deposit).is_none());
    }

    /// The ceilings come from the pool's own configuration; restating them
    /// would let the two drift apart the first time an operator tuned one.
    #[test]
    fn the_ceilings_are_the_pools_own() {
        let pool = reth_transaction_pool::PoolConfig::default();
        let preconf = PreconfConfig::default();
        let cap = capacity_from_pool_config(&pool, &preconf);

        assert_eq!(cap.max_txs, pool.pending_limit.max_txs);
        assert_eq!(cap.max_size, pool.pending_limit.max_size);
        assert_eq!(cap.max_account_slots, pool.max_account_slots);
        assert_eq!(cap.max_inflight_delegated, pool.max_inflight_delegated_slot_limit);
        // The one ceiling the pool has no opinion on: it is the preconf
        // per-block budget times the backlog we will hold, so retuning the
        // budget moves it without anyone remembering to.
        assert_eq!(
            cap.max_queued_gas,
            preconf.preconf_max_gas_per_block * preconf.preconf_queue_gas_blocks,
        );
    }
}

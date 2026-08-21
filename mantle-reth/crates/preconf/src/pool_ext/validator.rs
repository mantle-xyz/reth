//! Pool-validator decoration that enforces preconf-specific rules **before**
//! the wrapped (Mantle / OP / Eth) validator runs.
//!
//! Two checks are added on top of whatever inner validator is wrapped:
//!
//! 1. **Replacement guard**: a tx whose `(sender, nonce)` is already held by a *different*
//!    in-flight preconf tx is rejected with [`ReplaceActivePreconf`]. Occupancy is the classifier's
//!    slot index, claimed synchronously at admission so it covers the window before the pool
//!    listener creates a fifo entry. Only [`PreconfStatus::is_replaceable`] holders release it, and
//!    the reclaimed holder is torn down only once the replacement is actually admitted.
//!
//! Occupancy is the index alone, never the fifo: a commitment inside its
//! retention window owns its nonce with no fifo entry at all. See the holder
//! read in `guarded_validate`.
//!
//! 2. **Per-tx gas ceiling (operator hardening)**: preconf-eligible txs whose `gas_limit` exceeds
//!    `cfg.preconf_max_gas_per_tx` are rejected. Non-preconf txs pass through unaffected.
//!
//! Both gates run before the inner validator. A rejection releases the verdict
//! frozen on the way in only when *this* call froze it (`Admission::Fresh`) —
//! see `validate_transaction`. The replacement guard also has two late
//! rejections, after the inner validator, where the slot handover CAS is lost.
//!
//! A transaction whose receipt already went out is exempt from **both**, and
//! from anything added alongside them: it returns straight to the inner
//! validator before any preconf gate runs. The predicate is
//! [`PreconfClassifier::is_promised`], not the [`Verdict::Promised`] variant —
//! a reorg-reinjected commitment keeps `Verdict::Eligible` and must be exempt
//! too. Stated once, at the top of `guarded_validate`, rather than per gate.
//!
//! All other concerns (signature, balance, basefee, EIP-155, `MetaTx`, ...)
//! are delegated to the inner validator unchanged.
//!
//! # Cross-client note
//!
//! op-geth performs the same two-tier check inside `LegacyPool.add`
//! (`core/txpool/legacypool/legacypool.go`, "only timeout preconf tx can be
//! replaced"), using the pool's own pending list for ownership. Two deliberate
//! differences:
//!
//! * **Where ownership is recorded.** geth registers the preconf tx synchronously inside `add`,
//!   under the pool lock, so pool membership is an exact answer. reth decorates the *validator*,
//!   which runs before the pool takes its write lock, so neither the pool nor the fifo can answer
//!   without a window — hence the classifier-side slot index.
//! * **Which states release the slot.** geth accepts only `Timeout`; reth also accepts `Canceled`
//!   and `Failed`. That is not a laxer policy: geth's single `failed` status conflates "reverted,
//!   on chain" with "never executed", so it cannot safely release the slot for either, while reth's
//!   fifo-layer states are all provably not-on-chain (see [`crate::types::PreconfStatus`]).

use std::{any::Any, sync::Arc};

use alloy_consensus::Transaction;
use reth_optimism_txpool::OpPooledTx;
use reth_primitives_traits::SealedBlock;
use reth_transaction_pool::{
    EthPoolTransaction, PoolTransaction, TransactionOrigin, TransactionValidationOutcome,
    TransactionValidator, error::InvalidPoolTransactionError,
};
use tracing::warn;

use crate::{
    classifier::{Admission, PreconfClassifier, SlotClaim, Verdict},
    config::PreconfConfig,
    preconf_tx_set::PreconfTxSet,
    types::PreconfStatus,
};

/// Replacement attempt blocked because an active preconf commitment already
/// occupies `(sender, nonce)`.
#[derive(thiserror::Error, Debug)]
#[error("cannot replace active preconf commitment for the same (sender, nonce)")]
pub struct ReplaceActivePreconf;

impl reth_transaction_pool::error::PoolTransactionError for ReplaceActivePreconf {
    fn is_bad_transaction(&self) -> bool {
        // `is_bad_transaction == true` triggers, in reth's network layer:
        //   1. P2P reputation hit on the announcing peer (`ReputationChangeKind::BadTransactions`,
        //      weight ≈ -16384; ~4 hits and the peer is below the ban threshold)
        //   2. The tx hash is added to the `bad_imports` cache, so future announcements of the same
        //      hash from any peer are rejected without re-running validation
        //   3. (Skipped while the node is syncing)
        //
        // A replacement collision is not the sender's fault — they have no
        // way to know that another tx with the same (sender, nonce) is in
        // flight. Returning false here keeps both the peer and the hash
        // unblocked so the legitimate retry path (e.g., the same-hash
        // re-submit after `Timeout`) still works.
        //
        // Same treatment as op-geth's `ErrPreconfInProcess`: log it, do
        // not punish.
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A preconf-eligible tx's `gas_limit` exceeded the configured per-tx ceiling.
#[derive(thiserror::Error, Debug)]
#[error("preconf-eligible tx gas limit exceeds `preconf_max_gas_per_tx`")]
pub struct PreconfGasLimitExceeded;

impl reth_transaction_pool::error::PoolTransactionError for PreconfGasLimitExceeded {
    fn is_bad_transaction(&self) -> bool {
        // The preconf feature is opened only to internal trusted clients —
        // no public peer ever submits via this path. Returning true would
        // cause reth to apply a P2P `BadTransactions` reputation hit and
        // cache the tx hash in `bad_imports`, both of which would punish
        // our own internal RPC infrastructure on any misconfiguration.
        //
        // We therefore treat `preconf_max_gas_per_tx` the same way reth
        // treats its own `MaxTxGasLimitExceeded`: a node-local policy
        // bound that rejects the tx without penalizing the announcing
        // peer. The validation outcome still rejects the tx; only the
        // reputation side-effect is suppressed.
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Records `preconf.validate.duration_ms` on drop so all early-return paths
/// of [`PreconfAwareValidator::validate_transaction`] are covered. Spans the
/// inner (OP / Mantle) validator too — op-geth `preconf/txpool/filter` analogue.
struct ValidateTimer(std::time::Instant);

impl Drop for ValidateTimer {
    fn drop(&mut self) {
        metrics::histogram!("preconf.validate.duration_ms")
            .record(self.0.elapsed().as_millis() as f64);
    }
}

/// Validator decorator that enforces preconf-specific rules before delegating.
///
/// Constructed via [`Self::new`] and threaded into the pool validation chain.
/// Typically the inner validator is `MantleTransactionValidator<OpTransactionValidator<...>>`.
///
/// Cheap to clone — `Arc`-shares the config, the classifier and the fifo handle.
#[derive(Debug, Clone)]
pub struct PreconfAwareValidator<V> {
    inner: V,
    cfg: Arc<PreconfConfig>,
    classifier: Arc<PreconfClassifier>,
    fifo: Arc<PreconfTxSet>,
}

impl<V> PreconfAwareValidator<V> {
    /// Wrap an inner validator with preconf checks.
    pub const fn new(
        inner: V,
        cfg: Arc<PreconfConfig>,
        classifier: Arc<PreconfClassifier>,
        fifo: Arc<PreconfTxSet>,
    ) -> Self {
        Self { inner, cfg, classifier, fifo }
    }

    /// Borrow the wrapped validator.
    pub const fn inner(&self) -> &V {
        &self.inner
    }
}

impl<V> TransactionValidator for PreconfAwareValidator<V>
where
    V: TransactionValidator,
    V::Transaction: EthPoolTransaction + OpPooledTx,
{
    type Transaction = V::Transaction;
    type Block = V::Block;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        let _timer = ValidateTimer(std::time::Instant::now());

        let sender = transaction.sender();
        let nonce = transaction.nonce();
        let tx_hash = *transaction.hash();
        // **The latching point, not the deciding one.** Every other component
        // reads the verdict frozen here, and nobody re-derives it — but this
        // call does not derive it either. It writes `NotEligible` when the hash
        // has no verdict yet, and otherwise returns whatever the RPC boundary
        // (`claim_preconf`) or journal restore (`mark_promised`) already wrote.
        //
        // The recipient is deliberately not a parameter: eligibility is "did the
        // client ask for a preconfirmation", which this call cannot observe —
        // both `eth_` submission methods arrive as `TransactionOrigin::External`,
        // and p2p, reorg reinject and journal restore arrive with no RPC at all.
        // Handing it a `to` would invite exactly the live allowlist read the
        // classifier exists to prevent.
        //
        // The same call claims the `(sender, nonce)` slot — see
        // `PreconfClassifier::admit_and_claim` for why the claim has to
        // happen here and not against the fifo.
        let (verdict, slot_claim, admission) =
            self.classifier.admit_and_claim(tx_hash, &sender, nonce);

        let outcome = self
            .guarded_validate(origin, transaction, tx_hash, &sender, nonce, verdict, slot_claim)
            .await;

        // A transaction that did not make it into the pool must not keep a
        // frozen verdict, and above all must not keep its slot claim: the
        // verdict's timestamp means "the moment this entered the pool", and a
        // stranded claim would block that nonce until the next sweep.
        //
        // **Only release what this call created.** The obvious rule — "not
        // admitted ⇒ drop the record" — is wrong, because this is not
        // necessarily the admission that created it. `add_transaction` awaits
        // `validate` unconditionally, and the hash-dedup that answers
        // `AlreadyImported` sits *behind* it: that check lives in
        // `TxPool::add_transaction` and takes a `ValidPoolTransaction`, so a
        // failing re-validation never consults it. An ordinary
        // `eth_sendRawTransaction` resubmit applies no hash dedup of its own,
        // and the pool listener gives a fifo entry to every preconf-eligible
        // transaction whatever RPC admitted it, so a wallet retry or a
        // load-balancer replay re-runs this whole path against a record
        // belonging to an **earlier, successful** admission. (A p2p
        // re-announcement does *not* reach here — `retain_unknown` drops
        // already-pooled hashes before validation.) Such a re-run can fail for
        // reasons that say nothing about that record: `NonceNotConsistent` once
        // the transaction has landed, or `InsufficientFunds`, which on Mantle
        // flips with no act of the sender.
        //
        // Releasing there would either strand a **live fifo entry** without its
        // `(sender, nonce)`, or hand back the nonce of a commitment that is **on
        // chain inside its retention window** — exactly what that window exists
        // to prevent. `Admission::Fresh` is the precise condition: this call
        // inserted the record, so nothing else can be relying on it.
        //
        // It does not subsume the promise exemption. `mark_promised` is a
        // get-or-insert and can run on another task during the `await` above, so
        // a `Fresh` admission can come back to find its own hash promised. That
        // exemption lives in `release_preconf_claim`, which the RPC handler's
        // own failure path calls too. Do not re-derive it here.
        //
        // Pinned by `a_landed_commitment_survives_a_failed_rebroadcast` and
        // `a_repooled_tx_that_fails_revalidation_keeps_its_slot`.
        if !matches!(outcome, TransactionValidationOutcome::Valid { .. }) &&
            admission == Admission::Fresh
        {
            self.classifier.release_preconf_claim(&tx_hash);
        }

        outcome
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
    }
}

impl<V> PreconfAwareValidator<V>
where
    V: TransactionValidator,
    V::Transaction: EthPoolTransaction + OpPooledTx,
{
    /// The preconf gates, split out so `validate_transaction` has a single exit
    /// through which every rejection can release the verdict it just froze.
    #[expect(clippy::too_many_arguments, reason = "all of it is already computed by the caller")]
    async fn guarded_validate(
        &self,
        origin: TransactionOrigin,
        transaction: V::Transaction,
        tx_hash: alloy_primitives::TxHash,
        sender: &alloy_primitives::Address,
        nonce: u64,
        verdict: Verdict,
        slot_claim: SlotClaim,
    ) -> TransactionValidationOutcome<V::Transaction> {
        // Replacement guard. Two questions, answered by two different sources.
        //
        // **Does anything already hold this `(sender, nonce)`?** — the
        // classifier's slot index; see the read below for why it, and not the
        // fifo, is the right source.
        //
        // **May the holder be replaced?** — the fifo, which owns the state
        // machine. Only reclaimable terminal states release it: `Timeout`
        // (client deadline), `Canceled` (block-gas-budget pre-apply reject) and `Failed` (reth
        // builder pre-execute reject). All three mean **tx NOT on chain**; see
        // `crate::types::PreconfStatus`, which also explains why the fifo-layer
        // `Failed` differs from the wire-layer one. `Waiting` and `Success`
        // block replacement (`Success` is on chain or in flight, so replacing
        // would double-apply). A holder with no fifo entry at all is likewise
        // not replaceable — it is mid-window, not terminal. (The three-state set
        // diverges from op-geth; see the module's cross-client note.)
        //
        // A commitment whose receipt already went out skips **every** preconf
        // gate in this function, so it leaves here rather than being exempted at
        // each one — a gate added below cannot then quietly forget it. Any
        // preconf-layer gate that rejected it would, by construction, be
        // breaking an already-published commitment. The predicate is the
        // `promised` flag, not the `Verdict::Promised` variant — see
        // `CachedVerdict` for why those are different questions.
        //
        // The slot claim is *not* skipped, because it has already happened:
        // journal restore's pre-pass calls `mark_promised`, which claims the
        // `(sender, nonce)` in the same breath as it records the promise. Should
        // `replace_slot` below ever be hoisted out of the reclaim branch, this
        // early return has to be revisited — a restored record's verdict is
        // preconf, so a hoisted call would cover it.
        if self.classifier.is_promised(&tx_hash) {
            return self.inner.validate_transaction(origin, transaction).await;
        }

        // Occupancy is the slot index, and only the slot index. Every route to a
        // fifo entry claims the slot first: admission here claims it, and
        // journal restore's pre-pass claims it (its loser being refused an entry
        // by `push_if_absent`). Pinned by
        // `the_violating_state_cannot_be_constructed` here and
        // `journal::tests::restore_never_leaves_a_fifo_entry_without_its_slot`.
        //
        // Asking the fifo as well would read the *wrong* thing: a commitment
        // inside its retention window owns its nonce with no fifo entry at all
        // (`forward()` removed it), so the two sources disagree by design for a
        // whole `SEAL_DEPTH` window. The fifo is consulted only for the named
        // owner's status — a `None` there is handled below.
        let holder: Option<(alloy_primitives::TxHash, Option<PreconfStatus>)> = match slot_claim {
            Err(owner) => Some((owner, self.fifo.find_by_hash(&owner).await.map(|e| e.status))),
            Ok(()) => None,
        };

        // Reclaimable holders are torn down only once the replacement is
        // actually admitted, so the decision has to outlive the gas gate below.
        let mut reclaim: Option<alloy_primitives::TxHash> = None;
        if let Some((owner, status)) = holder {
            // `None` is the only non-replaceable holder here: the slot is held by
            // a hash with no fifo entry — the microsecond gap before the
            // listener's push, or a commitment inside its retention window.
            //
            // Every terminal state is replaceable, `Failed` among them — which
            // covers a commitment whose receipt already went out and which the
            // builder then could not apply (dispatch's `ApplyError::Rejected`
            // arm). Handing its nonce away is the intended outcome: only this
            // sender can produce a transaction on that nonce, so holding it
            // would wedge the sender's own account with nothing able to clear
            // it. The breach is recorded there
            // (`preconf.tx.commitment_broken_total`), not prevented here.
            //
            // The terminal set lives in `is_replaceable` — do not re-derive it.
            let replaceable = status.is_some_and(PreconfStatus::is_replaceable);
            if !replaceable {
                return TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidPoolTransactionError::Other(Box::new(ReplaceActivePreconf)),
                );
            }
            // Reclaimable — but do not tear the holder down yet. See below.
            reclaim = Some(owner);
        }

        // Per-tx gas ceiling: applies only to preconf-eligible txs.
        // Non-preconf txs are intentionally left to the upstream (reth /
        // OP) validator's own gas-limit checks.
        //
        // **Defence in depth, not the operative check.** The only writer of
        // `Verdict::Eligible` is `claim_preconf`, and the RPC applies this same
        // ceiling off the same `Arc<PreconfConfig>` (`node.rs` wires one
        // instance into both) *before* writing the verdict, so nothing can reach
        // here `Eligible` and over the cap. Kept as the enforcement point for
        // any future writer of an eligible verdict; the tests below drive it
        // directly rather than through a reachable production scenario.
        if verdict.is_preconf() && transaction.gas_limit() > self.cfg.preconf_max_gas_per_tx {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::Other(Box::new(PreconfGasLimitExceeded)),
            );
        }

        let outcome = self.inner.validate_transaction(origin, transaction).await;

        // Only now, with the replacement actually about to be admitted, tear the
        // reclaimable holder down. Doing it before the gates above would destroy
        // it even when *our* transaction is then rejected — leaving the sender
        // with neither: the holder's fifo entry gone (so its same-hash retry can
        // no longer revive it) and the replacement not admitted either.
        //
        // The handover is a **compare-and-swap**, and it comes first. "The holder
        // is reclaimable, so I may take its nonce" was decided above the `await`
        // on the inner validator, so two same-nonce transactions can both have
        // reached that conclusion about the same holder. Whoever wins the CAS is
        // the only one that tears the holder down and owns the nonce.
        //
        // The loser is refused, not admitted. `Valid` is not admission: the pool
        // inserts under its own lock afterwards and applies its own price-bump
        // rule, so returning `Valid` here would leave the index, the pool and the
        // fifo each picking a winner by a different rule (validation-completion
        // order, price, event order), and the transaction the *pool* accepted
        // could then be skipped by both build arms. First-come-first-served is
        // the intended semantics, matching what the guard does sequentially: a
        // later same-nonce transaction is refused however much it pays.
        if matches!(outcome, TransactionValidationOutcome::Valid { .. }) &&
            let Some(owner) = reclaim
        {
            if self.classifier.replace_slot(&owner, sender, nonce, tx_hash).is_err() {
                return TransactionValidationOutcome::Invalid(
                    // `Valid` is the only variant reachable here, and it carries
                    // the transaction we handed to the inner validator.
                    Self::recover_transaction(outcome),
                    InvalidPoolTransactionError::Other(Box::new(ReplaceActivePreconf)),
                );
            }
            // Tear the holder down — and if that is refused, **undo the
            // handover**.
            //
            // `remove_reclaimable` re-checks the holder's status and its
            // `apply_lock` under the fifo lock, so it declines to delete an
            // entry that a same-hash resubmit revived to `Waiting`, or that went
            // mid-apply, since the guard read it above. Reachable: the resubmit
            // passes `admit_and_claim` because the slot was still the holder's
            // at that moment (a same-hash claim is idempotent), and the pool
            // listener then revives the entry.
            //
            // Proceeding anyway would leave the holder with a live `Waiting`
            // entry and — after the release below — no verdict record. The pool
            // arm's skip predicate reads exactly that record, so it would treat
            // a live commitment as an ordinary pool transaction while the
            // preconf arm still holds it in the fifo. Deleting the entry instead
            // would destroy a commitment a client is right now waiting on.
            //
            // So neither: give the nonce back and refuse, which is what the
            // guard above would have done had it read the status a moment later.
            // The undo must precede the release, since `replace_slot` reads the
            // holder's verdict to decide the claim is a preconf one.
            if !self.fifo.remove_reclaimable(&owner).await {
                let undone = self.classifier.replace_slot(&tx_hash, sender, nonce, owner);
                warn!(
                    target: "mantle::preconf::validator",
                    ?owner, replacement = ?tx_hash, ?undone,
                    "holder was revived inside our validation; returning its nonce \
                     and refusing the replacement"
                );
                return TransactionValidationOutcome::Invalid(
                    Self::recover_transaction(outcome),
                    InvalidPoolTransactionError::Other(Box::new(ReplaceActivePreconf)),
                );
            }
            self.classifier.release_unless_committed(&owner);
        }

        outcome
    }

    /// Takes the transaction back out of a `Valid` outcome so a later gate can
    /// still reject it.
    ///
    /// Only called on the CAS-lost path, where the outcome is known to be
    /// `Valid`; the other variants already own their transaction (or, for
    /// `Error`, only a hash) and cannot be re-wrapped, so they are unreachable
    /// by construction rather than by assumption.
    fn recover_transaction(
        outcome: TransactionValidationOutcome<V::Transaction>,
    ) -> V::Transaction {
        match outcome {
            TransactionValidationOutcome::Valid { transaction, .. } => {
                transaction.into_transaction()
            }
            TransactionValidationOutcome::Invalid(transaction, _) => transaction,
            TransactionValidationOutcome::Error(..) => {
                unreachable!("only reached from the `Valid` branch above")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // `fixture` mutates `PreconfConfig::default()` to set the two fields it
    // cares about; struct-literal init of that type would be noise. Same
    // precedent as `classifier::tests`.
    #![allow(clippy::field_reassign_with_default)]

    use super::*;
    use reth_transaction_pool::error::PoolTransactionError;

    // `is_bad_transaction` returns reth's "should the sender be penalized"
    // signal — NOT a generic "is the tx invalid" predicate. Tests below
    // assert the chosen reputation policy for each error variant.

    #[test]
    fn replace_active_preconf_does_not_penalize_sender() {
        // Sender has no way to know another tx with the same
        // (sender, nonce) is in flight — don't penalize.
        let err = ReplaceActivePreconf;
        assert!(!err.is_bad_transaction());
        assert!(err.as_any().is::<ReplaceActivePreconf>());
    }

    #[test]
    fn preconf_gas_limit_exceeded_does_not_penalize_sender() {
        // Preconf is gated to internal trusted clients only — penalizing
        // the announcing peer would punish our own RPC infrastructure on
        // misconfiguration. The tx is still rejected; reputation is not
        // touched.
        let err = PreconfGasLimitExceeded;
        assert!(!err.is_bad_transaction());
        assert!(err.as_any().is::<PreconfGasLimitExceeded>());
    }

    // ---------------------------------------------------------------------
    // `validate_transaction` scaffolding.
    //
    // The replacement guard has three branches — index hit, reclaimable holder,
    // promise exemption. The integration suite exercises it through a real pool,
    // but some of those are unreachable from there, so it is pinned here too.
    //
    // Everything below needs three things: a concrete `OpPooledTransaction`, an
    // inner validator whose outcome the test scripts, and a `PreconfTxSet` in a
    // chosen state.
    // ---------------------------------------------------------------------

    use crate::{classifier::DEFAULT_VERDICT_CACHE_CAP, types::PreconfSource};
    use alloy_consensus::{Signed, TxEip1559, TxEnvelope};
    use alloy_primitives::{Address, Signature, TxHash, TxKind, U256, map::foldhash::HashSet};
    use op_alloy_consensus::OpTxEnvelope;
    use reth_optimism_primitives::OpBlock;
    use reth_optimism_txpool::OpPooledTransaction;
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::validate::ValidTransaction;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    /// What the stubbed inner validator returns once the preconf gates let a
    /// transaction reach it.
    #[derive(Debug, Clone, Copy)]
    enum Inner {
        Valid,
        /// A plain rejection.
        Invalid,
        /// The third outcome variant, which is *not* an `Invalid` — the
        /// verdict-release path has to treat it the same way, and it is the easy
        /// one to miss when matching on the outcome.
        Error,
    }

    /// Inner validator stub that records how many times it was delegated to.
    ///
    /// The call count is load-bearing: "the guard refused *before* delegating"
    /// is not observable from the outcome alone.
    ///
    /// This is the one instant at which a concurrent same-nonce transaction can
    /// disturb us, so both injections happen here rather than from a second
    /// task: `steal` takes the slot, `revive` flips the holder's fifo entry back
    /// to `Waiting`.
    ///
    /// `after_first` scripts the **second** delegation onward, which lets a test
    /// drive a real admission through `validate_transaction` and then fail the
    /// re-validation of that same transaction. `None` keeps `outcome` throughout.
    #[derive(Debug)]
    struct StubInner {
        calls: Arc<AtomicUsize>,
        outcome: Inner,
        after_first: Option<Inner>,
        steal: Option<Steal>,
        revive: Option<Revive>,
    }

    /// A concurrent same-hash resubmit that revives the holder's fifo entry from
    /// a reclaimable state back to `Waiting`.
    #[derive(Debug)]
    struct Revive {
        fifo: Arc<PreconfTxSet>,
        tx: Arc<TxEnvelope>,
        from: Address,
    }

    /// A concurrent same-nonce admission. `holder` is the slot's owner both
    /// racers observed.
    #[derive(Debug)]
    struct Steal {
        classifier: Arc<PreconfClassifier>,
        thief: TxHash,
        holder: TxHash,
    }

    impl TransactionValidator for StubInner {
        type Transaction = OpPooledTransaction;
        type Block = OpBlock;

        async fn validate_transaction(
            &self,
            _origin: TransactionOrigin,
            transaction: Self::Transaction,
        ) -> TransactionValidationOutcome<Self::Transaction> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let scripted =
                if call == 0 { self.outcome } else { self.after_first.unwrap_or(self.outcome) };
            if let Some(steal) = &self.steal {
                // The thief walks the same path we did — classified while the
                // holder still owned the nonce, found it reclaimable, now wins
                // the CAS. Only a preconf submission can take a slot, so it goes
                // through the RPC boundary first, exactly as our caller did.
                let (sender, nonce) = (transaction.sender(), transaction.nonce());
                let _ = steal.classifier.claim_preconf(steal.thief, &sender, Some(&recipient()));
                let _ = steal.classifier.admit_and_claim(steal.thief, &sender, nonce);
                assert_eq!(
                    steal.classifier.replace_slot(&steal.holder, &sender, nonce, steal.thief),
                    Ok(()),
                    "the thief must win the race for this test to mean anything",
                );
            }
            if let Some(revive) = &self.revive {
                // `push_if_absent` revives a reclaimable entry back to
                // `Waiting`; the same-hash claim the resubmit made on the way in
                // is idempotent, so the holder keeps the slot it already owns.
                revive
                    .fifo
                    .push_if_absent(revive.tx.clone(), revive.from, PreconfSource::Rpc)
                    .await;
            }
            match scripted {
                Inner::Valid => TransactionValidationOutcome::Valid {
                    balance: U256::MAX,
                    state_nonce: 0,
                    bytecode_hash: None,
                    transaction: ValidTransaction::Valid(transaction),
                    propagate: true,
                    authorities: None,
                },
                Inner::Invalid => TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidPoolTransactionError::Underpriced,
                ),
                Inner::Error => TransactionValidationOutcome::Error(
                    *transaction.hash(),
                    "stub inner validator failed".into(),
                ),
            }
        }
    }

    /// The allowlisted sender.
    fn sender() -> Address {
        Address::from([1u8; 20])
    }

    /// The allowlisted recipient. Eligibility needs both sides (see
    /// `PreconfClassifier::evaluate_whitelist`).
    fn recipient() -> Address {
        Address::from([2u8; 20])
    }

    fn h(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    /// A set holding one exact `(from, to)` rule.
    fn pair_set(entries: &[(Address, Address)]) -> HashSet<(Address, Address)> {
        entries.iter().copied().collect()
    }

    /// A pooled OP transaction with a caller-chosen hash, so a fifo entry built
    /// by [`fifo_tx`] can be made to denote the same transaction.
    ///
    /// The signature is a fixed dummy and the sender is attached out of band —
    /// nothing here re-runs ec-recover.
    fn op_tx(hash_byte: u8, from: Address, nonce: u64, gas_limit: u64) -> OpPooledTransaction {
        let inner =
            TxEip1559 { nonce, gas_limit, to: TxKind::Call(recipient()), ..Default::default() };
        let signed = Signed::new_unchecked(inner, Signature::test_signature(), h(hash_byte));
        OpPooledTransaction::new(Recovered::new_unchecked(OpTxEnvelope::Eip1559(signed), from), 200)
    }

    /// The fifo's view of the same transaction (it holds alloy envelopes).
    fn fifo_tx(hash_byte: u8, nonce: u64) -> Arc<TxEnvelope> {
        let inner = TxEip1559 { nonce, to: TxKind::Call(recipient()), ..Default::default() };
        let signed = Signed::new_unchecked(inner, Signature::test_signature(), h(hash_byte));
        Arc::new(TxEnvelope::Eip1559(signed))
    }

    struct Fixture {
        validator: PreconfAwareValidator<StubInner>,
        classifier: Arc<PreconfClassifier>,
        fifo: Arc<PreconfTxSet>,
        calls: Arc<AtomicUsize>,
    }

    impl Fixture {
        /// Validate `tx` as an **ordinary** submission — plain
        /// `eth_sendRawTransaction`, p2p, or the pool's own reorg reinject.
        ///
        /// Nothing claims a preconf verdict first, so the validator latches
        /// `NotEligible` unless some earlier call already recorded otherwise.
        async fn validate(
            &self,
            tx: OpPooledTransaction,
        ) -> TransactionValidationOutcome<OpPooledTransaction> {
            self.validator.validate_transaction(TransactionOrigin::Local, tx).await
        }

        /// Validate `tx` the way a **preconf RPC** submission arrives: the
        /// verdict is claimed at the RPC boundary first, exactly as `rpc.rs`
        /// does before `pool.add_transaction`.
        ///
        /// Two steps, because that is the production shape and the order is the
        /// point — `validate` alone can never produce an eligible transaction.
        async fn validate_preconf(
            &self,
            tx: OpPooledTransaction,
        ) -> TransactionValidationOutcome<OpPooledTransaction> {
            let hash = *tx.hash();
            let _ = self.classifier.claim_preconf(hash, &tx.sender(), Some(&recipient()));
            let outcome = self.validate(tx).await;
            // What `rpc.rs` does on any pool refusal, modelled here or the
            // fixture would leave records production cleans up: the verdict now
            // belongs to the RPC boundary, so the validator's own release branch
            // sees `Admission::Existing` and leaves it alone. Asserting the real
            // call site is `rpc::tests`' job.
            if !matches!(outcome, TransactionValidationOutcome::Valid { .. }) {
                self.classifier.release_preconf_claim(&hash);
            }
            outcome
        }

        fn inner_calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// Install an incumbent's frozen verdict and slot claim directly, which
        /// is what admitting it through the validator would have done. Used
        /// where the test needs the incumbent in place *before* driving the
        /// transaction under test through the (single-outcome) stub.
        #[track_caller]
        fn seat_incumbent(&self, hash_byte: u8, nonce: u64) {
            assert_eq!(
                self.classifier.claim_preconf(h(hash_byte), &sender(), Some(&recipient())),
                Ok(()),
                "the incumbent arrived through the preconf RPC",
            );
            let (verdict, claim, _) =
                self.classifier.admit_and_claim(h(hash_byte), &sender(), nonce);
            assert_eq!(verdict, Verdict::Eligible);
            assert_eq!(claim, Ok(()), "the incumbent must actually own the slot");
        }
    }

    /// `sender() → recipient()` is the only eligible pair; `max_gas` is the
    /// per-tx ceiling. The grace period is long enough that nothing is ever
    /// sweepable mid-test.
    fn fixture(outcome: Inner, max_gas: u64) -> Fixture {
        build_fixture(outcome, None, max_gas, |_| None, |_| None)
    }

    /// As [`fixture`], but the **first** admission succeeds and every delegation
    /// after it returns `later`, so a test can produce its own precondition
    /// through `validate_transaction` rather than installing it by hand.
    fn fixture_admitting_then(later: Inner, max_gas: u64) -> Fixture {
        build_fixture(Inner::Valid, Some(later), max_gas, |_| None, |_| None)
    }

    /// As [`fixture`], but a concurrent **same-hash** resubmit revives the slot
    /// holder's fifo entry to `Waiting` inside the inner validator.
    fn fixture_with_revived_holder(max_gas: u64, holder: u8, nonce: u64) -> Fixture {
        build_fixture(
            Inner::Valid,
            None,
            max_gas,
            |_| None,
            move |fifo| Some(Revive { fifo, tx: fifo_tx(holder, nonce), from: sender() }),
        )
    }

    /// As [`fixture`], but a concurrent same-nonce transaction `thief` takes the
    /// slot from `holder` inside the inner validator.
    fn fixture_with_thief(max_gas: u64, thief: u8, holder: u8) -> Fixture {
        build_fixture(
            Inner::Valid,
            None,
            max_gas,
            |classifier| Some(Steal { classifier, thief: h(thief), holder: h(holder) }),
            |_| None,
        )
    }

    fn build_fixture(
        outcome: Inner,
        after_first: Option<Inner>,
        max_gas: u64,
        steal: impl FnOnce(Arc<PreconfClassifier>) -> Option<Steal>,
        revive: impl FnOnce(Arc<PreconfTxSet>) -> Option<Revive>,
    ) -> Fixture {
        let mut cfg = PreconfConfig::default();
        cfg.enabled = true;
        cfg.preconf_max_gas_per_tx = max_gas;

        let classifier =
            PreconfClassifier::new(false, Duration::from_secs(3600), DEFAULT_VERDICT_CACHE_CAP);
        classifier.update_whitelist(
            pair_set(&[(sender(), recipient())]),
            HashSet::default(),
            HashSet::default(),
        );

        let classifier = Arc::new(classifier);
        let fifo = Arc::new(PreconfTxSet::new(16));
        let calls = Arc::new(AtomicUsize::new(0));
        let validator = PreconfAwareValidator::new(
            StubInner {
                calls: calls.clone(),
                outcome,
                after_first,
                steal: steal(classifier.clone()),
                revive: revive(fifo.clone()),
            },
            Arc::new(cfg),
            classifier.clone(),
            fifo.clone(),
        );

        Fixture { validator, classifier, fifo, calls }
    }

    #[track_caller]
    fn assert_refused<E: 'static>(outcome: &TransactionValidationOutcome<OpPooledTransaction>) {
        match outcome {
            TransactionValidationOutcome::Invalid(_, InvalidPoolTransactionError::Other(err)) => {
                assert!(err.as_any().is::<E>(), "rejected, but for the wrong reason")
            }
            _ => panic!("expected a preconf-specific rejection"),
        }
    }

    #[track_caller]
    fn assert_admitted(outcome: &TransactionValidationOutcome<OpPooledTransaction>) {
        assert!(
            matches!(outcome, TransactionValidationOutcome::Valid { .. }),
            "expected the transaction to be admitted",
        );
    }

    // --- Branch 1: the slot index -----------------------------------------

    /// The window the index exists for: a transaction that has been classified
    /// and admitted, but whose fifo entry the listener has not created yet.
    /// Asking the fifo alone would report the nonce as free and let the
    /// replacement through — the tx would then be skipped by the pool arm (it is
    /// preconf) and never picked up by the preconf arm (it has no fifo entry),
    /// so the fee bump is silently eaten.
    #[tokio::test]
    async fn a_holder_with_no_fifo_entry_yet_still_owns_its_slot() {
        let f = fixture(Inner::Valid, 1_000_000);

        assert_admitted(&f.validate_preconf(op_tx(1, sender(), 5, 21_000)).await);
        assert_eq!(f.classifier.slot_owner(&sender(), 5), Some(h(1)), "admission claims the slot");
        assert!(
            f.fifo.find_by_sender_nonce(&sender(), 5).await.is_none(),
            "no fifo entry yet — this is the window",
        );

        // Same (sender, nonce), different hash: a higher-priced replacement.
        let replacement = f.validate_preconf(op_tx(2, sender(), 5, 21_000)).await;

        assert_refused::<ReplaceActivePreconf>(&replacement);
        assert_eq!(f.inner_calls(), 1, "the guard must refuse before delegating");
        assert_eq!(f.classifier.slot_owner(&sender(), 5), Some(h(1)), "the holder keeps the slot");
        assert_eq!(f.classifier.verdict(&h(2)), None, "a refused tx keeps no frozen verdict");
    }

    // --- The occupancy index is sufficient on its own ---------------------

    /// **The state the index alone must still catch**: a transaction holding a
    /// fifo entry whose `(sender, nonce)` is owned by a *different* hash.
    /// Reaching it would let both build arms end up holding a transaction for
    /// that nonce.
    ///
    /// This walks the route the retention period opens — an on-chain commitment
    /// keeps its slot but, because `forward()` removed its entry the moment its
    /// nonce advanced, has **no fifo entry** for a whole `SEAL_DEPTH` window —
    /// and shows the index closes it unaided: the claim fails, and a holder with
    /// no fifo entry reads as `None`, which is not replaceable.
    ///
    /// The other route, a promised transaction admitted without a slot, is closed
    /// one layer up — see
    /// `journal::tests::restore_never_leaves_a_fifo_entry_without_its_slot`.
    #[tokio::test]
    async fn the_violating_state_cannot_be_constructed() {
        let f = fixture(Inner::Valid, 1_000_000);

        assert_eq!(f.classifier.mark_promised(h(1), &sender(), 5), Ok(()));
        assert!(f.classifier.mark_committed(&h(1), 100));
        f.classifier.release_unless_committed(&h(1)); // what `forward` fires
        assert_eq!(f.classifier.slot_owner(&sender(), 5), Some(h(1)), "still owns the nonce");
        assert!(!f.fifo.contains(&h(1)).await, "and has no fifo entry");

        assert_refused::<ReplaceActivePreconf>(&f.validate(op_tx(2, sender(), 5, 21_000)).await);
        assert_eq!(f.inner_calls(), 0, "refused before the inner validator");

        // So it never reaches the pool, the listener never pushes it, and no
        // entry without a slot comes into existence.
        assert!(!f.fifo.contains(&h(2)).await);
    }

    /// The guard's occupancy test must not depend on the *newcomer's* verdict:
    /// gating it on `is_preconf()` opens a hole for a sender the allowlist
    /// dropped between the two submissions. Only the incumbent is inspected.
    #[tokio::test]
    async fn a_de_whitelisted_replacement_is_still_refused_the_slot() {
        let f = fixture(Inner::Valid, 1_000_000);

        assert_admitted(&f.validate_preconf(op_tx(1, sender(), 5, 21_000)).await);
        f.fifo.push_if_absent(fifo_tx(1, 5), sender(), PreconfSource::Rpc).await;

        // Governance revokes the rule. The incumbent's frozen verdict is
        // untouched — it is still headed for the preconf arm.
        f.classifier.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());

        let replacement = f.validate_preconf(op_tx(2, sender(), 5, 21_000)).await;

        assert_refused::<ReplaceActivePreconf>(&replacement);
        assert_eq!(f.inner_calls(), 1);
        assert_eq!(f.classifier.slot_owner(&sender(), 5), Some(h(1)));
    }

    // --- Branch 3: a reclaimable holder -----------------------------------

    /// `Timeout` / `Canceled` / `Failed` are all provably not on chain, so the
    /// nonce is free to be reused. Asserted for each of the three: the set
    /// diverges from op-geth (see the module's cross-client note), so it is
    /// pinned here rather than left to the fifo's own tests.
    #[tokio::test]
    async fn a_reclaimable_holder_hands_the_slot_over() {
        for status in [PreconfStatus::Timeout, PreconfStatus::Canceled, PreconfStatus::Failed] {
            let f = fixture(Inner::Valid, 1_000_000);

            f.seat_incumbent(1, 5);
            f.fifo.push_if_absent(fifo_tx(1, 5), sender(), PreconfSource::Rpc).await;
            match status {
                PreconfStatus::Timeout => f.fifo.mark_timeout(&h(1)).await.unwrap(),
                PreconfStatus::Canceled => f.fifo.mark_canceled(&h(1)).await.unwrap(),
                PreconfStatus::Failed => f.fifo.mark_failed(&h(1)).await.unwrap(),
                other => panic!("not a reclaimable status: {other:?}"),
            }

            let replacement = f.validate_preconf(op_tx(2, sender(), 5, 21_000)).await;

            assert_admitted(&replacement);
            assert_eq!(
                f.inner_calls(),
                1,
                "{status:?}: the replacement reaches the inner validator"
            );
            assert!(!f.fifo.contains(&h(1)).await, "{status:?}: the holder's fifo entry is gone");
            assert_eq!(f.classifier.verdict(&h(1)), None, "{status:?}: and so is its verdict");
            assert_eq!(
                f.classifier.slot_owner(&sender(), 5),
                Some(h(2)),
                "{status:?}: the slot is now the replacement's",
            );
        }
    }

    /// `Waiting` blocks replacement (the commitment is live) and so does
    /// `Success` (on chain, or in flight to it — replacing would double-apply).
    #[tokio::test]
    async fn an_active_holder_blocks_replacement() {
        for status in [PreconfStatus::Waiting, PreconfStatus::Success] {
            let f = fixture(Inner::Valid, 1_000_000);

            f.seat_incumbent(1, 5);
            f.fifo.push_if_absent(fifo_tx(1, 5), sender(), PreconfSource::Rpc).await;
            if status == PreconfStatus::Success {
                f.fifo.mark_succeeded(&h(1)).await.unwrap();
            }

            let replacement = f.validate(op_tx(2, sender(), 5, 21_000)).await;

            assert_refused::<ReplaceActivePreconf>(&replacement);
            assert_eq!(f.inner_calls(), 0, "{status:?}");
            assert!(f.fifo.contains(&h(1)).await, "{status:?}: the holder is untouched");
        }
    }

    /// **The guard must let a sender past a broken commitment.** Once the apply
    /// has been rejected the entry is an ordinary `Failed`: its tx is not on
    /// chain and its `(sender, nonce)` is released. Refusing the replacement here
    /// would stall the account on that nonce until the classifier's grace-period
    /// sweep cleared the claim, and would do so to protect a commitment from the
    /// only party able to use that nonce: its promisee.
    #[tokio::test]
    async fn a_broken_commitment_does_not_block_a_same_nonce_tx() {
        let f = fixture(Inner::Valid, 1_000_000);

        f.seat_incumbent(1, 5);
        f.fifo.push_if_absent(fifo_tx(1, 5), sender(), PreconfSource::Replay).await;
        // The first apply rejection is terminal for a replayed commitment.
        f.fifo.mark_failed(&h(1)).await.unwrap();
        let e = f.fifo.find_by_hash(&h(1)).await.unwrap();
        assert_eq!(e.status, PreconfStatus::Failed);
        assert_eq!(e.source, PreconfSource::Replay, "still marked as a breach");

        let replacement = f.validate(op_tx(2, sender(), 5, 21_000)).await;

        assert!(
            matches!(replacement, TransactionValidationOutcome::Valid { .. }),
            "the replacement must be admitted, not refused: {replacement:?}",
        );
        assert_eq!(f.inner_calls(), 1, "and it reached the inner validator");
        assert!(!f.fifo.contains(&h(1)).await, "the broken entry was torn down");
        // A plain (`NotEligible`) replacement does not itself claim the slot —
        // `VerdictStore::claim` only writes `by_slot` for preconf verdicts — so
        // the slot reads as free. That is the point: it is no longer pinned.
        assert_eq!(
            f.classifier.slot_owner(&sender(), 5),
            None,
            "the (sender, nonce) is released, not handed to the replacement",
        );
    }

    /// Tearing the reclaimable holder down before the inner validator has
    /// spoken leaves the sender with neither transaction: the holder's fifo
    /// entry is gone (so its same-hash retry can no longer revive it) and the
    /// replacement was rejected anyway.
    #[tokio::test]
    async fn a_rejected_replacement_leaves_the_reclaimable_holder_intact() {
        let f = fixture(Inner::Invalid, 1_000_000);

        f.seat_incumbent(1, 5);
        f.fifo.push_if_absent(fifo_tx(1, 5), sender(), PreconfSource::Rpc).await;
        f.fifo.mark_timeout(&h(1)).await.unwrap();

        let replacement = f.validate(op_tx(2, sender(), 5, 21_000)).await;

        assert!(matches!(replacement, TransactionValidationOutcome::Invalid(..)));
        assert_eq!(f.inner_calls(), 1, "the guard let it through; the inner validator refused");
        let holder = f.fifo.find_by_hash(&h(1)).await.expect("holder must survive");
        assert_eq!(holder.status, PreconfStatus::Timeout, "including its reclaimable status");
        assert_eq!(f.classifier.verdict(&h(1)), Some(Verdict::Eligible), "and its frozen verdict");
        assert_eq!(f.classifier.slot_owner(&sender(), 5), Some(h(1)), "and its claim");
    }

    /// Two same-nonce transactions can both conclude the same holder is
    /// reclaimable, so the handover is a compare-and-swap and the loser must be
    /// *refused*, not admitted. See the CAS comment in `guarded_validate` for
    /// why admitting both is unsafe.
    #[tokio::test]
    async fn losing_the_handover_race_refuses_the_replacement() {
        // 3 steals the slot from holder 1 while our transaction (2) is inside
        // the inner validator.
        let f = fixture_with_thief(1_000_000, 3, 1);

        f.seat_incumbent(1, 5);
        f.fifo.push_if_absent(fifo_tx(1, 5), sender(), PreconfSource::Rpc).await;
        f.fifo.mark_timeout(&h(1)).await.unwrap();

        let outcome = f.validate_preconf(op_tx(2, sender(), 5, 21_000)).await;

        assert_refused::<ReplaceActivePreconf>(&outcome);
        assert_eq!(f.inner_calls(), 1, "the guard passed it through; the CAS refused it");
        assert_eq!(f.classifier.slot_owner(&sender(), 5), Some(h(3)), "the winner keeps the slot");
        assert_eq!(f.classifier.verdict(&h(2)), None, "the loser keeps no frozen verdict");
    }

    /// The loser must also leave the holder alone. Tearing it down is the
    /// winner's job, and doing it anyway would destroy a fifo entry that the
    /// winner — or the holder's own same-hash retry — still needs.
    #[tokio::test]
    async fn losing_the_handover_race_leaves_the_holder_intact() {
        let f = fixture_with_thief(1_000_000, 3, 1);

        f.seat_incumbent(1, 5);
        f.fifo.push_if_absent(fifo_tx(1, 5), sender(), PreconfSource::Rpc).await;
        f.fifo.mark_timeout(&h(1)).await.unwrap();

        let _ = f.validate_preconf(op_tx(2, sender(), 5, 21_000)).await;

        let holder = f.fifo.find_by_hash(&h(1)).await.expect("holder must survive");
        assert_eq!(holder.status, PreconfStatus::Timeout);
    }

    /// A holder revived inside our validation keeps its nonce, and the
    /// replacement is refused.
    ///
    /// The guard read the holder as reclaimable, then a same-hash resubmit
    /// revived it to `Waiting`. We win the slot CAS — which only asks *who* owns
    /// the nonce, not what the fifo says about it — and then find the teardown
    /// refused, because the entry is live and a client is waiting on it. So the
    /// handover is undone and the replacement refused; see the teardown comment
    /// in `guarded_validate` for why neither alternative is safe.
    #[tokio::test]
    async fn a_holder_revived_mid_validation_keeps_its_nonce_and_refuses_the_replacement() {
        let f = fixture_with_revived_holder(1_000_000, 1, 5);

        f.seat_incumbent(1, 5);
        f.fifo.push_if_absent(fifo_tx(1, 5), sender(), PreconfSource::Rpc).await;
        f.fifo.mark_timeout(&h(1)).await.unwrap();

        let outcome = f.validate_preconf(op_tx(2, sender(), 5, 21_000)).await;

        assert_refused::<ReplaceActivePreconf>(&outcome);
        assert_eq!(
            f.classifier.slot_owner(&sender(), 5),
            Some(h(1)),
            "the handover is undone: the revived holder keeps its nonce",
        );

        let holder = f.fifo.find_by_hash(&h(1)).await.expect("the revived holder is still there");
        assert_eq!(holder.status, PreconfStatus::Waiting, "and its entry is untouched");
        assert!(
            f.classifier.verdict(&h(1)).is_some_and(Verdict::is_preconf),
            "and it keeps the verdict the pool arm reads to stay off it",
        );
        assert_eq!(f.classifier.verdict(&h(2)), None, "the refused replacement leaves nothing");
    }

    // --- Branch 3: the promise exemption ----------------------------------

    /// A commitment restored from the journal was acknowledged to its client
    /// before the restart and must be re-admitted unconditionally. Refusing it
    /// here would turn a kept promise into "commitment cannot be honoured";
    /// which of two same-nonce transactions actually gets applied is decided one
    /// layer down, by `push_if_absent`.
    #[tokio::test]
    async fn promised_is_admitted_even_when_the_slot_is_taken() {
        let f = fixture(Inner::Valid, 1_000_000);

        // An incumbent that nothing may replace: it holds the slot and has no
        // fifo entry, so it is not in a reclaimable state.
        f.seat_incumbent(1, 5);
        // Journal restore records the promise; its claim loses to the incumbent,
        // so it arrives with no slot of its own.
        assert_eq!(f.classifier.mark_promised(h(2), &sender(), 5), Err(h(1)));

        assert_admitted(&f.validate(op_tx(2, sender(), 5, 21_000)).await);
        assert_eq!(f.inner_calls(), 1);
        assert_eq!(f.classifier.verdict(&h(2)), Some(Verdict::Promised), "still promised");
        assert_eq!(
            f.classifier.slot_owner(&sender(), 5),
            Some(h(1)),
            "the exemption bypasses the guard; it does not seize the slot",
        );
    }

    /// The receipt went out before the restart, so a restored commitment must
    /// not be re-judged against a ceiling the operator has since lowered.
    /// Without this, lowering `--preconf.max-gas-per-tx` and restarting would
    /// silently drop an already-acknowledged commitment.
    #[tokio::test]
    async fn promised_bypasses_the_per_tx_gas_ceiling() {
        let f = fixture(Inner::Valid, 21_000);

        assert_eq!(f.classifier.mark_promised(h(1), &sender(), 5), Ok(()));

        assert_admitted(&f.validate(op_tx(1, sender(), 5, 500_000)).await);
        assert_eq!(f.inner_calls(), 1);
    }

    // --- The per-tx gas ceiling ------------------------------------------

    #[tokio::test]
    async fn an_eligible_tx_over_the_gas_ceiling_is_refused() {
        let f = fixture(Inner::Valid, 21_000);

        let outcome = f.validate_preconf(op_tx(1, sender(), 5, 500_000)).await;

        assert_refused::<PreconfGasLimitExceeded>(&outcome);
        assert_eq!(f.inner_calls(), 0);
        assert_eq!(f.classifier.verdict(&h(1)), None, "the frozen verdict is released");
        assert_eq!(f.classifier.slot_owner(&sender(), 5), None, "and so is the slot claim");
    }

    /// The ceiling is a preconf policy knob. Ordinary traffic is left to the
    /// upstream validator's own gas-limit checks.
    #[tokio::test]
    async fn a_non_preconf_tx_is_not_subject_to_the_gas_ceiling() {
        let f = fixture(Inner::Valid, 21_000);
        let stranger = Address::from([9u8; 20]);

        assert_admitted(&f.validate(op_tx(1, stranger, 5, 500_000)).await);
        assert_eq!(f.inner_calls(), 1);
        assert_eq!(
            f.classifier.slot_owner(&stranger, 5),
            None,
            "a non-preconf tx has no arm to defend, so it claims no slot",
        );
    }

    // --- Verdict release on the way out ----------------------------------

    /// A transaction that did not make it into the pool must keep neither its
    /// verdict nor its slot claim: the verdict's timestamp means "the moment
    /// this entered the pool", and a stranded claim would block the nonce until
    /// the next sweep. `Error` is checked alongside `Invalid` because it is a
    /// third variant, not a flavour of the second.
    #[tokio::test]
    async fn a_rejection_by_the_inner_validator_releases_the_verdict() {
        for outcome in [Inner::Invalid, Inner::Error] {
            let f = fixture(outcome, 1_000_000);

            let result = f.validate(op_tx(1, sender(), 5, 21_000)).await;

            assert!(!matches!(result, TransactionValidationOutcome::Valid { .. }));
            assert_eq!(f.inner_calls(), 1, "{outcome:?}: the gates passed it through");
            assert_eq!(f.classifier.verdict(&h(1)), None, "{outcome:?}: verdict released");
            assert_eq!(f.classifier.slot_owner(&sender(), 5), None, "{outcome:?}: slot released");
        }
    }

    /// `Promised` is exempt from that release too. Journal restore relies on the
    /// verdict surviving an `add_envelope` that fails because the commitment is
    /// already on chain; dropping it here would also re-expose the
    /// reorg-reinject path the exemption protects.
    #[tokio::test]
    async fn a_rejected_promised_tx_keeps_its_verdict() {
        for outcome in [Inner::Invalid, Inner::Error] {
            let f = fixture(outcome, 1_000_000);
            assert_eq!(f.classifier.mark_promised(h(1), &sender(), 5), Ok(()));

            let result = f.validate(op_tx(1, sender(), 5, 21_000)).await;

            assert!(!matches!(result, TransactionValidationOutcome::Valid { .. }));
            assert_eq!(
                f.classifier.verdict(&h(1)),
                Some(Verdict::Promised),
                "{outcome:?}: a restored commitment must survive a failed re-admission",
            );
        }
    }

    /// The **other** commitment that must survive a failed re-admission, and the
    /// one the `Promised` exemption above does *not* cover.
    ///
    /// A commitment made in this process keeps `Verdict::Eligible` — the promise
    /// is recorded on a separate field precisely so the validator's `Promised`
    /// exemption does not widen (see `CachedVerdict`). So once it lands, its
    /// record sits here with `Eligible` + a live `committed_height`, inside the
    /// retention window.
    ///
    /// Re-validating that hash is ordinary: the transaction left the pool when
    /// it landed, so a p2p re-announcement or a client retry runs the full
    /// validator, and the inner one rejects it as nonce-too-low. That reaches
    /// the release branch with a **not-Valid outcome and a non-`Promised`
    /// verdict** — both conditions true. Releasing there would hand the nonce
    /// back inside the retention window and undo the whole scheme; only
    /// `release_unless_committed`'s own condition stops it.
    #[tokio::test]
    async fn a_landed_commitment_survives_a_failed_rebroadcast() {
        for outcome in [Inner::Invalid, Inner::Error] {
            let f = fixture(outcome, 1_000_000);

            // Admitted, receipt returned, landed, fifo entry already forwarded
            // away — the shape of a commitment inside its retention window.
            // Through the preconf RPC, which is the only door that yields `Eligible`.
            let _ = f.classifier.claim_preconf(h(1), &sender(), Some(&recipient()));
            let _ = f.classifier.admit_and_claim(h(1), &sender(), 5);
            assert_eq!(f.classifier.mark_promised(h(1), &sender(), 5), Ok(()));
            assert!(f.classifier.mark_committed(&h(1), 100));
            f.classifier.release_unless_committed(&h(1)); // what `forward` fires
            assert_eq!(f.classifier.slot_owner(&sender(), 5), Some(h(1)));
            assert_eq!(f.classifier.verdict(&h(1)), Some(Verdict::Eligible), "not Promised");

            // The same hash comes back and the inner validator refuses it.
            let result = f.validate(op_tx(1, sender(), 5, 21_000)).await;
            assert!(!matches!(result, TransactionValidationOutcome::Valid { .. }));

            assert_eq!(
                f.classifier.slot_owner(&sender(), 5),
                Some(h(1)),
                "{outcome:?}: a landed commitment must keep its nonce through the retention window",
            );
            assert!(f.classifier.is_promised(&h(1)), "{outcome:?}: and its promise record");
        }
    }

    /// **The hole that makes `Admission::Fresh` necessary.** A wallet retry or a
    /// load-balancer replay re-runs the whole validator against a transaction
    /// that is already in the pool with a live fifo entry, and the re-run can
    /// fail on state the inner validator re-reads. Dropping the record on any
    /// non-`Valid` outcome would then strand that entry without its
    /// `(sender, nonce)`. The full argument is on the release branch in
    /// `validate_transaction`.
    ///
    /// Its counterpart — so the rule cannot be satisfied by never releasing
    /// anything — is `a_rejection_by_the_inner_validator_releases_the_verdict`,
    /// which drives the same failure on a **first** admission and requires the
    /// record to be dropped.
    #[tokio::test]
    async fn a_repooled_tx_that_fails_revalidation_keeps_its_slot() {
        for outcome in [Inner::Invalid, Inner::Error] {
            let f = fixture_admitting_then(outcome, 1_000_000);

            // The first admission is driven through the real path rather than
            // installed by hand, so the record this test is about is one a real
            // preconf submission produced.
            assert_admitted(&f.validate_preconf(op_tx(1, sender(), 5, 21_000)).await);
            assert_eq!(f.classifier.slot_owner(&sender(), 5), Some(h(1)));

            // The listener's push is still simulated: the fixture wraps a
            // validator, not a pool, so no pool event ever reaches
            // `PreconfPoolListener`. This is the one hand-made half of the
            // precondition — and it is not what the release branch reads.
            f.fifo.push_if_absent(fifo_tx(1, 5), sender(), PreconfSource::Rpc).await;

            // The same hash comes round again and this time the inner validator
            // says no.
            let result = f.validate(op_tx(1, sender(), 5, 21_000)).await;
            assert!(!matches!(result, TransactionValidationOutcome::Valid { .. }));
            // The guard cannot see a same-hash resubmit: `claim` is idempotent, so
            // the re-validation looks exactly like a first admission from here and
            // is delegated rather than refused. That is *why* the outcome alone
            // cannot decide whether to release.
            assert_eq!(f.inner_calls(), 2, "{outcome:?}: the re-validation was delegated");

            assert_eq!(
                f.classifier.slot_owner(&sender(), 5),
                Some(h(1)),
                "{outcome:?}: a re-validation must not release a record it did not create",
            );
            assert!(
                f.fifo.contains(&h(1)).await,
                "{outcome:?}: and the entry it would have stranded is still there",
            );
        }
    }

    /// **Why the exemption keys on `is_promised()` and not `Verdict::Promised`.**
    /// Only journal restore writes that variant. A reorg reinject is re-admitted
    /// by the pool, so its record already exists and keeps `Verdict::Eligible`;
    /// keying on the variant would wave a commitment past the per-tx gas ceiling
    /// after a **restart** but not after a **reorg**, and an operator who lowered
    /// `--preconf.max-gas-per-tx` in between would see the reinject refused and
    /// the commitment broken.
    #[tokio::test]
    async fn a_reorged_commitment_bypasses_a_lowered_gas_ceiling() {
        // Ceiling now lower than the tx asked for when it was promised.
        let f = fixture(Inner::Valid, 21_000);

        // Promised in *this* process: classified on the way in, receipt returned.
        // The verdict stays `Eligible` — only the `promised` flag is set.
        // Through the preconf RPC, which is the only door that yields `Eligible`.
        let _ = f.classifier.claim_preconf(h(1), &sender(), Some(&recipient()));
        let _ = f.classifier.admit_and_claim(h(1), &sender(), 5);
        assert_eq!(f.classifier.mark_promised(h(1), &sender(), 5), Ok(()));
        assert_eq!(f.classifier.verdict(&h(1)), Some(Verdict::Eligible), "not Promised");

        // The pool re-admits it after a reorg.
        assert_admitted(&f.validate(op_tx(1, sender(), 5, 500_000)).await);
        assert_eq!(f.inner_calls(), 1, "reached the inner validator, i.e. the ceiling was skipped");
    }

    /// The counterpart: a transaction that was never promised is still subject to
    /// the ceiling, so the exemption above cannot be read as "the ceiling is
    /// gone".
    #[tokio::test]
    async fn an_unpromised_tx_still_hits_the_gas_ceiling() {
        let f = fixture(Inner::Valid, 21_000);

        assert_refused::<PreconfGasLimitExceeded>(
            &f.validate_preconf(op_tx(1, sender(), 5, 500_000)).await,
        );
        assert_eq!(f.inner_calls(), 0);
    }
}

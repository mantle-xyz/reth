//! The preconf allowlists, mirrored from the on-chain contract.
//!
//! ## Where eligibility is decided
//!
//! Once the allowlists became on-chain governed and refreshable at runtime (see
//! [`crate::whitelist`]), "is this transaction preconf-eligible?" became a
//! function of *when you ask*. Two questions hide in that one.
//!
//! **Which build arm owns the transaction** is settled once, at admission: what
//! [`PreconfClassifier::preview_eligibility`] lets through becomes a fifo entry,
//! and both arms skip a hash iff the fifo holds it
//! (`builder::payload_builder::apply_one_best_tx` for the pool arm). A later
//! allowlist update cannot move a transaction between the arms.
//!
//! **Whether policy still authorizes it** is re-decided per entry at build
//! time, by `builder::payload_builder::barred_by_allowlist`, against the
//! allowlist in force for that block. **That is the binding check**; the
//! admission one is a non-authoritative preview.
//!
//! ## Why the lists are private
//!
//! There is deliberately no public `is_preconf_tx`. Re-deriving eligibility is
//! allowed — [`PreconfClassifier::whitelist_snapshot`] hands out an
//! `Arc<Whitelist>` and [`Whitelist::is_eligible`] evaluates against it, which
//! is what the payload builder does once per block — but the shape forces the
//! caller to **name which allowlist it means**.
//!
//! ## Locking
//!
//! `parking_lot`, because the builder reads the snapshot from a sync `fn`. The
//! commitment records that used to share this type live in
//! [`crate::commitments`] now; they never shared a lock with the lists.

use alloy_primitives::{Address, map::foldhash::HashSet};
use parking_lot::RwLock;
use std::sync::Arc;

use crate::config::PreconfConfig;

/// The preconf allowlists, mirrored from the on-chain `PreconfWhitelist`
/// contract (see [`crate::whitelist`]).
///
/// Lives here rather than on [`PreconfConfig`] because [`PreconfClassifier`]
/// owns the lists privately; a reader that wants to evaluate the predicate has
/// to name which snapshot it means — see the module docs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Whitelist {
    /// Exact `(from, to)` rules.
    pub pairs: HashSet<(Address, Address)>,
    /// Senders whose every transaction is eligible, whatever the recipient —
    /// including a contract creation, which has no recipient at all.
    pub from_wildcards: HashSet<Address>,
    /// Recipients that make any transaction to them eligible, whatever the
    /// sender.
    pub to_wildcards: HashSet<Address>,
}

impl Whitelist {
    /// The three-way OR that decides eligibility, on **this** set of lists.
    ///
    /// A method rather than inline code in `PreconfClassifier::evaluate_whitelist`
    /// because the payload builder evaluates the same question against a
    /// different `Whitelist` — the build-scoped snapshot that carries a
    /// governance update landing in the block being built. Two copies of the
    /// predicate would be two places for the wildcard rules to drift apart.
    ///
    /// Says nothing about `enabled` / `all_preconfs`; those are classifier
    /// state, and the caller applies them.
    pub fn is_eligible(&self, from: &Address, to: Option<&Address>) -> bool {
        match to {
            None => self.from_wildcards.contains(from),
            Some(to) => {
                self.pairs.contains(&(*from, *to)) ||
                    self.from_wildcards.contains(from) ||
                    self.to_wildcards.contains(to)
            }
        }
    }
}

/// Owns the preconf allowlists.
///
/// Held as `Arc<PreconfClassifier>` and shared by the on-chain whitelist watcher
/// (the only writer), admission's early-rejection preview, and the payload
/// builder's per-block snapshot.
///
/// The commitment records used to live here too. They moved to
/// [`Commitments`](crate::commitments::Commitments), owned by the fifo — the two
/// halves never shared a lock, a consumer, or a method.
#[derive(Debug)]
pub struct PreconfClassifier {
    /// Mirrors `PreconfConfig::enabled`. When false this node runs no preconf
    /// machinery at all, so nothing is classified and **nothing is cached**.
    ///
    /// The leak this originally guarded against is gone — a node that has not
    /// opted in no longer builds the RPC handler, so nothing reaches the
    /// classifier to cache anything. Refusing here anyway keeps the invariant
    /// the classifier's own, rather than a consequence of how it is wired.
    enabled: bool,

    /// Mirrors `PreconfConfig::all_preconfs`: bypass the allowlists entirely.
    /// Copied rather than referenced because it is immutable after config
    /// validation.
    all_preconfs: bool,

    /// The allowlists, mirrored from the on-chain contract. **Private** — this
    /// is the point of the module. All three sets share one lock so a refresh
    /// swaps them together; with separate locks a reader could pair a new `from`
    /// against a stale `to`.
    ///
    /// Behind an `Arc` so [`Self::whitelist_snapshot`] can pin the lists without
    /// copying them — see there for why that has to be a refcount bump.
    whitelist: RwLock<Arc<Whitelist>>,
}

impl PreconfClassifier {
    /// Builds an **enabled** classifier with explicit parameters.
    ///
    /// The disabled shape is only reachable through [`Self::from_config`],
    /// which is also the only way production builds one.
    pub fn new(all_preconfs: bool) -> Self {
        Self { enabled: true, all_preconfs, whitelist: RwLock::new(Arc::new(Whitelist::default())) }
    }

    /// Builds a classifier from validated config.
    ///
    /// The allowlists start **empty** — they are filled by `bootstrap_whitelist`
    /// before anything that can reach the classifier comes up.
    pub fn from_config(cfg: &PreconfConfig) -> Self {
        Self { enabled: cfg.enabled, ..Self::new(cfg.all_preconfs) }
    }

    /// Replaces the whole allowlist in one write. Called by the whitelist
    /// watcher.
    ///
    /// One write for all three sets, not three: they are three parts of a
    /// single policy and a reader must never see a mix of old and new. The
    /// watcher reads them from one state view for the same reason.
    ///
    /// Leaves every existing commitment record as it is — a record states what
    /// its transaction was classified as at admission, and nothing here rewrites
    /// it. That is **not** the same as "only future transactions are affected":
    /// the payload builder snapshots these lists per block
    /// (`builder::payload_builder::barred_by_allowlist`), so a revocation
    /// landing here still bars an already-admitted entry that has not been built
    /// yet.
    pub fn update_whitelist(
        &self,
        pairs: HashSet<(Address, Address)>,
        from_wildcards: HashSet<Address>,
        to_wildcards: HashSet<Address>,
    ) {
        *self.whitelist.write() = Arc::new(Whitelist { pairs, from_wildcards, to_wildcards });
    }

    /// Current allowlist sizes as `(pairs, from_wildcards, to_wildcards)`.
    ///
    /// For assertions only — nothing in production reads it. Public rather than
    /// `#[cfg(test)]` because the integration tests are a separate crate, and
    /// `whitelist_onchain` polls it to wait for a governance update to land.
    pub fn whitelist_counts(&self) -> (usize, usize, usize) {
        let wl = self.whitelist.read();
        (wl.pairs.len(), wl.from_wildcards.len(), wl.to_wildcards.len())
    }

    /// Pins the current allowlist so a reader can hold one fixed view of it.
    ///
    /// A refcount bump, not a copy — which is what makes it affordable for the
    /// payload builder to take one per block. [`Self::update_whitelist`] swaps
    /// the `Arc` wholesale, so a snapshot taken before a refresh keeps the lists
    /// it was taken with, and every transaction in one block is judged against
    /// the same policy even if governance lands mid-build.
    pub fn whitelist_snapshot(&self) -> Arc<Whitelist> {
        self.whitelist.read().clone()
    }

    /// Number of commitment records — for logging, metrics and assertions.
    /// nothing, so it cannot pre-empt the record the apply establishes (see
    /// [`crate::commitments::Commitments::record_announced`]).
    pub fn preview_eligibility(&self, from: &Address, to: Option<&Address>) -> bool {
        self.evaluate_whitelist(from, to)
    }

    /// The allowlist rule itself. Private, and the only reader of
    /// [`Self::whitelist`].
    ///
    /// A plain three-way OR, with no precedence and no deny list:
    ///
    /// ```text
    /// eligible(from, to) <=> pairs.contains((from, to))
    ///                     || from_wildcards.contains(from)
    ///                     || to_wildcards.contains(to)
    /// ```
    ///
    /// One consequence is worth stating because governance will meet it:
    /// revoking an exact rule does **not** revoke traffic that a wildcard also
    /// covers. `(A, X)` can be removed from `pairs` and `A -> X` stays eligible
    /// while `A` is a from wildcard.
    ///
    /// # Contract creations
    ///
    /// A creation has no recipient — `TxKind::Create`, not `Call(0x0)` — so it
    /// reaches here as `None` and can match neither `pairs` nor `to_wildcards`,
    /// both of which need a `to`. A from wildcard is the only rule that can
    /// authorize it, which is exactly what "every transaction from this sender"
    /// says.
    ///
    /// This is the crate's one recorded **divergence from op-geth**, whose
    /// `IsPreconfTx` returns false whenever `to == nil`
    /// (`preconf/tx_pool_config.go`); it also still cross-products two
    /// independent lists rather than holding explicit rules. op-geth is the
    /// reference implementation, not consensus — preconf runs on a single
    /// sequencer — so divergence is allowed, but only deliberately.
    ///
    /// Note also that a transfer *to* `address(0)` is a normal transaction here,
    /// distinct from a creation. It simply can never match on the `to` side: the
    /// contract refuses to store the zero address, which it reserves as the
    /// calldata marker that routes a rule to a wildcard set.
    fn evaluate_whitelist(&self, from: &Address, to: Option<&Address>) -> bool {
        self.enabled && (self.all_preconfs || self.whitelist.read().is_eligible(from, to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::map::foldhash::HashSet;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    /// A set of exact `(from, to)` rules.
    fn pair_set(entries: &[(Address, Address)]) -> HashSet<(Address, Address)> {
        entries.iter().copied().collect()
    }

    fn set(addrs: &[Address]) -> HashSet<Address> {
        addrs.iter().copied().collect()
    }

    /// Allows `addr(1)` → `addr(2)` and nothing else.
    fn classifier() -> PreconfClassifier {
        let c = PreconfClassifier::new(false);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), HashSet::default(), HashSet::default());
        c
    }

    /// One of each rule form: `(1 -> 2)` exact, `3` a from wildcard, `4` a to
    /// wildcard.
    fn or_classifier() -> PreconfClassifier {
        let c = PreconfClassifier::new(false);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), set(&[addr(3)]), set(&[addr(4)]));
        c
    }

    /// **The predicate, exhaustively.** Each of the three rules must be
    /// sufficient on its own, and their absence must be sufficient to refuse —
    /// the table covers every combination of the three sub-predicates, so
    /// turning the OR into an AND, or dropping any one arm, kills a row.
    #[test]
    fn eligibility_is_the_or_of_three_rules() {
        let c = or_classifier();
        // (from, to, pair?, from-wc?, to-wc?, expected)
        let cases = [
            (addr(1), addr(2), true, false, false, true),
            (addr(3), addr(9), false, true, false, true),
            (addr(9), addr(4), false, false, true, true),
            (addr(3), addr(4), false, true, true, true),
            (addr(1), addr(4), false, false, true, true),
            (addr(3), addr(2), false, true, false, true),
            (addr(9), addr(9), false, false, false, false),
            // The exact rule is directional: the reverse is none of the three.
            (addr(2), addr(1), false, false, false, false),
        ];
        for (from, to, pair, from_wc, to_wc, want) in cases {
            assert_eq!(
                c.preview_eligibility(&from, Some(&to)),
                want,
                "from={from:?} to={to:?} (pair={pair} from_wc={from_wc} to_wc={to_wc})",
            );
        }
    }

    /// The consequence governance will actually meet: revoking an exact rule
    /// does **not** revoke traffic a wildcard also covers. Stated in
    /// `evaluate_whitelist`'s docs, pinned here so it cannot quietly become a
    /// precedence rule.
    #[test]
    fn a_wildcard_still_covers_traffic_whose_exact_rule_was_revoked() {
        let c = PreconfClassifier::new(false);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), set(&[addr(1)]), HashSet::default());
        assert!(c.preview_eligibility(&addr(1), Some(&addr(2))));

        // Governance drops the exact rule but leaves the sender wildcard.
        c.update_whitelist(HashSet::default(), set(&[addr(1)]), HashSet::default());
        assert!(
            c.preview_eligibility(&addr(1), Some(&addr(2))),
            "the wildcard still authorizes it — revoking needs both",
        );

        c.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());
        assert!(!c.preview_eligibility(&addr(1), Some(&addr(2))));
    }

    /// **Contract creations have no recipient**, so only a from wildcard can
    /// authorize them: `pairs` and `to_wildcards` both need a `to` to match
    /// against. A deliberate divergence from op-geth — see `evaluate_whitelist`.
    #[test]
    fn a_contract_creation_is_eligible_only_through_a_from_wildcard() {
        let c = or_classifier();

        assert!(c.preview_eligibility(&addr(3), None), "from wildcard covers a creation");
        assert!(
            !c.preview_eligibility(&addr(1), None),
            "an exact rule cannot: a creation has no `to` to match its other half",
        );
        assert!(
            !c.preview_eligibility(&addr(9), None),
            "and a to wildcard cannot cover a transaction with no recipient at all",
        );
    }

    /// A transfer **to** the zero address is an ordinary transaction, distinct
    /// from a contract creation, and is judged by the ordinary `Some(to)` arm.
    /// Pinned because flattening `TxKind::Create` into `Some(Address::ZERO)`
    /// anywhere upstream would collapse two cases the rule treats differently:
    /// this one can be authorized by a to wildcard or an exact pair, a creation
    /// cannot.
    ///
    /// That the zero address can never be on the `to` side of a *rule* is a
    /// separate guarantee, owned and tested one layer up — see
    /// `whitelist::report_zero_entries`. Asserting it here would mean
    /// hand-building an allowlist the production path cannot produce.
    #[test]
    fn a_transfer_to_the_zero_address_is_judged_like_any_other() {
        let c = PreconfClassifier::new(false);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), set(&[addr(3)]), HashSet::default());

        assert!(
            c.preview_eligibility(&addr(3), Some(&Address::ZERO)),
            "the sender's wildcard covers it, exactly as it would any other recipient",
        );
        assert!(
            !c.preview_eligibility(&addr(1), Some(&Address::ZERO)),
            "and an exact rule for a different recipient does not",
        );
    }

    #[test]
    fn update_whitelist_replaces_wholesale_and_accessors_follow() {
        let c = classifier();
        assert_eq!(c.whitelist_counts(), (1, 0, 0));

        c.update_whitelist(
            pair_set(&[(addr(3), addr(4))]),
            set(&[addr(5)]),
            set(&[addr(6), addr(7)]),
        );

        assert_eq!(c.whitelist_counts(), (1, 1, 2));
        let snapshot = c.whitelist_snapshot();
        assert!(snapshot.pairs.contains(&(addr(3), addr(4))));
        assert!(snapshot.from_wildcards.contains(&addr(5)));
        assert!(snapshot.to_wildcards.contains(&addr(7)));
        // Replacement, not a union: the previous generation is gone.
        assert!(!snapshot.pairs.contains(&(addr(1), addr(2))));
        assert!(!c.preview_eligibility(&addr(1), Some(&addr(2))));
    }
}

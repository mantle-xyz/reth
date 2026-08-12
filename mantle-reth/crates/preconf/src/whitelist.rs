//! Reads the preconf allowlists from the on-chain L2 `PreconfWhitelist`
//! contract and keeps [`PreconfClassifier`]'s in-memory copy in sync.
//!
//! The contract is the single source of truth: governance updates it through a
//! standard OP-Stack L1→L2 cross-domain message, so the lists are auditable
//! on-chain and every node converges on the same values. This module is the
//! sequencer-local mirror of that state — it is a policy input to the
//! classifier, not part of consensus.
//!
//! Two entry points:
//!
//! * [`bootstrap_whitelist`] — cold start. Verifies the configured address actually holds a
//!   contract, then loads both lists.
//! * [`run_whitelist_watcher`] — long-running task. Re-reads whenever a canonical block carries a
//!   `WhitelistUpdated` log, or a reorg lands.
//!
//! ## Whose decision is it
//!
//! The split of responsibility is deliberate and worth stating, because it
//! decides which conditions are fatal:
//!
//! * **Governance owns *who* is eligible.** Whatever the contract says — including two empty lists,
//!   meaning nobody — is authoritative. The node mirrors it and never overrides it, so an empty
//!   allowlist is a legitimate state that warns but does not stop the node.
//! * **This node owns its own configuration.** A `--preconf.whitelist-contract` that holds no code
//!   is reth's mistake, not governance's, and is fatal.
//! * **The operator owns the on/off switch** (`--preconf.enable` for a full rollback to the
//!   upstream payload path, `--preconf.all` to bypass the lists). Disabling preconf is not
//!   expressed by draining the allowlists.
//!
//! ## Storage layout coupling
//!
//! The slot numbers below mirror the declaration order in
//! `mantle-v2/packages/contracts-bedrock/src/L2/PreconfWhitelist.sol`. Nothing
//! links them at compile time, so both sides assert them:
//! `test/PreconfWhitelist.t.sol` pins the layout with `vm.load`, and the tests
//! at the bottom of this file pin the derived slot bases. Changing the contract's
//! state-variable order without updating [`FROM_WILDCARDS_SLOT`] / [`TO_WILDCARDS_SLOT`]
//! would make the sequencer read garbage — or, worse, an empty list.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, B256, U256, keccak256, map::foldhash::HashSet};
use futures::StreamExt;
use reth_chain_state::{CanonStateNotification, CanonStateSubscriptions};
use reth_execution_types::Chain;
use reth_primitives_traits::NodePrimitives;
use reth_storage_api::{StateProviderBox, StateProviderFactory};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

use crate::{classifier::PreconfClassifier, config::PreconfConfig};

/// Slot of `Pair[] pairs` — the exact `(from, to)` rules.
///
/// **Elements span two slots**, not one: a `Pair` is two `address` fields, 40
/// bytes, which cannot share a 32-byte slot. `pairs[i].from` is at
/// `keccak256(0) + 2i` and `pairs[i].to` at `keccak256(0) + 2i + 1`. Pinned at
/// runtime by `test_storageLayout_matchesRethExpectations_succeeds` in
/// `test/PreconfWhitelist.t.sol`, which reads all four of those slots with
/// `vm.load`.
pub const PAIRS_SLOT: u64 = 0;

/// Slot of `address[] fromWildcards` — senders whose every transaction is
/// eligible.
///
/// Slot 1 is `pairIndex`, the membership mapping for `pairs`.
pub const FROM_WILDCARDS_SLOT: u64 = 2;

/// Slot of `address[] toWildcards` — recipients that make any transaction to
/// them eligible.
///
/// Slot 3 is `fromWildcardIndex`.
pub const TO_WILDCARDS_SLOT: u64 = 4;

/// Slot of `uint256 layoutVersion` — the contract's declaration of which storage
/// layout it was deployed with.
///
/// Appended after every array on the Solidity side on purpose, so bumping the
/// version can never shift the slots above.
pub const LAYOUT_VERSION_SLOT: u64 = 6;

/// The layout this binary knows how to read.
///
/// `1` was the cross-product allowlist (`preconfFromList` / `preconfToList`).
/// That contract never wrote [`LAYOUT_VERSION_SLOT`], so it reads back as `0`
/// and is refused by the same check.
///
/// The comparison is **exact**, not a minimum: a future layout moves slots, so a
/// binary built for this one has no business reading it.
pub const EXPECTED_LAYOUT_VERSION: u64 = 2;

/// `keccak256("WhitelistUpdated(uint256,uint256,uint256)")` — topic0 of the
/// contract's only event.
///
/// Asserted against the same literal by `test_whitelistUpdatedTopic0_isStable`
/// in `test/PreconfWhitelist.t.sol`. If the event signature changes and this
/// constant does not, the watcher stops firing and the sequencer runs forever on
/// a stale allowlist — silently.
///
/// The signature gained a third count when the allowlist became explicit pairs,
/// so this value changed. A binary built against the old one pairs with a new
/// contract by loading the allowlist once at bootstrap and then never seeing
/// another governance update.
pub const WHITELIST_UPDATED_TOPIC0: B256 = B256::new([
    0x53, 0x2f, 0xe7, 0x09, 0xf3, 0x40, 0xed, 0xa4, 0x0c, 0x9d, 0x51, 0xe7, 0xdb, 0xba, 0xcf, 0x9d,
    0x5b, 0x25, 0x5b, 0x36, 0x42, 0x9e, 0xd9, 0x0f, 0x86, 0x5b, 0xd2, 0xa3, 0x13, 0x1e, 0xf1, 0xbc,
]);

/// Warn once a list passes this size — **advisory only, never rejects**.
///
/// The allowlist itself is deliberately **unbounded**: its length is a
/// governance decision and this node has no business overriding it (the same
/// reasoning that makes an empty list acceptable — see "whose decision is it"
/// above). What *is* bounded is how long reading it may block, which is
/// [`WHITELIST_READ_BUDGET`]'s job.
///
/// The two must not be conflated. A count limit bounds the harm only indirectly
/// while capping policy directly — exactly backwards. This threshold therefore
/// only tells the operator "the read is getting expensive", derived from the
/// measured cost below.
///
/// ## Measured cost
///
/// [`read_preconf_set`] issues one `StateProvider::storage` call per entry, and
/// that loop is irreducible: `StateProvider` exposes only the single-key
/// `storage()`, and the one bulk API that exists
/// (`StorageReader::plain_state_storages`) reads *persisted* plain state rather
/// than the `latest()` view this module needs, so it would trade correctness for
/// speed.
///
/// Measured on a real provider (20k entries, genesis-allocated state, warm
/// caches): **1.06 µs per entry** — a floor; a live node reading a large trie
/// with cold caches is materially worse.
///
/// | entries | measured floor | cold-cache estimate |
/// |---|---|---|
/// | 100k | 0.11 s | ~1 s |
/// | **1M (this threshold)** | **1.1 s** | **~11 s** |
/// | 10M | 11 s | ~110 s |
///
/// For scale: the contract caps one update at `MAX_BATCH = 500` entries and 500
/// adds measure ~23M gas, i.e. a full L2 block per governance message — so
/// reaching 1M takes ~2000 such messages.
pub const WHITELIST_WARN_THRESHOLD: usize = 1_000_000;

/// How long one full list read may block before it is abandoned.
///
/// This guard replaces a count limit, and it guards the thing that actually
/// hurts. `read_preconf_set` takes the array length from **slot 0 of the
/// configured address**, so a wrong-but-deployed contract supplies that number.
/// The has-code check in [`bootstrap_whitelist`] proves *something* is deployed
/// there; it cannot prove it is a `PreconfWhitelist`. Plausible slot-0 values,
/// and what an unbounded loop would then do:
///
/// | slot 0 happens to hold | value | blocked for (at 1.06 µs/entry) |
/// |---|---|---|
/// | a timestamp (`lastUpdated`, …) | ~1.77e9 | **~31 minutes** |
/// | a wei amount / `totalSupply` | ~1e18 | ~34,000 years |
/// | anything ≥ `u64::MAX` after saturation | 1.84e19 | ~620,000 years |
///
/// Note the first row: it does not take an astronomical number. And the failure
/// mode is the worst one available — no error, no progress, a node that has
/// simply stopped. A time budget turns that into a bounded, explanatory failure
/// **without capping the list**: any list the machine can actually read still
/// loads, however long it is.
///
/// Fatal at cold start (like the has-code check — this node's own configuration
/// is what is being judged); at reload the previous lists stay in force and the
/// failure is a warning, which is already the behaviour for any read error.
pub const WHITELIST_READ_BUDGET: Duration = Duration::from_secs(30);

/// How often the budget is checked inside the read loop, in **storage reads**.
///
/// Large enough that the `Instant::now()` cost is noise against 4096 storage
/// reads (~4ms at the measured rate), small enough to bound overshoot to well
/// under a second.
const BUDGET_CHECK_STRIDE: u64 = 4096;

/// Enforces [`WHITELIST_READ_BUDGET`] across a read loop.
///
/// Counts **storage reads**, not array elements. The distinction is load-bearing
/// now that one reader walks `address[]` (one read per element) and another
/// walks `Pair[]` (two): counting elements would silently halve the check
/// frequency for pairs and double the overshoot, while the constant above claims
/// to bound both. Charging what is actually spent keeps one budget meaningful
/// for both.
struct ReadBudget {
    started: Instant,
    budget: Duration,
    reads: u64,
}

impl ReadBudget {
    fn new(budget: Duration) -> Self {
        Self { started: Instant::now(), budget, reads: 0 }
    }

    /// Charges one storage read. `Some(elapsed)` means the budget is spent and
    /// the caller must abort **without** performing that read.
    ///
    /// The first call always checks, so a zero budget does no work at all —
    /// that is the intended contract of the test seam, not an edge case. The
    /// comparison is `>=` rather than `>` because `elapsed` can legitimately
    /// read back as 0ns and `> 0` would then let a whole stride through.
    fn charge(&mut self) -> Option<Duration> {
        let check = self.reads.is_multiple_of(BUDGET_CHECK_STRIDE);
        self.reads += 1;
        if check {
            let elapsed = self.started.elapsed();
            if elapsed >= self.budget {
                return Some(elapsed);
            }
        }
        None
    }
}

/// Errors from reading the whitelist contract.
#[derive(Debug, thiserror::Error)]
pub enum WhitelistError {
    /// The state provider failed. Never swallowed: a read error is not the same
    /// as an empty allowlist, and treating it as one would silently disable the
    /// preconf fast path.
    #[error("failed to read whitelist state: {0}")]
    Provider(#[from] reth_storage_api::errors::ProviderError),

    /// `whitelist_contract` holds no code — the contract is not deployed there,
    /// or the address is simply wrong.
    ///
    /// Fatal, and this check is what makes tolerating empty allowlists safe: a
    /// wrong address reads back as two empty lists, which is indistinguishable
    /// from "governance currently allows nobody". Proving there is a contract
    /// there first means a later empty read can be trusted as policy rather than
    /// silently masking a typo.
    #[error(
        "preconf whitelist contract {0} has no code — check --preconf.whitelist-contract and that the contract is deployed"
    )]
    ContractHasNoCode(Address),

    /// The contract at `whitelist_contract` declares a storage layout this binary
    /// cannot read.
    ///
    /// Fatal, and for the same reason [`Self::ContractHasNoCode`] is: what is
    /// being judged is this node's own configuration. The has-code check proves
    /// *something* is deployed there, not that it is a `PreconfWhitelist` at the
    /// layout these slot constants describe — and a skew in either direction is
    /// silent and actively wrong rather than merely stale. Read against the
    /// previous cross-product contract, [`FROM_WILDCARDS_SLOT`] lands on its
    /// **recipient** list, which would then be installed as sender wildcards:
    /// every transaction *from* a former recipient becomes preconf-eligible,
    /// authorized by nobody.
    ///
    /// `found: 0` is the specific signature of that previous contract, which
    /// never wrote the slot.
    #[error(
        "preconf whitelist contract {contract} declares storage layout {found}, but this build reads layout {expected} — deploy the matching PreconfWhitelist, or run a binary built for layout {found}"
    )]
    LayoutVersionMismatch {
        /// Configured contract address.
        contract: Address,
        /// The layout this binary reads.
        expected: u64,
        /// The layout the contract declares. `0` means the slot was never
        /// written, i.e. a pre-`layoutVersion` contract.
        found: u64,
    },

    /// The read did not finish within [`WHITELIST_READ_BUDGET`].
    ///
    /// Two very different causes, and this error cannot tell them apart — hence
    /// a message that names both:
    ///
    /// * the address does not point at a `PreconfWhitelist`, so slot 0 supplied a nonsense length
    ///   (by far the likelier cause — see [`WHITELIST_READ_BUDGET`]);
    /// * the list is genuine but larger than this machine can read inside the budget.
    #[error(
        "whitelist read at {contract} slot {slot} abandoned after {elapsed:?}: read {read} of {claimed_len} claimed entries — check that --preconf.whitelist-contract points at a PreconfWhitelist, or raise WHITELIST_READ_BUDGET if the list really is this large"
    )]
    ReadBudgetExceeded {
        /// Configured contract address.
        contract: Address,
        /// Array slot being read.
        slot: u64,
        /// Length the slot claimed.
        claimed_len: u64,
        /// How many entries were read before giving up.
        read: u64,
        /// Time spent before giving up.
        elapsed: Duration,
    },
}

/// State-source seam, mirroring [`crate::rpc`]'s approach and the one in
/// `mantle-reth-rpc-ext`.
///
/// A blanket impl covers every [`StateProviderFactory`], so production passes
/// the real provider unchanged while tests implement this single method to
/// inject provider failures without standing up a provider stack.
pub trait WhitelistState {
    /// Latest committed state.
    fn latest_state(&self) -> Result<StateProviderBox, reth_storage_api::errors::ProviderError>;
}

impl<P: StateProviderFactory> WhitelistState for P {
    fn latest_state(&self) -> Result<StateProviderBox, reth_storage_api::errors::ProviderError> {
        self.latest()
    }
}

/// First element slot of the dynamic array declared at `slot`: `keccak256(slot)`.
///
/// Solidity stores a dynamic array's length at its declaring slot and element
/// `i` at `keccak256(slot) + i`. Each `address` element occupies a whole slot.
fn array_data_base(slot: u64) -> U256 {
    U256::from_be_bytes(keccak256(B256::from(U256::from(slot))).0)
}

/// Reads the `address[]` at `list_slot` out of `contract`'s storage.
///
/// The list length is **not** capped — see [`WHITELIST_WARN_THRESHOLD`]. What is
/// capped is the time spent reading it ([`WHITELIST_READ_BUDGET`]), so a nonsense
/// length taken from the wrong contract fails in bounded time instead of hanging
/// the node.
///
/// Zero entries are skipped: the contract refuses to store `address(0)`, and an
/// unset slot also decodes to zero, so filtering keeps the two representations
/// in agreement.
pub fn read_preconf_set(
    state: &StateProviderBox,
    contract: Address,
    list_slot: u64,
) -> Result<HashSet<Address>, WhitelistError> {
    read_preconf_set_within(state, contract, list_slot, WHITELIST_READ_BUDGET)
}

/// [`read_preconf_set`] with an explicit budget.
///
/// Injected rather than read from the constant so both sides of the budget check
/// are testable without making a test actually spend 30 seconds.
pub(crate) fn read_preconf_set_within(
    state: &StateProviderBox,
    contract: Address,
    list_slot: u64,
    budget: Duration,
) -> Result<HashSet<Address>, WhitelistError> {
    let claimed_len = claimed_array_len(state, contract, list_slot)?;
    let base = array_data_base(list_slot);
    let mut budget = ReadBudget::new(budget);
    let mut out = HashSet::default();
    let mut zeros = 0u64;

    for i in 0..claimed_len {
        if let Some(elapsed) = budget.charge() {
            return Err(WhitelistError::ReadBudgetExceeded {
                contract,
                slot: list_slot,
                claimed_len,
                read: i,
                elapsed,
            });
        }
        let addr = read_address(state, contract, base, i)?;
        if addr.is_zero() {
            zeros += 1;
        } else {
            out.insert(addr);
        }
    }

    report_zero_entries(contract, list_slot, zeros);
    Ok(out)
}

/// Reads the `Pair[]` at `list_slot` out of `contract`'s storage.
///
/// Same budget and same zero handling as [`read_preconf_set`]; the difference is
/// the **stride**. A `Pair` is two `address` fields and cannot pack into one
/// slot, so element `i` occupies `base + 2i` (`from`) and `base + 2i + 1`
/// (`to`) — see [`PAIRS_SLOT`], and the `vm.load` assertions that pin it.
///
/// A pair with either half zero is discarded whole. In the new allowlist the
/// zero address is a **calldata-only marker** that routes a rule to one of the
/// wildcard sets; the contract never stores it in `pairs`. So a zero half here
/// is not a wildcard and not an empty slot — it means we are reading the wrong
/// layout, and taking half of it would invent a rule nobody wrote.
pub fn read_preconf_pairs(
    state: &StateProviderBox,
    contract: Address,
    list_slot: u64,
) -> Result<HashSet<(Address, Address)>, WhitelistError> {
    read_preconf_pairs_within(state, contract, list_slot, WHITELIST_READ_BUDGET)
}

/// [`read_preconf_pairs`] with an explicit budget. Injected for the same reason
/// as [`read_preconf_set_within`]'s.
pub(crate) fn read_preconf_pairs_within(
    state: &StateProviderBox,
    contract: Address,
    list_slot: u64,
    budget: Duration,
) -> Result<HashSet<(Address, Address)>, WhitelistError> {
    let claimed_len = claimed_array_len(state, contract, list_slot)?;
    let base = array_data_base(list_slot);
    let mut budget = ReadBudget::new(budget);
    let mut out = HashSet::default();
    let mut zeros = 0u64;

    for i in 0..claimed_len {
        // Two charges, because this element costs two reads. Charging once per
        // element would make the effective stride 8192 reads while the constant
        // says 4096.
        for _ in 0..2 {
            if let Some(elapsed) = budget.charge() {
                return Err(WhitelistError::ReadBudgetExceeded {
                    contract,
                    slot: list_slot,
                    claimed_len,
                    read: i,
                    elapsed,
                });
            }
        }
        let from = read_address(state, contract, base, 2 * i)?;
        let to = read_address(state, contract, base, 2 * i + 1)?;
        if from.is_zero() || to.is_zero() {
            zeros += 1;
        } else {
            out.insert((from, to));
        }
    }

    report_zero_entries(contract, list_slot, zeros);
    Ok(out)
}

/// The length a dynamic array claims, taken from its declaring slot.
///
/// Saturating: a length beyond `u64` is nonsense anyway, and the read budget is
/// what stops us acting on it. Keeping the claimed value for the error message
/// is deliberate — "read 4096 of 18446744073709551615" is what tells an operator
/// they have the wrong address.
fn claimed_array_len(
    state: &StateProviderBox,
    contract: Address,
    list_slot: u64,
) -> Result<u64, WhitelistError> {
    let raw = state.storage(contract, B256::from(U256::from(list_slot)))?.unwrap_or_default();
    Ok(raw.saturating_to::<u64>())
}

/// Reads the address stored `offset` slots past `base`.
fn read_address(
    state: &StateProviderBox,
    contract: Address,
    base: U256,
    offset: u64,
) -> Result<Address, WhitelistError> {
    let slot = base.saturating_add(U256::from(offset));
    let word = state.storage(contract, B256::from(slot))?.unwrap_or_default();
    Ok(Address::from_word(B256::from(word)))
}

/// Surfaces zero entries, which the contract cannot produce.
///
/// Under the cross-product allowlist a zero was an ordinary empty slot and
/// skipping it silently was right. It no longer is: every one of the three
/// arrays now stores real addresses only — the zero address exists solely as a
/// calldata marker that routes a rule to a wildcard set, and `updatePreconfs`
/// rejects the all-zero form outright. So a zero read back here means the layout
/// is wrong, the address is not a `PreconfWhitelist`, or the contract changed
/// without this constant following. Still skipped rather than fatal (a partial
/// allowlist beats no sequencer), but it is a signal someone has to see.
fn report_zero_entries(contract: Address, list_slot: u64, zeros: u64) {
    if zeros == 0 {
        return;
    }
    metrics::counter!("preconf.whitelist.zero_entry_skipped").increment(zeros);
    warn!(
        target: "mantle::preconf::whitelist",
        %contract, slot = list_slot, skipped = zeros,
        "whitelist array held zero entries, which the contract never stores — check that \
         --preconf.whitelist-contract points at a PreconfWhitelist and that the slot constants \
         match its layout",
    );
}

/// Whether this config wants the on-chain allowlists at all.
///
/// `all_preconfs` short-circuits the allowlist rule, so the contract is never
/// read in that mode; when preconf is off there is nothing to read for either.
fn wants_whitelist(cfg: &PreconfConfig) -> Option<Address> {
    if !cfg.enabled || cfg.all_preconfs {
        return None;
    }
    cfg.whitelist_contract
}

/// Installs freshly-read lists into `classifier`, publishes their sizes as
/// gauges, and flags the case where nothing can be eligible.
///
/// Shared by [`bootstrap_whitelist`] and [`reload_whitelist`] so the gauges and
/// the warning cannot drift between the cold-start and steady-state paths.
///
/// Returns the `(pairs, from_wildcards, to_wildcards)` sizes for the caller's
/// own log line.
fn apply_whitelist(
    classifier: &PreconfClassifier,
    contract: Address,
    pairs: HashSet<(Address, Address)>,
    from_wildcards: HashSet<Address>,
    to_wildcards: HashSet<Address>,
) -> (usize, usize, usize) {
    let (pair_len, from_len, to_len) = (pairs.len(), from_wildcards.len(), to_wildcards.len());
    classifier.update_whitelist(pairs, from_wildcards, to_wildcards);

    // The authoritative sizes, for dashboards and alerting. An empty allowlist is
    // a legitimate state that the node accepts silently otherwise (see below), so
    // these gauges are the only continuous signal that the fast path is live.
    metrics::gauge!("preconf.whitelist.pair_count").set(pair_len as f64);
    metrics::gauge!("preconf.whitelist.from_wildcard_count").set(from_len as f64);
    metrics::gauge!("preconf.whitelist.to_wildcard_count").set(to_len as f64);
    // Published so a dashboard can alert on the *ratio* without hard-coding the
    // cap, which would then drift from the binary.
    metrics::gauge!("preconf.whitelist.warn_threshold").set(WHITELIST_WARN_THRESHOLD as f64);

    // A large list is legitimate — nothing rejects it — but it is worth saying
    // out loud, because the cost lands on node startup where it is easy to
    // mistake for a hang. See `WHITELIST_WARN_THRESHOLD`.
    //
    // Counted in entries, but note a `pairs` entry costs *two* storage reads, so
    // the same count there takes about twice as long to load.
    if pair_len >= WHITELIST_WARN_THRESHOLD ||
        from_len >= WHITELIST_WARN_THRESHOLD ||
        to_len >= WHITELIST_WARN_THRESHOLD
    {
        warn!(
            target: "mantle::preconf::whitelist",
            %contract, pairs = pair_len, from_wildcards = from_len, to_wildcards = to_len,
            threshold = WHITELIST_WARN_THRESHOLD,
            budget = ?WHITELIST_READ_BUDGET,
            "preconf allowlist is large — each cold start and each refresh re-reads it one \
             storage slot at a time, and the read is abandoned past its budget",
        );
    }

    // An empty allowlist is a governance decision, not an error: the contract is
    // the sole authority and may legitimately allow nobody, so the node obeys
    // rather than refusing to run. It is still worth surfacing, since from the
    // operator's seat it looks identical to a misconfiguration.
    //
    // The condition is **all three empty**, not "any one empty". Eligibility is a
    // three-way OR now, so a populated `pairs` alone makes the fast path live
    // even with both wildcard sets empty — which is the expected steady state.
    // Warning on any-empty (the old rule, correct when eligibility was an AND
    // across two lists) would fire on every healthy configuration and train the
    // operator to ignore it.
    if pair_len == 0 && from_len == 0 && to_len == 0 {
        warn!(
            target: "mantle::preconf::whitelist",
            %contract,
            "preconf allowlist is empty — no transaction will take the preconf fast path until \
             governance populates it",
        );
    }

    (pair_len, from_len, to_len)
}

/// Re-reads both lists at the current canonical head and swaps them into `cfg`.
///
/// Reads `latest()` rather than a pinned block on purpose: it makes the reload
/// idempotent (several notifications collapse to the same end state) and handles
/// reorgs for free, since `latest()` is already the post-rollback view.
///
/// On error the existing lists are left untouched — a failed refresh must not
/// degrade into an empty allowlist.
///
/// # Why the success line is `info!`, not `debug!`
///
/// This is the **only** signal that the in-memory allowlists changed after
/// startup, and both of the watcher's triggers are operationally significant and
/// low-frequency on a sequencer (a governance action, or a reorg — see
/// [`run_whitelist_watcher`]). At `debug!` a governance update that has landed
/// on chain and taken effect leaves no trace at the default log level, so
/// "did my `updatePreconfs` reach the sequencer?" can only be answered from the
/// `preconf.whitelist.*` gauges. The failure path next to it is already `warn!`;
/// logging the success at `debug!` made the pair asymmetric.
pub fn reload_whitelist<P: WhitelistState>(
    provider: &P,
    cfg: &PreconfConfig,
    classifier: &PreconfClassifier,
) -> Result<(), WhitelistError> {
    let Some(contract) = wants_whitelist(cfg) else { return Ok(()) };

    let state = provider.latest_state()?;
    let (pairs, from_wildcards, to_wildcards) = read_all(&state, contract)?;

    let (pair_len, from_len, to_len) =
        apply_whitelist(classifier, contract, pairs, from_wildcards, to_wildcards);
    info!(
        target: "mantle::preconf::whitelist",
        %contract, pairs = pair_len, from_wildcards = from_len, to_wildcards = to_len,
        "refreshed preconf allowlist from L2 state",
    );
    Ok(())
}

/// Reads all three sets from one state view.
///
/// One `StateProviderBox` for all three on purpose: they are three halves of a
/// single policy, and reading them against different views could produce a
/// combination governance never wrote — a pair whose covering wildcard has
/// already been revoked, say. `latest()` is taken once by the caller and shared
/// The three allowlist collections a full read yields: exact `(from, to)` pairs,
/// `from` wildcards, `to` wildcards. Named so the read/reload signatures stay
/// legible — the tuple is threaded through several layers.
type AllowlistSets = (HashSet<(Address, Address)>, HashSet<Address>, HashSet<Address>);

/// here, so the three reads see the same state.
fn read_all(state: &StateProviderBox, contract: Address) -> Result<AllowlistSets, WhitelistError> {
    let pairs = read_preconf_pairs(state, contract, PAIRS_SLOT)?;
    let from_wildcards = read_preconf_set(state, contract, FROM_WILDCARDS_SLOT)?;
    let to_wildcards = read_preconf_set(state, contract, TO_WILDCARDS_SLOT)?;
    Ok((pairs, from_wildcards, to_wildcards))
}

/// Cold start: validate the configured address, then load both lists.
///
/// Returns `Ok(())` without touching state when preconf is disabled or running
/// in `all_preconfs` mode.
///
/// The has-code check is deliberately fatal, and it is the *only* fatal check
/// here. The distinction is about whose decision is being judged:
///
/// * **Address without code** — this node's own configuration is wrong (typo, or the contract is
///   not deployed). Refusing to start is correct: reth is validating its own input. Checked once,
///   because deployed code does not disappear (post-Cancun `selfdestruct` no longer clears it), so
///   the watcher does not repeat it.
/// * **Empty allowlists** — governance's current policy, faithfully reported by a contract that is
///   working exactly as intended. The node has no business overriding it, so it loads the empty
///   lists, warns, and runs (see [`apply_whitelist`]).
pub fn bootstrap_whitelist<P: WhitelistState>(
    provider: &P,
    cfg: &PreconfConfig,
    classifier: &PreconfClassifier,
) -> Result<(), WhitelistError> {
    let Some(contract) = wants_whitelist(cfg) else {
        // Two contradictory intents: `all_preconfs` makes every tx eligible and
        // short-circuits the allowlists, so the contract is never read. Worth
        // saying out loud, or the operator may believe the lists are in force.
        if cfg.enabled && cfg.all_preconfs && cfg.whitelist_contract.is_some() {
            warn!(
                target: "mantle::preconf::whitelist",
                whitelist_contract = ?cfg.whitelist_contract,
                "--preconf.all is set, so --preconf.whitelist-contract is ignored: every tx is \
                 preconf-eligible and the contract is never read",
            );
        }
        debug!(
            target: "mantle::preconf::whitelist",
            enabled = cfg.enabled, all_preconfs = cfg.all_preconfs,
            "skipping whitelist bootstrap",
        );
        return Ok(());
    };

    let state = provider.latest_state()?;
    let has_code = state.basic_account(&contract)?.is_some_and(|account| account.has_bytecode());
    if !has_code {
        return Err(WhitelistError::ContractHasNoCode(contract));
    }

    // Checked here and nowhere else, for the same reason the has-code check is:
    // it cannot change without a redeploy, and a redeploy means a new address
    // and a new `--preconf.whitelist-contract`. The watcher would only be
    // re-asking a question whose answer is fixed for this process's lifetime.
    let found = state
        .storage(contract, B256::from(U256::from(LAYOUT_VERSION_SLOT)))?
        .unwrap_or_default()
        .saturating_to::<u64>();
    if found != EXPECTED_LAYOUT_VERSION {
        return Err(WhitelistError::LayoutVersionMismatch {
            contract,
            expected: EXPECTED_LAYOUT_VERSION,
            found,
        });
    }

    let (pairs, from_wildcards, to_wildcards) = read_all(&state, contract)?;
    let (pair_len, from_len, to_len) =
        apply_whitelist(classifier, contract, pairs, from_wildcards, to_wildcards);

    info!(
        target: "mantle::preconf::whitelist",
        %contract, pairs = pair_len, from_wildcards = from_len, to_wildcards = to_len,
        "loaded preconf allowlist from L2 whitelist contract",
    );
    Ok(())
}

/// Whether `chain` contains a `WhitelistUpdated` log emitted by `contract`.
///
/// Split out from the watcher loop so it can be unit-tested against a
/// hand-built log set — constructing a full `CanonStateNotification` is far more
/// work than the logic warrants.
pub fn has_whitelist_event<N: NodePrimitives>(chain: &Chain<N>, contract: Address) -> bool {
    chain.receipts_iter().flat_map(TxReceipt::logs).any(|log| {
        log.address == contract && log.topics().first() == Some(&WHITELIST_UPDATED_TOPIC0)
    })
}

/// Whether `notif` means the allowlists must be re-read.
///
/// The watcher's whole decision, extracted so it can be unit-tested against real
/// [`CanonStateNotification`]s without standing up a notification stream
/// (`MockEthProvider` cannot emit any — its subscription drops the sender). See
/// [`run_whitelist_watcher`] for why a reorg counts on its own.
pub fn should_reload<N: NodePrimitives>(
    notif: &CanonStateNotification<N>,
    contract: Address,
) -> bool {
    // Any reorg: an update may have vanished, and a disappearance emits no log.
    if notif.reverted().is_some() {
        return true;
    }
    // Otherwise only a fresh `WhitelistUpdated` in the committed segment matters.
    // Bind the `Arc<Chain>` so the log borrows outlive the temporary.
    let committed = notif.committed();
    has_whitelist_event(&committed, contract)
}

/// Watches the canonical chain and refreshes the allowlists when they change.
///
/// Event-driven rather than parsing the deposit that carried the update: the
/// deposit's calldata targets `L2CrossDomainMessenger.relayMessage`, with the
/// actual `updatePreconfs` call buried in its `message` argument. Watching for
/// the effect instead of decoding the cause is both simpler and reorg-safe.
///
/// # Why a reorg alone triggers a reload
///
/// Two independent triggers, because **an event can only say that something
/// happened, never that something un-happened**:
///
/// 1. A `WhitelistUpdated` log in the committed blocks — an update landed.
/// 2. *Any* reorg — an update may have **disappeared**.
///
/// Trigger 2 is not "reload even though it was reverted". The reverted blocks are
/// never read; [`reload_whitelist`] reads `latest()`, i.e. the **post-rollback**
/// canonical state. It exists because rolling back a block that carried an update
/// silently invalidates what is already in memory, and nothing in the *new* chain
/// announces that: undoing an add emits no log. Concretely — an add lands in block
/// N and is mirrored into memory, then N is reorged out and the replacement chain
/// does not carry that deposit. On-chain the address is no longer allowlisted, but
/// this node would keep fast-pathing it until some unrelated later update happened
/// to correct the cache. In the pure-revert case it is the *only* trigger that can
/// fire at all, since `new` is then an empty chain segment with no logs to scan.
///
/// Deliberately coarse: it does not check whether the reverted segment actually
/// contained a `WhitelistUpdated`. Reorgs are rare and a reload is two storage
/// reads, so the unconditional re-read buys **self-healing** — memory that drifted
/// for any reason (a missed notification, an earlier reload that only warned, a
/// race between `latest()` and the notification) is corrected on the next reorg.
/// Filtering on `old` would forfeit that for no meaningful saving.
///
/// A failed refresh is logged and retried on the next notification rather than
/// killing the task — the previous allowlists stay in force meanwhile.
pub async fn run_whitelist_watcher<Pr, N>(
    provider: Pr,
    cfg: Arc<PreconfConfig>,
    classifier: Arc<PreconfClassifier>,
) where
    Pr: CanonStateSubscriptions<Primitives = N> + WhitelistState + 'static,
    N: NodePrimitives,
{
    let Some(contract) = wants_whitelist(&cfg) else {
        debug!(target: "mantle::preconf::whitelist", "whitelist watcher not started");
        return;
    };

    let mut stream = provider.canonical_state_stream();
    info!(target: "mantle::preconf::whitelist", %contract, "whitelist watcher started");

    while let Some(notif) = stream.next().await {
        if !should_reload(&notif, contract) {
            continue;
        }

        if let Err(err) = reload_whitelist(&provider, &cfg, &classifier) {
            warn!(
                target: "mantle::preconf::whitelist",
                %err, %contract, reverted = notif.reverted().is_some(),
                "whitelist refresh failed; keeping previous allowlists",
            );
        }
    }

    debug!(target: "mantle::preconf::whitelist", "whitelist watcher stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_storage_api::errors::ProviderError;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    const WL: Address = Address::new([0xcc; 20]);

    /// Storage words for an `address[]` at `slot`: the length plus one word per
    /// element, laid out the way Solidity does.
    fn array_storage(slot: u64, entries: &[Address]) -> Vec<(B256, U256)> {
        let mut out = vec![(B256::from(U256::from(slot)), U256::from(entries.len()))];
        let base = array_data_base(slot);
        for (i, a) in entries.iter().enumerate() {
            out.push((
                B256::from(base.saturating_add(U256::from(i))),
                U256::from_be_bytes(a.into_word().0),
            ));
        }
        out
    }

    /// Storage words for a `Pair[]` at `slot`. Element `i` occupies **two**
    /// slots — `from` at `base + 2i`, `to` at `base + 2i + 1` — which is the
    /// layout `test_storageLayout_matchesRethExpectations_succeeds` pins with
    /// `vm.load` on the Solidity side.
    fn pair_array_storage(slot: u64, entries: &[(Address, Address)]) -> Vec<(B256, U256)> {
        let mut out = vec![(B256::from(U256::from(slot)), U256::from(entries.len()))];
        let base = array_data_base(slot);
        for (i, (f, t)) in entries.iter().enumerate() {
            let i = i as u64;
            out.push((
                B256::from(base.saturating_add(U256::from(2 * i))),
                U256::from_be_bytes(f.into_word().0),
            ));
            out.push((
                B256::from(base.saturating_add(U256::from(2 * i + 1))),
                U256::from_be_bytes(t.into_word().0),
            ));
        }
        out
    }

    /// Provider with a `PreconfWhitelist` at [`WL`] holding the given allowlist.
    fn provider_with(
        pairs: &[(Address, Address)],
        from_wildcards: &[Address],
        to_wildcards: &[Address],
        with_code: bool,
    ) -> MockEthProvider {
        provider_with_layout(
            pairs,
            from_wildcards,
            to_wildcards,
            with_code,
            EXPECTED_LAYOUT_VERSION,
        )
    }

    /// As [`provider_with`], but declaring an arbitrary layout version. `0` is
    /// the signature of the previous cross-product contract, which never wrote
    /// that slot.
    fn provider_with_layout(
        pairs: &[(Address, Address)],
        from_wildcards: &[Address],
        to_wildcards: &[Address],
        with_code: bool,
        layout: u64,
    ) -> MockEthProvider {
        let mut storage = pair_array_storage(PAIRS_SLOT, pairs);
        storage.extend(array_storage(FROM_WILDCARDS_SLOT, from_wildcards));
        storage.extend(array_storage(TO_WILDCARDS_SLOT, to_wildcards));
        storage.push((B256::from(U256::from(LAYOUT_VERSION_SLOT)), U256::from(layout)));

        let mut account = ExtendedAccount::new(0, U256::ZERO).extend_storage(storage);
        if with_code {
            // Any non-empty code satisfies the has-code check.
            account = account.with_bytecode(alloy_primitives::bytes!("00"));
        }

        let provider = MockEthProvider::default();
        provider.add_account(WL, account);
        provider
    }

    /// Config in on-chain whitelist mode.
    fn cfg_onchain() -> PreconfConfig {
        PreconfConfig { enabled: true, whitelist_contract: Some(WL), ..Default::default() }
    }

    /// On-chain-mode config plus the classifier the loaders write into. Paired
    /// because the loaders read the gating fields from the config and install
    /// the lists on the classifier.
    fn onchain_pair() -> (PreconfConfig, PreconfClassifier) {
        let cfg = cfg_onchain();
        let classifier = PreconfClassifier::from_config(&cfg);
        (cfg, classifier)
    }

    /// Config in `all_preconfs` mode, with an address that must be ignored.
    fn cfg_all_preconfs() -> PreconfConfig {
        PreconfConfig {
            enabled: true,
            all_preconfs: true,
            whitelist_contract: Some(WL),
            ..Default::default()
        }
    }

    /// A `WhitelistState` that always fails, to prove a code path never reads.
    struct ExplodingState;
    impl WhitelistState for ExplodingState {
        fn latest_state(&self) -> Result<StateProviderBox, ProviderError> {
            Err(ProviderError::BestBlockNotFound)
        }
    }

    // ===== slot math (pinned against the Solidity side) =====

    #[test]
    fn list_slots_match_contract_layout() {
        // Declaration order in PreconfWhitelist.sol: pairs(0), pairIndex(1),
        // fromWildcards(2), fromWildcardIndex(3), toWildcards(4),
        // toWildcardIndex(5).
        assert_eq!(PAIRS_SLOT, 0);
        assert_eq!(FROM_WILDCARDS_SLOT, 2);
        assert_eq!(TO_WILDCARDS_SLOT, 4);
    }

    /// The marker sits **after** every array, so bumping it cannot shift the
    /// slots it protects. Pinned against the Solidity side by
    /// `test_storageLayout_layoutVersion_succeeds`.
    #[test]
    fn layout_version_constants_are_pinned() {
        assert_eq!(LAYOUT_VERSION_SLOT, 6);
        assert_eq!(EXPECTED_LAYOUT_VERSION, 2);
        const {
            assert!(
                LAYOUT_VERSION_SLOT > TO_WILDCARDS_SLOT,
                "the marker must be appended past the arrays, or bumping it moves them",
            )
        };
    }

    /// **The skew this check exists for.** The previous cross-product contract
    /// never wrote the marker, so it reads back as `0`.
    ///
    /// Without the check, this same state would load: slot 0's length would be
    /// taken as a pair count and read with a two-slot stride, and slot 2 — that
    /// contract's *recipient* list — would be installed as **sender wildcards**,
    /// making every transaction from a former recipient preconf-eligible. Fatal
    /// is the right answer, and for the same reason `ContractHasNoCode` is: what
    /// is wrong is this node's own configuration.
    #[test]
    fn bootstrap_refuses_a_contract_from_the_previous_layout() {
        let provider = provider_with_layout(&[(addr(1), addr(2))], &[addr(3)], &[], true, 0);
        let (cfg, c) = onchain_pair();

        let err = bootstrap_whitelist(&provider, &cfg, &c).unwrap_err();
        assert!(
            matches!(
                err,
                WhitelistError::LayoutVersionMismatch { contract, expected: 2, found: 0 }
                    if contract == WL
            ),
            "got {err:?}",
        );
        assert_eq!(c.whitelist_counts(), (0, 0, 0), "and nothing was loaded from it");
    }

    /// A *future* layout is refused too. The comparison is exact rather than a
    /// minimum: a later version moves slots, so this binary has no business
    /// reading it either.
    #[test]
    fn bootstrap_refuses_a_newer_layout() {
        let provider = provider_with_layout(&[], &[], &[], true, 3);
        let (cfg, c) = onchain_pair();

        let err = bootstrap_whitelist(&provider, &cfg, &c).unwrap_err();
        assert!(
            matches!(err, WhitelistError::LayoutVersionMismatch { found: 3, .. }),
            "got {err:?}",
        );
    }

    /// The marker is checked **once**, at cold start, exactly like the has-code
    /// check: it cannot change without a redeploy, and a redeploy means a new
    /// address and a new `--preconf.whitelist-contract`. Re-asking on every
    /// notification would only spend a storage read on a fixed answer.
    #[test]
    fn reload_does_not_recheck_the_layout_version() {
        let provider = provider_with_layout(&[(addr(1), addr(2))], &[], &[], true, 0);
        let (cfg, c) = onchain_pair();

        reload_whitelist(&provider, &cfg, &c).expect("reload must not re-validate the layout");
        assert_eq!(c.whitelist_counts(), (1, 0, 0));
    }

    #[test]
    fn array_data_base_matches_keccak_of_slot() {
        // Same values asserted from Solidity via keccak256(abi.encode(uint256 slot)).
        assert_eq!(
            B256::from(array_data_base(0)),
            alloy_primitives::b256!(
                "290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563"
            )
        );
        assert_eq!(
            B256::from(array_data_base(2)),
            alloy_primitives::b256!(
                "405787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace"
            )
        );
    }

    #[test]
    fn whitelist_updated_topic0_matches_event_signature() {
        assert_eq!(
            keccak256("WhitelistUpdated(uint256,uint256,uint256)"),
            WHITELIST_UPDATED_TOPIC0
        );
    }

    // ===== read_preconf_set =====

    #[test]
    fn read_preconf_set_reads_all_entries() {
        let provider = provider_with(&[], &[addr(1), addr(2)], &[addr(3)], true);
        let state = provider.latest_state().unwrap();

        let from = read_preconf_set(&state, WL, FROM_WILDCARDS_SLOT).unwrap();
        assert_eq!(from, HashSet::from_iter([addr(1), addr(2)]));

        let to = read_preconf_set(&state, WL, TO_WILDCARDS_SLOT).unwrap();
        assert_eq!(to, HashSet::from_iter([addr(3)]));
    }

    #[test]
    fn read_preconf_set_empty_list_is_empty_not_error() {
        let provider = provider_with(&[], &[], &[], true);
        let state = provider.latest_state().unwrap();
        assert!(read_preconf_set(&state, WL, FROM_WILDCARDS_SLOT).unwrap().is_empty());
        assert!(read_preconf_pairs(&state, WL, PAIRS_SLOT).unwrap().is_empty());
    }

    #[test]
    fn read_preconf_set_skips_zero_entries() {
        // The contract stores no zero addresses at all now — the zero address is
        // a calldata-only marker that routes a rule to a wildcard set. So a zero
        // here means the layout is wrong, and admitting it would invent a rule.
        let provider = provider_with(&[], &[addr(1), Address::ZERO], &[], true);
        let state = provider.latest_state().unwrap();
        let from = read_preconf_set(&state, WL, FROM_WILDCARDS_SLOT).unwrap();
        assert_eq!(from, HashSet::from_iter([addr(1)]));
    }

    // ===== read_preconf_pairs =====

    /// The two-slot stride is the whole difference from `read_preconf_set`, and
    /// getting it wrong reads `pairs[1].from` where `pairs[0].to` should be —
    /// which would not error, just silently authorize the wrong traffic. Three
    /// pairs, so an off-by-one in the stride cannot coincidentally line up.
    #[test]
    fn read_preconf_pairs_decodes_the_two_slot_stride() {
        let want = [(addr(1), addr(2)), (addr(3), addr(4)), (addr(5), addr(6))];
        let provider = provider_with(&want, &[], &[], true);
        let state = provider.latest_state().unwrap();

        let got = read_preconf_pairs(&state, WL, PAIRS_SLOT).unwrap();
        assert_eq!(got, HashSet::from_iter(want));
    }

    /// Direction is part of the rule: `(A, B)` must not authorize `B -> A`.
    #[test]
    fn read_preconf_pairs_keeps_direction() {
        let provider = provider_with(&[(addr(1), addr(2))], &[], &[], true);
        let state = provider.latest_state().unwrap();

        let got = read_preconf_pairs(&state, WL, PAIRS_SLOT).unwrap();
        assert!(got.contains(&(addr(1), addr(2))));
        assert!(!got.contains(&(addr(2), addr(1))));
    }

    /// A pair with either half zero is discarded **whole**. Keeping the non-zero
    /// half would turn a corrupt read into an authorization nobody wrote.
    #[test]
    fn read_preconf_pairs_discards_a_half_zero_pair() {
        let provider = provider_with(
            &[(addr(1), addr(2)), (Address::ZERO, addr(4)), (addr(5), Address::ZERO)],
            &[],
            &[],
            true,
        );
        let state = provider.latest_state().unwrap();

        let got = read_preconf_pairs(&state, WL, PAIRS_SLOT).unwrap();
        assert_eq!(got, HashSet::from_iter([(addr(1), addr(2))]));
    }

    /// **The budget is spent in storage reads, not array elements.** A pair costs
    /// two reads, so a budget that expires mid-element must still abort — with
    /// element-based counting this would sail past the check and only notice at
    /// the next element boundary.
    #[test]
    fn read_preconf_pairs_charges_the_budget_per_slot_read() {
        let provider = provider_with(&[(addr(1), addr(2))], &[], &[], true);
        let state = provider.latest_state().unwrap();

        let err = read_preconf_pairs_within(&state, WL, PAIRS_SLOT, Duration::ZERO).unwrap_err();
        assert!(
            matches!(err, WhitelistError::ReadBudgetExceeded { read: 0, claimed_len: 1, .. }),
            "got {err:?}",
        );
    }

    /// [`ReadBudget`]'s own contract: one charge per **storage read**, checked on
    /// a stride, and a zero budget does no work at all.
    ///
    /// This is where the counting unit is pinned. What it does *not* pin is that
    /// the `Pair[]` loop charges twice per element — see the note there. That
    /// wiring is arithmetic rather than behaviour: getting it wrong only doubles
    /// the stride, i.e. moves the check from every ~4ms to every ~8ms, which no
    /// deterministic test can observe without a fake clock.
    #[test]
    fn read_budget_charges_per_read_and_a_zero_budget_does_no_work() {
        let mut spent = ReadBudget::new(Duration::ZERO);
        assert!(spent.charge().is_some(), "a zero budget must abort before the first read");

        let mut ample = ReadBudget::new(Duration::from_secs(3600));
        let n = BUDGET_CHECK_STRIDE * 2 + 1;
        for _ in 0..n {
            assert!(ample.charge().is_none());
        }
        assert_eq!(ample.reads, n, "every charge is one read");
    }

    /// **The list length is not a limit.** A length far above
    /// [`WHITELIST_WARN_THRESHOLD`] must still be read, because how long the
    /// allowlist is, is governance's decision — this node only bounds the *time*
    /// spent reading it.
    ///
    /// Reads 1.2M entries (20% past the warn threshold) from the mock. Costs
    /// ~50ms: the mock's storage is a `HashMap`, missing keys return `None`
    /// cheaply and zero addresses are filtered, so this doubles as a smoke test
    /// that the loop stays linear well past the advisory threshold.
    #[test]
    fn a_list_past_the_warn_threshold_is_still_read() {
        let over = WHITELIST_WARN_THRESHOLD as u64 * 12 / 10;
        let account = ExtendedAccount::new(0, U256::ZERO)
            .extend_storage([(B256::from(U256::from(FROM_WILDCARDS_SLOT)), U256::from(over))]);
        let provider = MockEthProvider::default();
        provider.add_account(WL, account);
        let state = provider.latest_state().unwrap();

        // Generous budget: the point is that *length* does not reject.
        let got =
            read_preconf_set_within(&state, WL, FROM_WILDCARDS_SLOT, Duration::from_secs(3600));
        assert!(got.is_ok(), "an over-threshold list must still load, got {got:?}");
    }

    /// A nonsense length — the signature of an address that is not a
    /// `PreconfWhitelist` — must fail in **bounded** time rather than hang the
    /// node. At `u64::MAX` and ~1 µs/entry an unbounded loop would run for
    /// ~620,000 years.
    ///
    /// A zero budget is used so the test is deterministic and instant; the
    /// production budget is [`WHITELIST_READ_BUDGET`].
    #[test]
    fn a_nonsense_length_is_abandoned_within_the_budget() {
        let account = ExtendedAccount::new(0, U256::ZERO)
            .extend_storage([(B256::from(U256::from(FROM_WILDCARDS_SLOT)), U256::from(u64::MAX))]);
        let provider = MockEthProvider::default();
        provider.add_account(WL, account);
        let state = provider.latest_state().unwrap();

        let err =
            read_preconf_set_within(&state, WL, FROM_WILDCARDS_SLOT, Duration::ZERO).unwrap_err();
        match err {
            WhitelistError::ReadBudgetExceeded { claimed_len, read, .. } => {
                assert_eq!(claimed_len, u64::MAX, "the claimed length belongs in the error");
                assert_eq!(read, 0, "a zero budget must abandon before reading anything");
            }
            other => panic!("expected ReadBudgetExceeded, got {other:?}"),
        }
    }

    /// A U256 length beyond `u64` saturates rather than wrapping — wrapping could
    /// turn an absurd length into a small plausible one and load garbage
    /// addresses silently.
    #[test]
    fn an_over_u64_length_saturates_and_is_abandoned() {
        let account = ExtendedAccount::new(0, U256::ZERO)
            .extend_storage([(B256::from(U256::from(FROM_WILDCARDS_SLOT)), U256::MAX)]);
        let provider = MockEthProvider::default();
        provider.add_account(WL, account);
        let state = provider.latest_state().unwrap();

        let err =
            read_preconf_set_within(&state, WL, FROM_WILDCARDS_SLOT, Duration::ZERO).unwrap_err();
        assert!(
            matches!(err, WhitelistError::ReadBudgetExceeded { claimed_len: u64::MAX, .. }),
            "got {err:?}",
        );
    }

    // ===== bootstrap =====

    #[test]
    fn bootstrap_populates_classifier() {
        let provider = provider_with(&[(addr(1), addr(2))], &[addr(3)], &[addr(4)], true);
        let (cfg, c) = onchain_pair();
        bootstrap_whitelist(&provider, &cfg, &c).unwrap();

        assert_eq!(c.whitelist_counts(), (1, 1, 1));
        // All three routes, and one miss.
        assert!(c.preview_eligibility(&addr(1), Some(&addr(2))), "exact pair");
        assert!(c.preview_eligibility(&addr(3), Some(&addr(9))), "from wildcard");
        assert!(c.preview_eligibility(&addr(9), Some(&addr(4))), "to wildcard");
        assert!(!c.preview_eligibility(&addr(9), Some(&addr(8))), "no rule");
    }

    #[test]
    fn bootstrap_rejects_address_with_no_code() {
        let provider = provider_with(&[(addr(1), addr(2))], &[], &[], false);
        let (cfg, c) = onchain_pair();

        let err = bootstrap_whitelist(&provider, &cfg, &c).unwrap_err();
        assert!(matches!(err, WhitelistError::ContractHasNoCode(a) if a == WL), "got {err:?}");
        // Nothing was loaded from a wrong address.
        assert_eq!(c.whitelist_counts(), (0, 0, 0));
    }

    #[test]
    fn bootstrap_rejects_absent_account() {
        // Nothing at all at that address — same failure as "no code".
        let provider = MockEthProvider::default();
        let (cfg, c) = onchain_pair();
        let err = bootstrap_whitelist(&provider, &cfg, &c).unwrap_err();
        assert!(matches!(err, WhitelistError::ContractHasNoCode(_)), "got {err:?}");
    }

    #[test]
    fn bootstrap_accepts_empty_allowlists() {
        // Empty lists are governance's decision, not a misconfiguration: a
        // deployed contract that currently allows nobody must NOT stop the node.
        // Regression guard — making this fatal would let governance brick the
        // sequencer's ability to restart.
        let provider = provider_with(&[], &[], &[], true);
        let (cfg, c) = onchain_pair();
        bootstrap_whitelist(&provider, &cfg, &c).expect("empty allowlists must not fail startup");
        assert_eq!(c.whitelist_counts(), (0, 0, 0));
        assert!(!c.preview_eligibility(&addr(1), Some(&addr(2))));
    }

    /// Eligibility is a three-way OR now, so **one** populated set is a complete,
    /// working allowlist. The old shape needed a hit on both lists and so warned
    /// whenever either was empty; carrying that rule over would fire on every
    /// healthy pairs-only configuration.
    #[test]
    fn bootstrap_accepts_a_pairs_only_allowlist() {
        let provider = provider_with(&[(addr(1), addr(2))], &[], &[], true);
        let (cfg, c) = onchain_pair();
        bootstrap_whitelist(&provider, &cfg, &c).expect("a pairs-only allowlist is complete");
        assert_eq!(c.whitelist_counts(), (1, 0, 0));
        assert!(c.preview_eligibility(&addr(1), Some(&addr(2))));
    }

    #[test]
    fn reload_accepts_emptying_the_allowlists() {
        // Governance may legitimately drain the lists at runtime; the reload must
        // apply that faithfully rather than keeping stale entries alive.
        let (cfg, c) = onchain_pair();
        c.update_whitelist(
            HashSet::from_iter([(addr(1), addr(2))]),
            HashSet::default(),
            HashSet::default(),
        );
        assert!(c.preview_eligibility(&addr(1), Some(&addr(2))));

        let provider = provider_with(&[], &[], &[], true);
        reload_whitelist(&provider, &cfg, &c).expect("draining must not error");
        assert_eq!(c.whitelist_counts(), (0, 0, 0));
        assert!(!c.preview_eligibility(&addr(1), Some(&addr(2))));
    }

    #[test]
    fn bootstrap_is_noop_when_disabled() {
        // ExplodingState proves state is never touched.
        let cfg = PreconfConfig::default();
        assert!(!cfg.enabled);
        let c = PreconfClassifier::from_config(&cfg);
        bootstrap_whitelist(&ExplodingState, &cfg, &c).unwrap();
    }

    #[test]
    fn bootstrap_is_noop_in_all_preconfs_mode() {
        // Even with an address configured, all_preconfs skips the contract.
        let cfg = cfg_all_preconfs();
        let c = PreconfClassifier::from_config(&cfg);
        bootstrap_whitelist(&ExplodingState, &cfg, &c).unwrap();
        assert_eq!(c.whitelist_counts(), (0, 0, 0));
    }

    #[test]
    fn bootstrap_is_noop_without_address() {
        // No whitelist_contract — validate() would have rejected this, but the
        // reader must not panic or read state regardless.
        let cfg = PreconfConfig { enabled: true, ..Default::default() };
        let c = PreconfClassifier::from_config(&cfg);
        bootstrap_whitelist(&ExplodingState, &cfg, &c).unwrap();
    }

    // ===== reload =====

    #[test]
    fn reload_replaces_previous_sets() {
        let (cfg, c) = onchain_pair();
        c.update_whitelist(
            HashSet::from_iter([(addr(8), addr(7))]),
            HashSet::from_iter([addr(9)]),
            HashSet::default(),
        );

        let provider = provider_with(&[(addr(1), addr(2))], &[], &[], true);
        reload_whitelist(&provider, &cfg, &c).unwrap();

        // Wholesale replacement, not a union with the stale generation.
        assert_eq!(c.whitelist_counts(), (1, 0, 0));
        assert!(c.preview_eligibility(&addr(1), Some(&addr(2))));
        assert!(!c.preview_eligibility(&addr(8), Some(&addr(7))), "stale pair is gone");
        assert!(!c.preview_eligibility(&addr(9), Some(&addr(7))), "stale wildcard is gone");
    }

    #[test]
    fn reload_leaves_sets_intact_on_provider_error() {
        let (cfg, c) = onchain_pair();
        c.update_whitelist(
            HashSet::from_iter([(addr(1), addr(2))]),
            HashSet::default(),
            HashSet::default(),
        );

        let err = reload_whitelist(&ExplodingState, &cfg, &c).unwrap_err();
        assert!(matches!(err, WhitelistError::Provider(_)), "got {err:?}");
        // A failed refresh must not degrade into an empty allowlist.
        assert_eq!(c.whitelist_counts(), (1, 0, 0));
        assert!(c.preview_eligibility(&addr(1), Some(&addr(2))));
    }

    #[test]
    fn reload_skips_contract_when_all_preconfs() {
        let cfg = cfg_all_preconfs();
        let c = PreconfClassifier::from_config(&cfg);
        reload_whitelist(&ExplodingState, &cfg, &c).unwrap();
    }

    // ===== event detection =====

    mod event {
        use super::*;
        use alloy_consensus::Receipt;
        use alloy_primitives::Log;
        use reth_optimism_primitives::{OpPrimitives, OpReceipt};

        /// Chain carrying a single receipt with the given logs.
        fn chain_with_logs(logs: Vec<Log>) -> Chain<OpPrimitives> {
            let receipt =
                OpReceipt::Legacy(Receipt { status: true.into(), cumulative_gas_used: 0, logs });
            let mut chain = Chain::<OpPrimitives>::default();
            chain.execution_outcome_mut().receipts.push(vec![receipt]);
            chain
        }

        fn log(address: Address, topic0: B256) -> Log {
            Log::new_unchecked(address, vec![topic0], Default::default())
        }

        #[test]
        fn detects_matching_log() {
            let chain = chain_with_logs(vec![log(WL, WHITELIST_UPDATED_TOPIC0)]);
            assert!(has_whitelist_event(&chain, WL));
        }

        #[test]
        fn ignores_right_topic_from_other_address() {
            let chain = chain_with_logs(vec![log(addr(1), WHITELIST_UPDATED_TOPIC0)]);
            assert!(!has_whitelist_event(&chain, WL));
        }

        #[test]
        fn ignores_other_event_from_whitelist() {
            let chain = chain_with_logs(vec![log(WL, keccak256("SomethingElse()"))]);
            assert!(!has_whitelist_event(&chain, WL));
        }

        #[test]
        fn ignores_empty_chain() {
            assert!(!has_whitelist_event(&Chain::<OpPrimitives>::default(), WL));
        }

        #[test]
        fn finds_log_among_others() {
            let chain = chain_with_logs(vec![
                log(addr(1), keccak256("Transfer(address,address,uint256)")),
                log(WL, WHITELIST_UPDATED_TOPIC0),
            ]);
            assert!(has_whitelist_event(&chain, WL));
        }

        // ===== the watcher's trigger decision =====
        //
        // Driven through real `CanonStateNotification`s. `MockEthProvider` cannot
        // emit any (its `subscribe_to_canonical_state` drops the sender), so the
        // decision lives in `should_reload` and is exercised directly.

        fn updated_chain() -> Arc<Chain<OpPrimitives>> {
            Arc::new(chain_with_logs(vec![log(WL, WHITELIST_UPDATED_TOPIC0)]))
        }

        fn quiet_chain() -> Arc<Chain<OpPrimitives>> {
            Arc::new(chain_with_logs(vec![log(addr(1), keccak256("Unrelated()"))]))
        }

        #[test]
        fn commit_with_update_triggers_reload() {
            let notif = CanonStateNotification::Commit { new: updated_chain() };
            assert!(should_reload(&notif, WL));
        }

        #[test]
        fn commit_without_update_does_not_trigger_reload() {
            // The common case: ordinary blocks must not cause storage reads.
            let notif = CanonStateNotification::Commit { new: quiet_chain() };
            assert!(!should_reload(&notif, WL));
        }

        #[test]
        fn pure_revert_triggers_reload_despite_having_no_logs_to_scan() {
            // THE case the `reverted` branch exists for. A revert rolls back the
            // block that carried an update; `new` is an empty chain segment, so
            // there is no log anywhere that says the update went away. Detecting
            // it via events is impossible — only the reorg itself is observable.
            let notif = CanonStateNotification::Reorg {
                old: updated_chain(),
                new: Arc::new(Chain::<OpPrimitives>::default()),
            };
            assert!(
                should_reload(&notif, WL),
                "a reverted update must force a re-read; otherwise the orphaned entry \
                 stays live in memory with no later event to correct it",
            );
        }

        #[test]
        fn reorg_triggers_reload_even_with_no_whitelist_activity() {
            // Deliberately coarse: any reorg re-reads, which is what makes the
            // cache self-healing after a missed notification or a failed reload.
            let notif = CanonStateNotification::Reorg { old: quiet_chain(), new: quiet_chain() };
            assert!(should_reload(&notif, WL));
        }

        #[test]
        fn reorg_carrying_a_new_update_triggers_reload() {
            let notif = CanonStateNotification::Reorg { old: quiet_chain(), new: updated_chain() };
            assert!(should_reload(&notif, WL));
        }

        #[test]
        fn other_contracts_reorg_still_triggers_reload() {
            // The reorg branch is contract-agnostic on purpose — we cannot know
            // whether our contract's state moved without re-reading it.
            let notif = CanonStateNotification::Reorg {
                old: Arc::new(chain_with_logs(vec![log(addr(1), WHITELIST_UPDATED_TOPIC0)])),
                new: Arc::new(Chain::<OpPrimitives>::default()),
            };
            assert!(should_reload(&notif, WL));
        }
    }
}

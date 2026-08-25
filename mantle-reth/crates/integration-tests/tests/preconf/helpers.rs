//! Shared helpers for `preconf/*` integration tests.

use alloy_genesis::Genesis;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B64, B256, Bytes, TxKind, U256, address, hex, keccak256};
use alloy_rpc_types_engine::PayloadAttributes;
use jsonrpsee::{core::client::ClientT, http_client::HttpClient};
use mantle_reth_preconf::PreconfConfig;
use mantle_reth_rpc_ext::PreconfTxEvent;
use op_alloy_consensus::TxDeposit;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::payload::OpPayloadAttrs;
use std::{path::PathBuf, sync::Arc};

/// Mantle-flavoured chainspec used across preconf tests. Reuses the
/// same genesis fixture as the top-level integration tests. Defaults to
/// Mantle mainnet (`chainId=5000`).
pub fn mantle_test_chain_spec() -> Arc<OpChainSpec> {
    mantle_chain_spec_for(5000)
}

/// EIP-1559 parameters as the built blocks encode them: version 1, denominator
/// 8, elasticity 2, then Mantle's trailing bytes.
///
/// The shipped fixture has `extraData: 0x00`, which encodes no parameters at all. Since
/// `newPayload` derives a block's expected base fee by decoding the parameters off its
/// **parent**, anything built on genesis was rejected `Invalid { "base fee missing" }` and
/// the head silently stayed at 0 — the root cause of this suite's long-running flakiness.
/// Giving genesis the same encoding its children use makes the parent decode succeed,
/// which is what [`canonicalize_payload!`] relies on.
const GENESIS_EIP1559_EXTRA_DATA: &str = "0x0100000008000000020000000000000000";

/// The shared genesis fixture with `chainId` patched and
/// [`GENESIS_EIP1559_EXTRA_DATA`] installed.
///
/// Shared by every spec helper below so no test can accidentally build on a
/// genesis that cannot be a valid parent.
fn patched_genesis_value(chain_id: u64) -> serde_json::Value {
    let raw = include_str!("../assets/genesis.json");
    let mut value: serde_json::Value = serde_json::from_str(raw).expect("valid genesis JSON");
    value["config"]["chainId"] = serde_json::Value::from(chain_id);
    value["extraData"] = serde_json::Value::from(GENESIS_EIP1559_EXTRA_DATA);
    value
}

/// Build a Mantle-flavoured `OpChainSpec` bound to the given L2 chain id.
///
/// The base fixture is Mantle mainnet's `assets/genesis.json` (all Mantle
/// hardforks at timestamp 0), with `chainId` patched and genesis given valid
/// EIP-1559 parameters (see [`patched_genesis_value`]). Every other invariant
/// (fork schedule, allocations) stays identical across networks — the only
/// difference on the L2 side is the id used for signature recovery and network
/// identification.
///
/// Supported ids for the pair-test matrix:
/// - `5000` — Mantle Mainnet
/// - `5003` — Mantle Sepolia
/// - `50002` — Mantle Hoodi
pub fn mantle_chain_spec_for(chain_id: u64) -> Arc<OpChainSpec> {
    let genesis: Genesis =
        serde_json::from_value(patched_genesis_value(chain_id)).expect("genesis deserialises");
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

/// Build a Mantle-flavoured `OpChainSpec` whose alloc includes the full
/// set of L2 predeploys (proxies + implementations + storage slots).
///
/// Composition:
/// - **Header + config** come entirely from `assets/genesis.json` (the same file
///   `mantle_chain_spec_for` uses) — single source of truth for chain-config, hardfork timeline,
///   EIP-1559 params and header fields. `chainId` is patched to the caller-supplied value.
/// - **Alloc** is the merge of `genesis.json`'s Hardhat-mnemonic EOAs and
///   `assets/predeploys.json`'s contract entries. `predeploys.json` is filtered from op-chain-ops'
///   `BuildL2DeveloperGenesis` dump (kept in `src/mantle-v2/.devnet/genesis-l2.json`) to include
///   only entries carrying `code` — every proxy + implementation + pre-configured storage slot ends
///   up here, and EOA balances are left to `genesis.json` so both helpers share the same fund
///   state.
///
/// This split guarantees that `mantle_chain_spec_for` and
/// `mantle_chain_spec_with_predeploys_for` differ **only** by the
/// presence of L2 predeploys — every other parameter (hardfork times,
/// gas limit, base fee, extra data, coinbase, EIP-1559 curves) is bit-
/// identical, so any behavioural difference observed between the two
/// helpers can only come from predeploy state.
///
/// Regeneration procedure for `predeploys.json`: rerun `task init-l2`,
/// then extract contract entries with:
/// ```sh
/// python3 -c 'import json; g=json.load(open("src/mantle-v2/.devnet/genesis-l2.json"));\
///   json.dump({"alloc":{k:v for k,v in g["alloc"].items() if "code" in v}},\
///   open("src/reth/mantle-reth/crates/integration-tests/tests/assets/predeploys.json","w"), indent=2)'
/// ```
pub fn mantle_chain_spec_with_predeploys_for(chain_id: u64) -> Arc<OpChainSpec> {
    let predeploys_raw = include_str!("../assets/predeploys.json");
    let mut base = patched_genesis_value(chain_id);

    let predeploys: serde_json::Value =
        serde_json::from_str(predeploys_raw).expect("valid predeploys JSON");
    if let Some(pre_alloc) = predeploys.get("alloc").and_then(|v| v.as_object()) {
        let base_alloc =
            base["alloc"].as_object_mut().expect("genesis.json `alloc` must be an object");
        for (k, v) in pre_alloc {
            base_alloc.insert(k.clone(), v.clone());
        }
    }

    let genesis: Genesis = serde_json::from_value(base).expect("merged genesis deserialises");
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

/// Builds a chainspec whose genesis allocates `address` with `code` and
/// `storage`, on top of [`mantle_chain_spec_for`].
///
/// Used by the on-chain whitelist suite to stand up a `PreconfWhitelist`-shaped
/// account without deploying anything: the sequencer only ever reads that
/// contract's storage, so pre-seeded slots are indistinguishable from slots a
/// real `updatePreconfs` wrote.
pub fn mantle_chain_spec_with_account(
    chain_id: u64,
    address: Address,
    code: &Bytes,
    storage: &[(B256, B256)],
) -> Arc<OpChainSpec> {
    let mut base = patched_genesis_value(chain_id);

    let storage_map: serde_json::Map<String, serde_json::Value> = storage
        .iter()
        .map(|(slot, value)| (format!("{slot:#x}"), serde_json::Value::from(format!("{value:#x}"))))
        .collect();

    let alloc = base["alloc"].as_object_mut().expect("genesis.json `alloc` must be an object");
    alloc.insert(
        format!("{address:#x}"),
        serde_json::json!({
            "balance": "0x0",
            "code": format!("{code:#x}"),
            "storage": serde_json::Value::Object(storage_map),
        }),
    );

    let genesis: Genesis = serde_json::from_value(base).expect("patched genesis deserialises");
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

/// Storage words laying out `entries` as a Solidity `address[]` declared at
/// `slot`: the length at `slot`, element `i` at `keccak256(slot) + i`.
///
/// Mirrors `PreconfWhitelist`'s layout so tests can seed a list — or assert one —
/// the way the contract would have written it. The layout itself is pinned from
/// both sides (`whitelist.rs` slot-base unit tests and
/// `test/PreconfWhitelist.t.sol`'s `vm.load` assertions).
pub fn address_array_storage(slot: u64, entries: &[Address]) -> Vec<(B256, B256)> {
    let mut out = vec![(B256::from(U256::from(slot)), B256::from(U256::from(entries.len())))];
    let base = U256::from_be_bytes(alloy_primitives::keccak256(B256::from(U256::from(slot))).0);
    for (i, entry) in entries.iter().enumerate() {
        out.push((B256::from(base.saturating_add(U256::from(i))), entry.into_word()));
    }
    out
}

/// Storage words laying out `entries` as a Solidity `Rule[]` declared at `slot`.
///
/// A `Rule` is two `address` fields — 40 bytes — so it cannot pack into one
/// slot: element `i` occupies `keccak256(slot) + 2i` (`from`) and
/// `keccak256(slot) + 2i + 1` (`to`). That stride is pinned from both sides
/// (`whitelist::tests::read_preconf_pairs_decodes_the_two_slot_stride` and
/// `test/PreconfWhitelist.t.sol`'s `vm.load` assertions).
pub fn pair_array_storage(slot: u64, entries: &[(Address, Address)]) -> Vec<(B256, B256)> {
    let mut out = vec![(B256::from(U256::from(slot)), B256::from(U256::from(entries.len())))];
    let base = U256::from_be_bytes(alloy_primitives::keccak256(B256::from(U256::from(slot))).0);
    for (i, (from, to)) in entries.iter().enumerate() {
        let i = i as u64;
        out.push((B256::from(base.saturating_add(U256::from(2 * i))), from.into_word()));
        out.push((B256::from(base.saturating_add(U256::from(2 * i + 1))), to.into_word()));
    }
    out
}

/// The one storage word that declares which layout a `PreconfWhitelist` was
/// deployed with.
///
/// Every fixture that stands one up has to include it: cold start refuses an
/// address whose marker does not match the binary, which is the point — reading
/// the previous layout would install that contract's recipient list as sender
/// wildcards. A fixture that forgets it looks exactly like a version skew, and
/// fails the same way.
pub fn layout_version_storage() -> (B256, B256) {
    (
        B256::from(U256::from(mantle_reth_preconf::LAYOUT_VERSION_SLOT)),
        B256::from(U256::from(mantle_reth_preconf::EXPECTED_LAYOUT_VERSION)),
    )
}

/// Assembles minimal EVM bytecode that performs `writes` then emits a single
/// log with `topic` and no data.
///
/// Why hand-assembled rather than a compiled contract: the sequencer's watcher
/// only reacts to two observable things — the storage at the whitelist's array
/// slots, and a log whose `address` and `topics[0]` match — so this is the whole
/// surface reth cares about. Producing it here keeps the suite free of any
/// cross-repo Solidity build step. The real contract's own behaviour (auth gates,
/// idempotence, batch cap) is covered by the forge tests in
/// `mantle-v2/packages/contracts-bedrock/test/PreconfWhitelist.t.sol`.
///
/// Emits, per write, `PUSH32 <value> PUSH32 <slot> SSTORE`, then
/// `PUSH32 <topic> PUSH1 0 PUSH1 0 LOG1` and `STOP`.
pub fn storage_writer_bytecode(writes: &[(B256, B256)], topic: B256) -> Bytes {
    const PUSH32: u8 = 0x7f;
    const PUSH1: u8 = 0x60;
    const SSTORE: u8 = 0x55;
    const LOG1: u8 = 0xa1;
    const STOP: u8 = 0x00;

    let mut code = Vec::new();
    for (slot, value) in writes {
        code.push(PUSH32);
        code.extend_from_slice(value.as_slice());
        code.push(PUSH32);
        code.extend_from_slice(slot.as_slice());
        code.push(SSTORE);
    }
    // LOG1 pops (offset, size, topic) — zero-length data needs no memory.
    code.push(PUSH32);
    code.extend_from_slice(topic.as_slice());
    code.push(PUSH1);
    code.push(0);
    code.push(PUSH1);
    code.push(0);
    code.push(LOG1);
    code.push(STOP);
    code.into()
}

/// Like [`mantle_chain_spec_for`] but with an **always-reverting** contract at
/// `addr` (runtime code `0x60006000fd` = `PUSH1 0; PUSH1 0; REVERT`). A tx to
/// `addr` is valid but its execution reverts, so it still lands with an EIP-658
/// `status = 0` receipt — the shape needed to exercise journaling of a
/// reverted-but-sealed tx (a plain transfer always succeeds, so the happy-path
/// helpers can't produce it).
pub fn mantle_chain_spec_with_reverting_contract(chain_id: u64, addr: Address) -> Arc<OpChainSpec> {
    let raw = include_str!("../assets/genesis.json");
    let mut value: serde_json::Value = serde_json::from_str(raw).expect("valid genesis JSON");
    value["config"]["chainId"] = serde_json::Value::from(chain_id);
    let alloc = value["alloc"].as_object_mut().expect("genesis.json `alloc` must be an object");
    // Runtime bytecode only (genesis `code` is the deployed code, not initcode).
    alloc.insert(
        format!("{addr:#x}"),
        serde_json::json!({ "code": "0x60006000fd", "balance": "0x0" }),
    );
    let genesis: Genesis = serde_json::from_value(value).expect("patched genesis deserialises");
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

/// Payload attributes generator for Mantle test chains — matches the
/// shared helper in `tests/helpers.rs` but scoped to this test binary.
pub fn mantle_payload_attributes(timestamp: u64) -> OpPayloadAttrs {
    OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
        },
        transactions: None,
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(0),
    })
}

/// Stand-in `PreconfWhitelist` address for tests that do not care about the
/// contract itself, only about which `(from, to)` pairs are allowlisted.
///
/// `launch_preconf_node!` allocates a minimal coded account here and writes the
/// builder's lists into its storage in the exact layout the real contract uses,
/// so cold start loads them through the production path
/// (`bootstrap_whitelist`) rather than having them injected into memory. Two
/// reasons that matters now that cold start runs inside `build_pool`:
///
/// * an address with **no code** is fatal by design, so every suite would refuse to boot;
/// * cold start (and any later watcher reload) overwrite the in-memory lists, so an in-memory seed
///   would simply be erased.
///
/// Tests that point `whitelist_contract` at a real address own that address's
/// genesis themselves and are left untouched — see
/// [`PreconfCfgBuilder::whitelist_contract`].
pub const WHITELIST_CONTRACT_SENTINEL: Address = Address::new([0x77; 20]);

/// Minimal bytecode for the sentinel: a single `STOP`. Nothing ever calls it; it exists
/// only to satisfy the has-code check described on [`WHITELIST_CONTRACT_SENTINEL`].
const SENTINEL_CODE: [u8; 1] = [0x00];

/// Returns `spec` with the sentinel account allocated and holding the given
/// allowlist — exact pairs plus the two wildcard sets — in `PreconfWhitelist`'s
/// storage layout.
///
/// Rebuilds the chain spec from `spec`'s own genesis plus one extra alloc entry,
/// so it composes with every spec helper here (plain, predeploys, custom
/// account) without those needing to know about preconf.
pub fn with_sentinel_whitelist(
    spec: Arc<OpChainSpec>,
    pairs: &[(Address, Address)],
    from_wildcards: &[Address],
    to_wildcards: &[Address],
) -> Arc<OpChainSpec> {
    use alloy_genesis::GenesisAccount;
    use reth_chainspec::EthChainSpec;

    // Same constants production reads, so a layout change breaks both sides
    // together instead of silently diverging.
    let mut storage = pair_array_storage(mantle_reth_preconf::PAIRS_SLOT, pairs);
    storage.extend(address_array_storage(mantle_reth_preconf::FROM_WILDCARDS_SLOT, from_wildcards));
    storage.extend(address_array_storage(mantle_reth_preconf::TO_WILDCARDS_SLOT, to_wildcards));
    storage.push(layout_version_storage());

    let mut genesis = spec.genesis().clone();
    genesis.alloc.insert(
        WHITELIST_CONTRACT_SENTINEL,
        GenesisAccount::default()
            .with_code(Some(Bytes::from_static(&SENTINEL_CODE)))
            .with_storage(Some(storage.into_iter().collect())),
    );
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

/// Fluent builder for `PreconfConfig` inside tests. Every knob has a
/// safe default; individual test cases tweak only what they exercise.
///
/// Decoupled from `PreconfConfig::default` so a future production
/// default change (e.g. `preconf_timeout` bumped fleet-wide) does not
/// silently retune the integration tests.
#[derive(Debug, Clone)]
pub struct PreconfCfgBuilder {
    pairs: Vec<(Address, Address)>,
    from: Vec<Address>,
    to: Vec<Address>,
    from_wildcards: Vec<Address>,
    to_wildcards: Vec<Address>,
    all_preconfs: bool,
    preconf_timeout_ms: u64,
    safety_margin_ms: u64,
    max_gas_per_tx: u64,
    max_gas_per_block: u64,
    journal_path: Option<PathBuf>,
    journal_max_size: u64,
    whitelist_contract: Address,
}

impl Default for PreconfCfgBuilder {
    fn default() -> Self {
        Self {
            pairs: Vec::new(),
            from: Vec::new(),
            to: Vec::new(),
            from_wildcards: Vec::new(),
            to_wildcards: Vec::new(),
            all_preconfs: false,
            preconf_timeout_ms: 1_500,
            safety_margin_ms: 40,
            max_gas_per_tx: 2_000_000,
            max_gas_per_block: 6_000_000,
            journal_path: None,
            journal_max_size: PreconfConfig::default().journal_max_size,
            whitelist_contract: WHITELIST_CONTRACT_SENTINEL,
        }
    }
}

impl PreconfCfgBuilder {
    /// Fresh builder — same as [`Default::default`], provided for
    /// call-site clarity.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one sender to the whitelist. Repeatable.
    pub fn whitelist_from(mut self, from: Address) -> Self {
        self.from.push(from);
        self
    }

    /// Allowlist exactly the rule `from -> to`. Repeatable.
    ///
    /// Prefer this in tests that are *about* the allowlist: [`Self::whitelist_from`] and
    /// [`Self::whitelist_to`] expand to their cross product (see [`Self::build`]), so a
    /// reader has to know that expansion to see what rules they actually install.
    pub fn whitelist_pair(mut self, from: Address, to: Address) -> Self {
        self.pairs.push((from, to));
        self
    }

    /// Allowlist every transaction **from** `addr`, whatever the recipient —
    /// including a contract creation. Repeatable.
    pub fn whitelist_from_wildcard(mut self, addr: Address) -> Self {
        self.from_wildcards.push(addr);
        self
    }

    /// Allowlist every transaction **to** `addr`, whatever the sender.
    /// Repeatable.
    pub fn whitelist_to_wildcard(mut self, addr: Address) -> Self {
        self.to_wildcards.push(addr);
        self
    }

    /// Add one recipient to the whitelist. Repeatable.
    pub fn whitelist_to(mut self, to: Address) -> Self {
        self.to.push(to);
        self
    }

    /// Point the config at a real on-chain `PreconfWhitelist` instead of relying on
    /// the in-memory seed.
    ///
    /// Overrides the placeholder [`WHITELIST_CONTRACT_SENTINEL`]. Cold start runs
    /// inside `build_pool`, so it will read this address; tests that need a
    /// specific post-boot state call `mantle_reth_preconf::bootstrap_whitelist`
    /// again themselves after seeding storage.
    pub fn whitelist_contract(mut self, contract: Address) -> Self {
        self.whitelist_contract = contract;
        self
    }

    /// Bypass the (from, to) whitelist and treat every tx as
    /// preconf-eligible. Aligns with op-geth's `--txpool.allpreconfs`.
    pub fn all_preconfs(mut self) -> Self {
        self.all_preconfs = true;
        self
    }

    /// Client-visible RPC oneshot deadline, in milliseconds.
    pub fn preconf_timeout_ms(mut self, ms: u64) -> Self {
        self.preconf_timeout_ms = ms;
        self
    }

    /// Dispatch-time preemption margin (see `PreconfConfig::safety_margin`).
    /// Setting this to `0` disables the preemptive Timeout gate so tests
    /// can exercise the RPC-layer race-resolution branch.
    pub fn safety_margin_ms(mut self, ms: u64) -> Self {
        self.safety_margin_ms = ms;
        self
    }

    /// Per-tx gas cap enforced at pool admission.
    pub fn max_gas_per_tx(mut self, gas: u64) -> Self {
        self.max_gas_per_tx = gas;
        self
    }

    /// Cumulative preconf gas budget per block.
    pub fn max_gas_per_block(mut self, gas: u64) -> Self {
        self.max_gas_per_block = gas;
        self
    }

    /// Enable the on-disk journal at `path`. Callers are responsible for
    /// choosing a unique tempfile per test.
    pub fn journal_path(mut self, path: PathBuf) -> Self {
        self.journal_path = Some(path);
        self
    }

    /// Journal file size ceiling in bytes.
    pub fn journal_max_size(mut self, max: u64) -> Self {
        self.journal_max_size = max;
        self
    }

    /// Materialise the config plus the allowlists to seed. Panics on invariant
    /// violations because these are test-controlled inputs — a panic is more
    /// useful than threading a `Result` through every launch call site.
    ///
    /// Returns a [`PreconfSetup`] rather than a bare `PreconfConfig` because the
    /// allowlists no longer live on the config: they belong to the
    /// `PreconfClassifier`, which only exists once the `PreconfServiceBuilder`
    /// has been constructed. `launch_preconf_node!` does that and then calls
    /// [`PreconfSetup::seed`].
    pub fn build(self) -> PreconfSetup {
        let cfg = PreconfConfig {
            enabled: true,
            // `validate()` requires a non-zero whitelist address whenever
            // preconf is enabled without `all_preconfs`. These tests seed the
            // allowlists directly instead of reading them from a contract, so a
            // sentinel satisfies the check — see `WHITELIST_CONTRACT_SENTINEL`
            // for why the address must still carry code.
            whitelist_contract: Some(self.whitelist_contract),
            all_preconfs: self.all_preconfs,
            preconf_timeout: std::time::Duration::from_millis(self.preconf_timeout_ms),
            safety_margin: std::time::Duration::from_millis(self.safety_margin_ms),
            preconf_max_gas_per_tx: self.max_gas_per_tx,
            preconf_max_gas_per_block: self.max_gas_per_block,
            journal_path: self.journal_path,
            journal_max_size: self.journal_max_size,
            ..PreconfConfig::default()
        };
        // `whitelist_from` / `whitelist_to` mean `from in F && to in T`, which is exactly
        // the cross product of the two lists. Tests wanting a wildcard say so directly.
        let mut pairs = self.pairs;
        pairs.extend(self.from.iter().flat_map(|f| self.to.iter().map(move |t| (*f, *t))));
        PreconfSetup {
            cfg,
            pairs,
            from_wildcards: self.from_wildcards,
            to_wildcards: self.to_wildcards,
        }
    }
}

/// A config plus the allowlists a test wants installed on the classifier.
///
/// Exists because the two halves are owned by different objects now: the config
/// is consumed by `PreconfServiceBuilder::from_config`, and the allowlist is
/// written into the genesis of the classifier that builder creates.
#[derive(Debug, Clone)]
pub struct PreconfSetup {
    /// The config handed to `PreconfServiceBuilder::from_config`.
    pub cfg: PreconfConfig,
    /// Exact `(from, to)` rules to seed.
    pub pairs: Vec<(Address, Address)>,
    /// Senders whose every transaction is eligible.
    pub from_wildcards: Vec<Address>,
    /// Recipients that make any transaction to them eligible.
    pub to_wildcards: Vec<Address>,
}

/// How long [`canonicalize_payload!`] waits for a block to become canonical,
/// as 20 ms ticks.
///
/// Deliberately generous (4s): the whole suite spawns nodes in parallel, and
/// under that load the engine's commit + provider refresh take far longer than
/// when a test runs alone. Costs nothing on the happy path — the loop exits as
/// soon as the head moves.
pub const CANON_POLL_TICKS: usize = 200;

/// Submit `payload`, make it canonical, and **wait until the node's own
/// provider actually serves it**. Returns the new head hash.
///
/// ## Why this exists
///
/// `update_forkchoice` resolves as soon as the engine has accepted the forkchoice update;
/// the provider starts serving the new head only once the canonical-chain update has been
/// committed. Bridging that gap with a fixed `sleep`, as this suite used to, is a race:
/// under parallel load the commit takes longer, the test reads pre-block state, and the
/// failure surfaces as a wrong balance or a stale nonce rather than as "canonicalisation was
/// slow". Polling on the
/// observable condition removes the guess, and failing to converge panics with the block
/// number and hash, so a real canonicalisation failure is distinguishable from a slow one.
///
/// A macro, not a function: the `NodeTestContext<Node, AddOns>` type parameters cannot be
/// spelled at a function boundary. `reth`'s own `NodeTestContext::wait_block` does almost
/// this, but loops forever — a hang instead of a failure — so it is not used here.
#[macro_export]
macro_rules! canonicalize_payload {
    ($node:expr, $payload:expr) => {{
        async {
            use reth_provider::{BlockNumReader, HeaderProvider};

            let number = $payload.block().header().number;
            // Submitted through the engine handle rather than
            // `NodeTestContext::submit_payload`, which discards the
            // `PayloadStatus` — an INVALID newPayload then only surfaced later as
            // an FCU "links to previously rejected block", or not at all.
            let new_head = $payload.block().hash();
            let np = $node
                .inner
                .add_ons_handle
                .beacon_engine_handle
                .new_payload(
                    <reth_optimism_node::engine::OpEngineTypes as reth_node_api::PayloadTypes>
                        ::block_to_payload($payload.block().clone()),
                )
                .await
                .expect("newPayload call must not error");
            assert!(
                np.is_valid(),
                "newPayload for block {number} must be VALID, got {np:?}",
            );
            let fcu = $node
                .inner
                .add_ons_handle
                .beacon_engine_handle
                .fork_choice_updated(
                    alloy_rpc_types_engine::ForkchoiceState {
                        head_block_hash: new_head,
                        safe_block_hash: new_head,
                        finalized_block_hash: new_head,
                    },
                    None,
                )
                .await
                .expect("forkchoice update must be accepted");

            let mut landed = false;
            for _ in 0..$crate::helpers::CANON_POLL_TICKS {
                let best = $node.inner.provider.best_block_number().unwrap_or_default();
                if best >= number &&
                    $node
                        .inner
                        .provider
                        .header_by_number(number)
                        .ok()
                        .flatten()
                        .is_some_and(|h| h.hash_slow() == new_head)
                {
                    landed = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(
                landed,
                "block {number} ({new_head:?}) never became canonical; best_block_number = {:?}; fcu = {fcu:?}",
                $node.inner.provider.best_block_number(),
            );

            new_head
        }
    }};
}

/// Call `eth_sendRawTransactionWithPreconf` and return the parsed wire
/// event. Errors surface as raw jsonrpsee client errors so callers can
/// inspect `error.code()` / `error.message()` for typed rejections
/// (nonce gap, whitelist miss, ...).
///
/// `tx_rlp` is passed as a hex-encoded string — `Bytes: Display`
/// already emits the required `0x`-prefixed form.
pub async fn send_preconf(
    http: &HttpClient,
    tx_rlp: Bytes,
) -> Result<PreconfTxEvent, jsonrpsee::core::ClientError> {
    http.request("eth_sendRawTransactionWithPreconf", vec![tx_rlp.to_string()]).await
}

/// Submit a plain tx via `eth_sendRawTransaction` (the tip-ordered pool path).
/// Returns immediately with the tx hash — unlike preconf it does not wait for
/// inclusion. Use for the "normal high-tip" tx in ordering tests.
pub async fn send_normal(
    http: &HttpClient,
    tx_rlp: Bytes,
) -> Result<B256, jsonrpsee::core::ClientError> {
    http.request("eth_sendRawTransaction", vec![tx_rlp.to_string()]).await
}

/// Poll `eth_getTransactionCount(sender, "pending")` until it reaches `want`.
///
/// Gates same-sender multi-tx submission on observed pool state instead of a
/// fixed sleep: the pool admits asynchronously, so under load a follow-up tx's
/// nonce-gap pre-check can run before the prior tx is pending and reject it as a
/// gap. Panics if the nonce never advances within ~2s.
pub async fn wait_pending_nonce(http: &HttpClient, sender: Address, want: u64) {
    for _ in 0..100 {
        let n: U256 = http
            .request("eth_getTransactionCount", vec![sender.to_string(), "pending".to_string()])
            .await
            .expect("eth_getTransactionCount");
        if n >= U256::from(want) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("pending nonce for {sender:?} never reached {want}");
}

/// Poll `eth_getTransactionCount(sender, "latest")` until it reaches `want` —
/// i.e. until a just-canonicalised block's state is applied.
///
/// Gates on canon settlement before the next slot: under load `canon_handler::
/// forward` lags the FCU, so a fixed sleep can let the next job re-apply the
/// prior slot's still-`Success` entries. Panics if it never advances within ~5s.
pub async fn wait_latest_nonce(http: &HttpClient, sender: Address, want: u64) {
    for _ in 0..250 {
        let n: U256 = http
            .request("eth_getTransactionCount", vec![sender.to_string(), "latest".to_string()])
            .await
            .expect("eth_getTransactionCount");
        if n >= U256::from(want) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("on-chain nonce for {sender:?} never reached {want}");
}

/// Create a fresh empty journal file under a unique tempdir so the journal-on
/// (reorg-replay) path is active. `prefix` disambiguates concurrent tests.
pub fn fresh_journal(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mantle-preconf-{prefix}-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir journal dir");
    dir.join("preconf.journal")
}

/// Launch a preconf-enabled `MantleNode`.
///
/// Yields a tuple `(node_ctx, http, wallet, chain_id)` — the `NodeTestContext`
/// (for engine-API / advance / assert helpers), a jsonrpsee HTTP client
/// bound to the RPC port, the pre-funded test wallet, and the chain id.
///
/// Uses a macro instead of a function because the concrete
/// `NodeTestContext<Node, AddOns>` type-parameter tuple threads a
/// deeply-nested `ComponentsBuilder<...>` through several generics —
/// spelling it out at a function boundary requires 20+ lines of
/// `where` clauses for zero readability gain. A macro sidesteps this
/// by inlining at each call site so the compiler infers everything.
///
/// Call as:
///
/// ```ignore
/// let (mut node, http, wallet, chain_id) =
///     launch_preconf_node!(
///         PreconfCfgBuilder::new().whitelist_from(sender).whitelist_to(recipient).build(),
///     )
///         .await;
/// ```
#[macro_export]
macro_rules! launch_preconf_node {
    ($cfg:expr) => {
        $crate::launch_preconf_node!($cfg, $crate::helpers::mantle_test_chain_spec())
    };
    ($cfg:expr, $chain_spec:expr) => {
        async {
            let (node, http, wallet, chain_id, _classifier) =
                $crate::launch_preconf_node!(
                    @build $cfg, $chain_spec,
                    |svc| mantle_reth_cli::node::MantleNode::default().with_preconf(svc)
                )
                .await;
            (node, http, wallet, chain_id)
        }
    };
    // Variant that additionally installs an `OpDAConfig` on the node so
    // tests can exercise the DA-footprint gate with a tight per-tx /
    // per-block DA limit. `$da` is any `OpDAConfig` expression.
    ($cfg:expr, $chain_spec:expr, da_config = $da:expr) => {
        async {
            let (node, http, wallet, chain_id, _classifier) =
                $crate::launch_preconf_node!(
                    @build $cfg, $chain_spec,
                    |svc| mantle_reth_cli::node::MantleNode::default()
                        .with_preconf(svc)
                        .with_da_config($da)
                )
                .await;
            (node, http, wallet, chain_id)
        }
    };
    (@build $cfg:expr, $chain_spec:expr, $make_node:expr) => {{
        async {
            use $crate::helpers::mantle_payload_attributes;
            use mantle_reth_cli::node::MantleNode;
            use mantle_reth_preconf::PreconfServiceBuilder;
            use reth_chainspec::EthChainSpec;
            use reth_db::test_utils::create_test_rw_db_with_path;
            use reth_e2e_test_utils::{node::NodeTestContext, wallet::Wallet};
            use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig};
            use reth_node_core::args::{DatadirArgs, RpcServerArgs};
            use reth_provider::providers::BlockchainProvider;
            use reth_tasks::Runtime;

            // Destructured before the spec is finalised: the allowlists have to
            // be written into genesis, not into memory, because cold start now
            // runs inside `build_pool` and would overwrite an in-memory seed.
            let $crate::helpers::PreconfSetup { cfg, pairs, from_wildcards, to_wildcards } = $cfg;
            let chain_spec = $chain_spec;
            let chain_spec =
                if cfg.whitelist_contract == Some($crate::helpers::WHITELIST_CONTRACT_SENTINEL) {
                    $crate::helpers::with_sentinel_whitelist(
                        chain_spec,
                        &pairs,
                        &from_wildcards,
                        &to_wildcards,
                    )
                } else {
                    // The test pointed at a real contract address and owns that
                    // address's genesis itself.
                    chain_spec
                };
            let chain_id = chain_spec.chain().id();
            let wallet = Wallet::default().with_chain_id(chain_id);

            let mut config: NodeConfig<reth_optimism_chainspec::OpChainSpec> =
                NodeConfig::new(chain_spec)
                    .with_unused_ports()
                    .with_datadir_args(DatadirArgs {
                        datadir: reth_db::test_utils::tempdir_path().into(),
                        ..Default::default()
                    })
                    .with_rpc(RpcServerArgs::default().with_unused_ports().with_http());
            config.network.discovery.discv5_port = 0;
            config.network.discovery.discv5_port_ipv6 = 0;

            let db = create_test_rw_db_with_path(
                config
                    .datadir
                    .datadir
                    .unwrap_or_chain_default(config.chain.chain(), config.datadir.clone())
                    .db(),
            );

            // The journal is mandatory; fill a temp default path when the test
            // didn't set one (mirrors production's datadir-relative default).
            let mut preconf_cfg = cfg;
            if preconf_cfg.journal_path.is_none() {
                preconf_cfg.journal_path = Some(
                    reth_db::test_utils::tempdir_path().join("mantle-preconf-journal.jsonl"),
                );
            }
            let svc = PreconfServiceBuilder::from_config(preconf_cfg)
                .await
                .expect("preconf svc init");
            let classifier = svc.classifier().clone();
            let make_node = $make_node;
            let node_type = make_node(svc);

            let runtime = Runtime::test();
            let node_handle = NodeBuilder::new(config)
                .with_database(db)
                .with_types_and_provider::<MantleNode, BlockchainProvider<_>>()
                .with_components(node_type.components())
                .with_add_ons(node_type.add_ons())
                .launch_with_fn(|builder| {
                    let launcher = EngineNodeLauncher::new(
                        runtime.clone(),
                        builder.config.datadir(),
                        Default::default(),
                    );
                    builder.launch_with(launcher)
                })
                .await
                .expect("MantleNode failed to launch");

            let http = node_handle
                .node
                .rpc_server_handle()
                .http_client()
                .expect("HTTP RPC must be enabled");

            let node_ctx =
                NodeTestContext::new(node_handle.node, mantle_payload_attributes)
                    .await
                    .unwrap();

            (node_ctx, http, wallet, chain_id, classifier)
        }
    }};
}

/// `L1Block` predeploy — recipient of the per-block L1-attributes deposit.
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

/// Base L2 timestamp; the height-`n` block gets `L2_GENESIS_TS + n*2` (fixed 2s
/// spacing), so a reorg rebuild at the same height carries the SAME timestamp as
/// the block it replaces — matching op-stack, where the block time is derived
/// from height, not from a per-build counter.
pub const L2_GENESIS_TS: u64 = 1_710_338_136;

/// L2 block timestamp for height `n` (2s spacing).
pub const fn l2_ts(n: u64) -> u64 {
    L2_GENESIS_TS + n * 2
}

/// Build the L1-attributes deposit (a block's tx[0]) for L1 origin `origin`.
///
/// `origin` is written into the L1 block number (payload `[24..32]`), the L1
/// block hash (`[96..128]` = keccak(origin)) and the deposit `source_hash`, so
/// two blocks built with different `origin` values reference genuinely different
/// L1 origins and therefore hash differently — this is how a reorg is made
/// observable here (an L1 reorg re-derives the L2 block against a new L1 origin),
/// instead of relying on a timestamp difference.
pub fn l1_info_deposit(origin: u64) -> Bytes {
    // Arsia setL1BlockValues calldata: 4-byte selector + 174-byte payload.
    let mut data = vec![0u8; 178];
    data[0..4].copy_from_slice(&hex!("49e72383"));
    let p = &mut data[4..];
    p[0..4].copy_from_slice(&1_000_000u32.to_be_bytes()); // base_fee_scalar (positive L1 fee)
    p[24..32].copy_from_slice(&origin.to_be_bytes()); // l1BlockNumber = origin
    p[32..64].copy_from_slice(&U256::from(1_000_000_000u64).to_be_bytes::<32>()); // l1_base_fee
    p[96..128].copy_from_slice(keccak256(origin.to_be_bytes()).as_slice()); // l1BlockHash = H(origin)

    let mut source = [0u8; 32];
    source[24..32].copy_from_slice(&origin.to_be_bytes());
    let dep = TxDeposit {
        source_hash: B256::from(source),
        from: Address::ZERO,
        to: TxKind::Call(L1_BLOCK),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        is_system_transaction: true,
        input: data.into(),
        eth_value: 0,
        eth_tx_value: None,
    };
    dep.encoded_2718().into()
}

/// Payload attributes for a block at height `n` referencing L1 origin `origin`:
/// timestamp = `l2_ts(n)`, tx[0] = the L1-attributes deposit for `origin`.
pub fn l1_attrs(n: u64, origin: u64) -> OpPayloadAttrs {
    OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: l2_ts(n),
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
        },
        transactions: Some(vec![l1_info_deposit(origin)]),
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(0),
    })
}

// ─────────────────────────── engine-API façade ───────────────────────────
//
// A thin, self-documenting adapter over the in-process engine handles. Each
// macro is named after the `engine_*` RPC op-node sends in production and maps
// it to the internal handle that authrpc itself forwards to — so a reader can
// see which engine call each test step corresponds to. This is NOT a mock:
// nothing is faked, the real engine / preconf logic runs; only the authrpc
// HTTP + JWT + payload-codec layer is skipped (covered separately).
//
// Brought into scope for every `preconf/*` module via `#[macro_use] pub mod
// helpers;` in `mod.rs`. All external paths are fully qualified so call sites
// need no extra imports.

/// == `engine_forkchoiceUpdatedV3` **with** payload attributes: begin a build
/// on `$head`. op-node sends this at slot start; on a reorg it points `$head`
/// at an ancestor. Returns the `payload_id` for the subsequent getPayload.
///
/// Production: op-node → authrpc `engine_forkchoiceUpdatedV3`
/// Here:       `beacon_engine_handle.fork_choice_updated(state, Some(attrs))`
#[macro_export]
macro_rules! fcu_v3_start {
    ($node:expr, $head:expr, $attrs:expr) => {{
        let state = ::alloy_rpc_types_engine::ForkchoiceState {
            head_block_hash: $head,
            safe_block_hash: $head,
            finalized_block_hash: $head,
        };
        let attrs = $attrs;
        // A freshly-switched forkchoice (esp. a reorg to an ancestor) can briefly
        // report SYNCING → no payload_id. Retry until the build starts. Generous
        // budget (150ms × 133 ≈ 20s): a starved or slow-start node can take
        // several seconds to leave SYNCING; the loop breaks as soon as it opens,
        // so the happy path is unaffected.
        let mut payload_id = None;
        for _ in 0..133 {
            let res = $node
                .inner
                .add_ons_handle
                .beacon_engine_handle
                .fork_choice_updated(state, Some(attrs.clone()))
                .await
                .expect("engine_forkchoiceUpdatedV3(attrs) must succeed");
            if let Some(p) = res.payload_id {
                payload_id = Some(p);
                break;
            }
            ::tokio::time::sleep(::std::time::Duration::from_millis(150)).await;
        }
        payload_id.expect("payload_id present after FCU retries")
    }};
}

/// == `engine_getPayloadV5`: take the built payload. op-node uses getPayloadV5
/// once the Mantle Limb fork is active (op-node `Config::GetPayloadVersion`:
/// `IsMantleLimb` → V5); the version only affects the authrpc envelope, so
/// in-process we resolve the same built payload regardless. Resolve kind is
/// `Earliest`, matching reth's production `payload_store.resolve()`. The sleep
/// stands in for op-node holding the build ~a slot so carryover/replay runs
/// before seal.
///
/// Production: op-node → authrpc `engine_getPayloadV5`
/// Here:       `payload_builder_handle.resolve_kind(id, Earliest)`
#[macro_export]
macro_rules! get_payload_v5 {
    ($node:expr, $pid:expr) => {{
        ::tokio::time::sleep(::std::time::Duration::from_millis(400)).await;
        $node
            .inner
            .payload_builder_handle
            .resolve_kind($pid, ::reth_node_api::PayloadKind::Earliest)
            .await
            .expect("engine_getPayloadV5 resolve")
            .expect("payload build present")
    }};
}

/// == `engine_newPayloadV4`: hand the payload back to the engine to execute /
/// insert. Returns the new head hash.
///
/// Production: op-node → authrpc `engine_newPayloadV4`
/// Here:       `beacon_engine_handle.new_payload(..)` via `submit_payload`
#[macro_export]
macro_rules! new_payload_v4 {
    ($node:expr, $payload:expr) => {{ $node.submit_payload($payload).await.expect("engine_newPayloadV4 submit") }};
}

/// == the post-insert `engine_forkchoiceUpdatedV3` **without** attributes:
/// commit `$head` as canonical.
#[macro_export]
macro_rules! fcu_v3_commit {
    ($node:expr, $head:expr) => {{
        $node.update_forkchoice($head, $head).await.expect("engine_forkchoiceUpdatedV3(commit)");
    }};
}

/// Status-aware canonization: re-drive newPayload+FCU until the FCU is **VALID**
/// and the canonical head reaches block `$target_number` (via `eth_blockNumber`).
/// Returns the head hash.
///
/// The plain `submit_payload`/`update_forkchoice` helpers discard the engine
/// status, so a transient `SYNCING` under load leaves the head un-advanced and
/// mis-drives the next slot. Re-sending newPayload each retry lets the engine
/// (re)validate the block — the step that actually closes the race.
#[macro_export]
macro_rules! canonize {
    ($node:expr, $http:expr, $payload:expr, $target_number:expr) => {{
        let payload = $payload;
        let head = $node.submit_payload(payload.clone()).await.expect("newPayload");
        let mut canonized = false;
        for _ in 0..200 {
            let fcu = $node
                .inner
                .add_ons_handle
                .beacon_engine_handle
                .fork_choice_updated(
                    ::alloy_rpc_types_engine::ForkchoiceState {
                        head_block_hash: head,
                        safe_block_hash: head,
                        finalized_block_hash: head,
                    },
                    None,
                )
                .await;
            if matches!(&fcu, Ok(f) if f.is_valid()) {
                let bn: ::alloy_primitives::U256 =
                    $http.request("eth_blockNumber", Vec::<String>::new()).await.unwrap_or_default();
                if bn >= ::alloy_primitives::U256::from($target_number as u64) {
                    canonized = true;
                    break;
                }
            }
            // FCU was SYNCING (or head hasn't advanced yet) — re-insert the
            // block so the engine can (re)validate it, then retry.
            let _ = $node.submit_payload(payload.clone()).await;
            ::tokio::time::sleep(::std::time::Duration::from_millis(50)).await;
        }
        assert!(canonized, "canonize: block {} never became canonical", $target_number);
        head
    }};
}

/// Like [`canonize!`] but for a payload already built inside a slot macro:
/// re-drives newPayload+FCU until the FCU is **VALID**, using the FCU status
/// directly (no RPC handle needed). The `fcu_v3_commit!` replacement — the
/// latter can't re-send newPayload (it lacks the payload), which is the step
/// that closes the race. Returns the head hash.
#[macro_export]
macro_rules! canonize_built {
    ($node:expr, $payload:expr) => {{
        let payload = $payload;
        let head =
            $node.submit_payload(payload.clone()).await.expect("engine_newPayloadV4 submit");
        let mut canonized = false;
        for _ in 0..200 {
            let fcu = $node
                .inner
                .add_ons_handle
                .beacon_engine_handle
                .fork_choice_updated(
                    ::alloy_rpc_types_engine::ForkchoiceState {
                        head_block_hash: head,
                        safe_block_hash: head,
                        finalized_block_hash: head,
                    },
                    None,
                )
                .await;
            if matches!(&fcu, Ok(f) if f.is_valid()) {
                canonized = true;
                break;
            }
            // FCU SYNCING — re-insert the block so the engine (re)validates it.
            let _ = $node.submit_payload(payload.clone()).await;
            ::tokio::time::sleep(::std::time::Duration::from_millis(50)).await;
        }
        assert!(canonized, "canonize_built: FCU never returned VALID for the committed head");
        head
    }};
}

/// One op-node output slot on parent `$on`:
///   FCUv3(build) → getPayloadV5 → newPayloadV4 → FCUv3(commit).
/// Returns `(new_head, sealed_tx_hashes)`.
#[macro_export]
macro_rules! op_node_slot {
    ($node:expr,on = $on:expr) => {{
        // NOTE: simplified attributes (shared with the rest of the suite). To
        // match op-node byte-for-byte, align these with real op-node attrs —
        // esp. `no_tx_pool`, the leading L1-info deposit tx, gas_limit,
        // eip_1559_params, min_base_fee. That change lives entirely here.
        let attrs = $node.payload.next_attributes();
        let pid = $crate::fcu_v3_start!($node, $on, attrs);
        let payload = $crate::get_payload_v5!($node, pid);
        let sealed: ::std::vec::Vec<::alloy_primitives::B256> = payload
            .block()
            .body()
            .transactions()
            .map(|tx| {
                ::alloy_primitives::keccak256(
                    ::alloy_network::eip2718::Encodable2718::encoded_2718(tx),
                )
            })
            .collect();
        let head = $crate::canonize_built!($node, payload);
        (head, sealed)
    }};
}

/// An op-node output slot at height `$n` on parent `$on`, referencing L1 origin
/// `$l1`. Unlike `op_node_slot!` this pins the block timestamp to the height
/// (`l2_ts($n)`) and injects the L1-attributes deposit as tx[0] — so a reorg
/// rebuild at the same `$n` (same timestamp) but a different `$l1` differs
/// ONLY by its L1 origin, exactly as an L1 reorg would produce.
/// Returns `(new_head, sealed_tx_hashes)`; `sealed[0]` is the L1-info deposit.
#[macro_export]
macro_rules! op_node_slot_l1 {
    ($node:expr,on = $on:expr,n = $n:expr,l1 = $l1:expr) => {{
        let attrs = $crate::helpers::l1_attrs($n, $l1);
        let pid = $crate::fcu_v3_start!($node, $on, attrs);
        let payload = $crate::get_payload_v5!($node, pid);
        let sealed: ::std::vec::Vec<::alloy_primitives::B256> = payload
            .block()
            .body()
            .transactions()
            .map(|tx| {
                ::alloy_primitives::keccak256(
                    ::alloy_network::eip2718::Encodable2718::encoded_2718(tx),
                )
            })
            .collect();
        let head = $crate::canonize_built!($node, payload);
        (head, sealed)
    }};
}

/// == the reorg trigger: "pause op-node → authrpc FCU to an ancestor → resume".
/// One op-node slot whose parent is the ancestor `$ancestor`, building a
/// competing block that reverts everything after it. The `n = .., l1 = ..` form
/// pins the height (timestamp) and L1 origin so the rebuild differs from the
/// reverted block ONLY by its L1 origin. Returns `(new_head, sealed_tx_hashes)`.
#[macro_export]
macro_rules! reorg_to {
    ($node:expr, $ancestor:expr) => {{ $crate::op_node_slot!($node, on = $ancestor) }};
    ($node:expr, $ancestor:expr,n = $n:expr,l1 = $l1:expr) => {{ $crate::op_node_slot_l1!($node, on = $ancestor, n = $n, l1 = $l1) }};
}

/// Same as [`launch_preconf_node!`] but also yields the node's
/// `Arc<PreconfClassifier>` as a fifth element.
///
/// Needed by tests that drive the allowlists themselves — reading them out of
/// chain state via `bootstrap_whitelist`, or asserting on what the watcher
/// loaded. They cannot reach the classifier otherwise: the service builder that
/// owns it is moved into the node.
#[macro_export]
macro_rules! launch_preconf_node_with_classifier {
    ($cfg:expr, $chain_spec:expr) => {
        $crate::launch_preconf_node!(
            @build $cfg, $chain_spec,
            |svc| mantle_reth_cli::node::MantleNode::default().with_preconf(svc)
        )
    };
}

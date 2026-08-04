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

/// Build a Mantle-flavoured `OpChainSpec` bound to the given L2 chain id.
///
/// The base fixture is Mantle mainnet's `assets/genesis.json` (all Mantle
/// hardforks at timestamp 0). The `chainId` field of the genesis config
/// is patched to `chain_id` before the spec is constructed, so every
/// other invariant (fork schedule, EIP-1559 params, allocations) stays
/// identical across networks — the only difference on the L2 side is
/// the id used for signature recovery and network identification.
///
/// Supported ids for the pair-test matrix:
/// - `5000` — Mantle Mainnet
/// - `5003` — Mantle Sepolia
/// - `50002` — Mantle Hoodi
pub fn mantle_chain_spec_for(chain_id: u64) -> Arc<OpChainSpec> {
    let raw = include_str!("../assets/genesis.json");
    let mut value: serde_json::Value = serde_json::from_str(raw).expect("valid genesis JSON");
    value["config"]["chainId"] = serde_json::Value::from(chain_id);
    let genesis: Genesis = serde_json::from_value(value).expect("patched genesis deserialises");
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
    let genesis_raw = include_str!("../assets/genesis.json");
    let predeploys_raw = include_str!("../assets/predeploys.json");

    let mut base: serde_json::Value =
        serde_json::from_str(genesis_raw).expect("valid genesis JSON");
    base["config"]["chainId"] = serde_json::Value::from(chain_id);

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

/// Fluent builder for `PreconfConfig` inside tests. Every knob has a
/// safe default; individual test cases tweak only what they exercise.
///
/// Decoupled from `PreconfConfig::default` so a future production
/// default change (e.g. `preconf_timeout` bumped fleet-wide) does not
/// silently retune the integration tests.
#[derive(Debug, Clone)]
pub struct PreconfCfgBuilder {
    from: Vec<Address>,
    to: Vec<Address>,
    all_preconfs: bool,
    preconf_timeout_ms: u64,
    safety_margin_ms: u64,
    max_gas_per_tx: u64,
    max_gas_per_block: u64,
    journal_path: Option<PathBuf>,
    journal_max_size: u64,
}

impl Default for PreconfCfgBuilder {
    fn default() -> Self {
        Self {
            from: Vec::new(),
            to: Vec::new(),
            all_preconfs: false,
            preconf_timeout_ms: 1_500,
            safety_margin_ms: 40,
            max_gas_per_tx: 2_000_000,
            max_gas_per_block: 6_000_000,
            journal_path: None,
            journal_max_size: PreconfConfig::default().journal_max_size,
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

    /// Add one recipient to the whitelist. Repeatable.
    pub fn whitelist_to(mut self, to: Address) -> Self {
        self.to.push(to);
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

    /// Materialise the config. Panics on invariant violations because
    /// these are test-controlled inputs — a panic is more useful than
    /// threading a `Result` through every launch call site.
    pub fn build(self) -> PreconfConfig {
        PreconfConfig {
            enabled: true,
            from_preconfs: self.from.into_iter().collect(),
            to_preconfs: self.to.into_iter().collect(),
            all_preconfs: self.all_preconfs,
            preconf_timeout: std::time::Duration::from_millis(self.preconf_timeout_ms),
            safety_margin: std::time::Duration::from_millis(self.safety_margin_ms),
            preconf_max_gas_per_tx: self.max_gas_per_tx,
            preconf_max_gas_per_block: self.max_gas_per_block,
            journal_path: self.journal_path,
            journal_max_size: self.journal_max_size,
            ..PreconfConfig::default()
        }
    }
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
pub async fn send_normal(http: &HttpClient, tx_rlp: Bytes) -> Result<B256, jsonrpsee::core::ClientError> {
    http.request("eth_sendRawTransaction", vec![tx_rlp.to_string()]).await
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
///     launch_preconf_node!(PreconfCfgBuilder::new().whitelist_from(addr).build())
///         .await;
/// ```
#[macro_export]
macro_rules! launch_preconf_node {
    ($cfg:expr) => {
        $crate::launch_preconf_node!($cfg, $crate::helpers::mantle_test_chain_spec())
    };
    ($cfg:expr, $chain_spec:expr) => {
        $crate::launch_preconf_node!(
            @build $cfg, $chain_spec,
            |svc| mantle_reth_cli::node::MantleNode::default().with_preconf(svc)
        )
    };
    // Variant that additionally installs an `OpDAConfig` on the node so
    // tests can exercise the DA-footprint gate with a tight per-tx /
    // per-block DA limit. `$da` is any `OpDAConfig` expression.
    ($cfg:expr, $chain_spec:expr, da_config = $da:expr) => {
        $crate::launch_preconf_node!(
            @build $cfg, $chain_spec,
            |svc| mantle_reth_cli::node::MantleNode::default()
                .with_preconf(svc)
                .with_da_config($da)
        )
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

            let chain_spec = $chain_spec;
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

            let svc = PreconfServiceBuilder::from_config($cfg)
                .await
                .expect("preconf svc init");
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

            (node_ctx, http, wallet, chain_id)
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
        // report SYNCING → no payload_id. Retry the FCU until the build starts,
        // mirroring `NodeTestContext::sync_to`'s FCU loop.
        let mut payload_id = None;
        for _ in 0..40 {
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
    ($node:expr, $payload:expr) => {{
        $node.submit_payload($payload).await.expect("engine_newPayloadV4 submit")
    }};
}

/// == the post-insert `engine_forkchoiceUpdatedV3` **without** attributes:
/// commit `$head` as canonical.
#[macro_export]
macro_rules! fcu_v3_commit {
    ($node:expr, $head:expr) => {{
        $node.update_forkchoice($head, $head).await.expect("engine_forkchoiceUpdatedV3(commit)");
    }};
}

/// One op-node output slot on parent `$on`:
///   FCUv3(build) → getPayloadV5 → newPayloadV4 → FCUv3(commit).
/// Returns `(new_head, sealed_tx_hashes)`.
#[macro_export]
macro_rules! op_node_slot {
    ($node:expr, on = $on:expr) => {{
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
            .map(|tx| ::alloy_primitives::keccak256(
                ::alloy_network::eip2718::Encodable2718::encoded_2718(tx),
            ))
            .collect();
        let head = $crate::new_payload_v4!($node, payload);
        $crate::fcu_v3_commit!($node, head);
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
    ($node:expr, on = $on:expr, n = $n:expr, l1 = $l1:expr) => {{
        let attrs = $crate::helpers::l1_attrs($n, $l1);
        let pid = $crate::fcu_v3_start!($node, $on, attrs);
        let payload = $crate::get_payload_v5!($node, pid);
        let sealed: ::std::vec::Vec<::alloy_primitives::B256> = payload
            .block()
            .body()
            .transactions()
            .map(|tx| ::alloy_primitives::keccak256(
                ::alloy_network::eip2718::Encodable2718::encoded_2718(tx),
            ))
            .collect();
        let head = $crate::new_payload_v4!($node, payload);
        $crate::fcu_v3_commit!($node, head);
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
    ($node:expr, $ancestor:expr, n = $n:expr, l1 = $l1:expr) => {{
        $crate::op_node_slot_l1!($node, on = $ancestor, n = $n, l1 = $l1)
    }};
}

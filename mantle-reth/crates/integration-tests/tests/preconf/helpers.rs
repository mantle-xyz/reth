//! Shared helpers for `preconf/*` integration tests.

use alloy_genesis::Genesis;
use alloy_primitives::{Address, B64, B256, Bytes};
use alloy_rpc_types_engine::PayloadAttributes;
use jsonrpsee::{core::client::ClientT, http_client::HttpClient};
use mantle_reth_preconf::PreconfConfig;
use mantle_reth_rpc_ext::PreconfTxEvent;
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

//! Regression test for the `eth_estimateTotalFee` cross-client `token_ratio` mismatch.
//!
//! ## The bug
//!
//! `MantleRpcExt::estimate_total_fee` (mantle-reth-rpc-ext) must read the `GasPriceOracle`
//! `token_ratio` (slot 0 of `0x42..0F`) from the **resolved/target block's post-state**, the
//! same way op-geth's `EstimateTotalFee` does (`StateAndHeaderByNumberOrHash(N)` →
//! `NewL1CostFuncArsia`). The buggy revision read it from the **parent** block:
//!
//! ```ignore
//! self.provider().state_by_block_hash(header.parent_hash())   // BUG: start-of-block-N value
//! self.provider().state_by_block_id(block_id)                  // FIX: end-of-block-N value
//! ```
//!
//! On every block this is invisible — except the exact block `N` where `token_ratio` changes:
//! there `post-state(N-1) != post-state(N)`, so the buggy node prices the L1 data fee with the
//! stale (pre-update) ratio while geth uses the updated one. Before/after `N` the two agree.
//! On Mantle Sepolia this surfaced at block 597707 (ratio 0xc9f → 0xc98).
//!
//! ## Why this must be a full-node test (not a mock)
//!
//! `reth_provider::test_utils::MockEthProvider` returns `self.clone()` for *every* block id and
//! serves `storage()` from a single map — it ignores the block entirely. A mock-based test would
//! therefore read the *same* ratio for parent and target and **pass on both the buggy and the
//! fixed code**, giving false confidence. Only a real mined chain has genuine per-block historical
//! state, so we launch a node, mine a `token_ratio` transition, and drive the real RPC.
//!
//! ## Strategy
//!
//! * Genesis places slot-0-mutating bytecode at the `GasPriceOracle` predeploy (`0x42..0F`) with an
//!   initial `token_ratio`, so a plain call to it changes slot 0.
//! * Every mined block carries a fixed Arsia L1-attributes deposit (selector `0x49e72383` + 174
//!   bytes) as its first tx, so `extract_l1_info` succeeds and the L1 data fee is non-zero and
//!   `token_ratio`-scaled (otherwise the buggy branch is never even entered).
//! * Block `N` additionally includes a user tx to `0x42..0F` that flips slot 0 → `token_ratio`
//!   differs between `post-state(N-1)` and `post-state(N)`.
//! * Assertions (all via the real `eth_estimateTotalFee` RPC):
//!   1. guard — the L1/ratio path is live at both sampled blocks (estimate strictly greater than
//!      the pure-L2 fee), so the test cannot silently stop covering the bug;
//!   2. absolute — the L1 data fee component of each estimate is *exactly proportional* to the
//!      on-chain `token_ratio` read at that same block (the ratio is the last, multiplicative
//!      factor of the Arsia L1 cost): `l1(N-1) * ratio(N) == l1(N) * ratio(N-1)`. The buggy node
//!      prices `N` with the parent's stale ratio, so `l1(N) == l1(N-1)` and the cross-product
//!      breaks by exactly the ratio delta;
//!   3. consistency — the transition block `N` and the next stable block `N+1` carry the same
//!      (post-update) ratio, so their estimates must be identical.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p mantle-reth-integration-tests --test estimate_total_fee_token_ratio -- --nocapture
//! ```

use alloy_genesis::{Genesis, GenesisAccount};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B64, B256, Bytes, TxKind, U256, address, hex};
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::client::ClientT;
use mantle_reth_cli::node::MantleNode;
use op_alloy_consensus::TxDeposit;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_chainspec::EthChainSpec;
use reth_db::test_utils::create_test_rw_db_with_path;
use reth_e2e_test_utils::{
    node::NodeTestContext, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_api::TreeConfig;
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig};
use reth_node_core::args::{DatadirArgs, RpcServerArgs};
use reth_optimism_node::payload::OpPayloadAttrs;
use reth_provider::providers::BlockchainProvider;
use reth_tasks::Runtime;
use std::sync::Arc;

/// `GasPriceOracle` predeploy — `token_ratio` lives at slot 0.
const GAS_ORACLE: Address = address!("420000000000000000000000000000000000000F");
/// `L1Block` predeploy — recipient of the L1-attributes deposit.
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

const INITIAL_RATIO: u64 = 3000;
/// Bytecode that, on every call, sets slot 0 to (slot0 - 4): decrements `token_ratio`.
/// PUSH1 0x00; SLOAD; PUSH1 0x04; SWAP1; SUB; PUSH1 0x00; SSTORE; STOP
const ORACLE_DECREMENT_BY_4: [u8; 11] =
    [0x60, 0x00, 0x54, 0x60, 0x04, 0x90, 0x03, 0x60, 0x00, 0x55, 0x00];

/// Builds the Arsia L1-attributes deposit calldata: 4-byte selector + 174-byte payload.
/// Non-zero `base_fee_scalar` and `l1_base_fee` make the L1 data fee strictly positive so the
/// `token_ratio` multiply is observable; operator-fee fields are left zero to keep the math clean.
fn arsia_l1_attributes_calldata() -> Bytes {
    let mut data = vec![0u8; 178];
    data[0..4].copy_from_slice(&hex!("49e72383")); // L1_BLOCK_ARSIA_SELECTOR
    let p = &mut data[4..]; // 174-byte jovian/arsia payload
    p[0..4].copy_from_slice(&1_000_000u32.to_be_bytes()); // base_fee_scalar
    p[4..8].copy_from_slice(&0u32.to_be_bytes()); // blob_base_fee_scalar
    p[32..64].copy_from_slice(&U256::from(1_000_000_000u64).to_be_bytes::<32>()); // l1_base_fee
    // p[64..96] l1_blob_base_fee = 0
    // p[160..164] operator_fee_scalar = 0, p[164..172] operator_fee_constant = 0
    // p[172..174] da_footprint_gas_scalar = 0
    data.into()
}

/// Encodes the per-block L1-attributes deposit as a 2718 envelope for the payload attributes.
fn l1_attributes_deposit_bytes() -> Bytes {
    let dep = TxDeposit {
        source_hash: B256::ZERO,
        from: Address::ZERO,
        to: TxKind::Call(L1_BLOCK),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        is_system_transaction: true,
        input: arsia_l1_attributes_calldata(),
        eth_value: 0,
        eth_tx_value: None,
    };
    dep.encoded_2718().into()
}

/// Payload attributes generator that injects the L1-attributes deposit as the first
/// (sequencer) transaction of every block, so `extract_l1_info` always succeeds.
fn attrs_with_l1_deposit(timestamp: u64) -> OpPayloadAttrs {
    OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
        },
        transactions: Some(vec![l1_attributes_deposit_bytes()]),
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(0),
    })
}

/// Genesis with the `GasPriceOracle` predeploy carrying the decrement bytecode + initial ratio.
fn chain_spec_with_oracle() -> Arc<reth_optimism_chainspec::OpChainSpec> {
    let mut genesis: Genesis =
        serde_json::from_str(include_str!("assets/genesis.json")).expect("valid genesis JSON");

    let mut storage = std::collections::BTreeMap::new();
    storage.insert(B256::ZERO, B256::from(U256::from(INITIAL_RATIO))); // slot 0 = token_ratio
    genesis.alloc.insert(
        GAS_ORACLE,
        GenesisAccount {
            code: Some(Bytes::from_static(&ORACLE_DECREMENT_BY_4)),
            storage: Some(storage),
            balance: U256::ZERO,
            ..Default::default()
        },
    );

    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

// Previously flaky under the engine-only e2e harness: it builds the engine but not the staged-sync
// pipeline, so the `AccountsHistory`/`StoragesHistory` indices are never populated and a *by-number
// historical* read for a freshly mined block intermittently errors (`HeaderNotFound`, the number→
// hash index lags the canonical commit) or silently returns the head state. Reading a *past* block
// (e.g. block 1 after block 3 exists) was therefore unreliable, and `sync_to`/waiting did not help
// because the per-block historical reconstruction itself is unavailable in this configuration.
//
// Fix: take every read while its block is the canonical head (the `latest` tag) and cache it,
// after `sync_to` confirms the head has settled. Head state is always present and consistent, so
// the reads are reliable; the bug under test is still exercised (`estimate_total_fee` resolves
// `latest` to the concrete head number and prices `token_ratio` from that block's post-state).
#[tokio::test]
async fn estimate_total_fee_uses_target_block_token_ratio() {
    reth_tracing::init_test_tracing();

    let chain_spec = chain_spec_with_oracle();
    let chain_id = chain_spec.chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);

    let mut config: NodeConfig<reth_optimism_chainspec::OpChainSpec> = NodeConfig::new(chain_spec)
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
    let runtime = Runtime::test();
    let node_handle = NodeBuilder::new(config)
        .with_database(db)
        .with_types_and_provider::<MantleNode, BlockchainProvider<_>>()
        .with_components(MantleNode::default().components())
        .with_add_ons(MantleNode::default().add_ons())
        .launch_with_fn(|builder| {
            // Keep the test's few blocks in the in-memory canonical chain (never persist/evict
            // them within the test): a high persistence threshold means historical state is always
            // served via the `MemoryOverlayStateProvider` (forward in-memory diffs), which does NOT
            // depend on the `StoragesHistory` index. The engine-only harness does not populate that
            // index, so once a block is evicted to the DB-historical path its by-number historical
            // read falls back to the head state (the stale-ratio flakiness). Keeping blocks in
            // memory sidesteps that path entirely.
            // threshold 8 (> the 3 blocks we mine, < the default backpressure threshold of 16):
            // the engine never persists/evicts within the test, so all blocks stay in memory.
            let tree_config = TreeConfig::default().with_persistence_threshold(8);
            let launcher =
                EngineNodeLauncher::new(runtime.clone(), builder.config.datadir(), tree_config);
            builder.launch_with(launcher)
        })
        .await
        .expect("MantleNode failed to launch");

    let mut node = NodeTestContext::new(node_handle.node, attrs_with_l1_deposit).await.unwrap();
    let client = node.inner.rpc_server_handle().http_client().expect("HTTP RPC enabled");

    // Blocks are mined deterministically from genesis (block 0): each `advance_block()` produces
    // exactly one block, so the numbers are 1 (pre), 2 (transition), 3 (stable). They remain in
    // the in-memory canonical chain and are queryable by number for the assertions below.

    let req = serde_json::json!({
        "from": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
        "to": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
        "value": "0x0",
        "data": "0x",
        "gasPrice": "0xba43b7400",
        "chainId": format!("0x{chain_id:x}"),
    });

    // Why every read below targets the "latest" tag (the head block) and is taken *immediately*
    // after that block is mined:
    //
    // This is an engine-only e2e harness — it runs the engine but not the staged-sync pipeline, so
    // the `AccountsHistory`/`StoragesHistory` changeset indices are never built. Reconstructing a
    // *past* block's state therefore has no reliable source, and a by-number historical read for a
    // recently mined block intermittently either errors (`HeaderNotFound`, the number→hash index
    // lags the canonical commit) or silently falls back to the head state. That is the flakiness:
    // e.g. `getStorageAt(block 1)` taken after block 3 exists sometimes returns block 3's ratio.
    // Waiting/`sync_to` does NOT fix it because it is not purely a settle delay — the per-block
    // historical reconstruction itself is unreliable in this configuration.
    //
    // The reliable path is the head state: when block N is the canonical head, `state_by_block_id`
    // resolves it to the in-memory head state, which is always present and consistent. So we read
    // each value while its block is still the head (via the `latest` tag) and cache it. The bug
    // under test is still exercised: `estimate_total_fee` resolves `latest` to the concrete head
    // number and reads `token_ratio` from that block's post-state — the buggy revision reads the
    // parent's (stale) ratio, the fixed one reads the head's, exactly as with an explicit number.
    async fn request_retry(
        client: &jsonrpsee::http_client::HttpClient,
        method: &str,
        params: Vec<serde_json::Value>,
        what: &str,
    ) -> U256 {
        use jsonrpsee::core::client::Error;
        let mut last_transport_err = None;
        for _ in 0..40 {
            match client.request::<U256, _>(method, params.clone()).await {
                Ok(v) => return v,
                // A JSON-RPC error *response* from the server is deterministic — e.g. the
                // `estimate_total_fee` state-read error this suite is built around. Surface it
                // immediately with full detail instead of spinning for 20s and reporting a
                // generic timeout that hides the real failure.
                Err(Error::Call(e)) => {
                    panic!("{what}: RPC returned an error response: {e}");
                }
                // Transport / connection errors are transient while the node's RPC comes up and
                // the freshly mined head settles — retry, but keep the last one for the report.
                Err(e) => {
                    last_transport_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
        panic!(
            "{what} did not succeed within timeout (40 x 500ms); last transport error: {}",
            last_transport_err.map_or_else(|| "<none>".to_string(), |e| e.to_string()),
        );
    }
    // Read `token_ratio` (slot 0 of the oracle) at the current head.
    let ratio_head = || {
        let client = client.clone();
        async move {
            request_retry(
                &client,
                "eth_getStorageAt",
                vec![
                    serde_json::json!(format!("0x{GAS_ORACLE:x}")),
                    serde_json::json!("0x0"),
                    serde_json::json!("latest"),
                ],
                "getStorageAt at head",
            )
            .await
        }
    };
    // `eth_estimateTotalFee` at the current head.
    let estimate_head = || {
        let client = client.clone();
        let req = req.clone();
        async move {
            request_retry(
                &client,
                "eth_estimateTotalFee",
                vec![req, serde_json::json!("latest")],
                "estimateTotalFee at head",
            )
            .await
        }
    };

    // Block 1: only the L1-attributes deposit → token_ratio stays at INITIAL_RATIO.
    let payload1 = node.advance_block().await.expect("mine block 1");
    node.sync_to(payload1.block().hash()).await.expect("settle block 1 as head");
    let ratio_pre = ratio_head().await; // head == block 1 → INITIAL_RATIO
    let total_pre = estimate_head().await; // estimateTotalFee priced with block 1's post-state

    // Block 2 (= transition N): inject a call to the oracle so it SSTOREs a new ratio.
    let oracle_call = TransactionTestContext::sign_tx(
        wallet.inner.clone(),
        TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(TxKind::Call(GAS_ORACLE)),
            gas: Some(100_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            value: Some(U256::ZERO),
            input: TransactionInput::from(Bytes::new()),
            ..Default::default()
        },
    )
    .await;
    node.rpc.inject_tx(oracle_call.encoded_2718().into()).await.expect("oracle call accepted");
    let payload2 = node.advance_block().await.expect("mine block 2 (transition)");
    node.sync_to(payload2.block().hash()).await.expect("settle block 2 as head");
    let ratio_n = ratio_head().await; // head == block 2 → post-update ratio
    let total_n = estimate_head().await; // estimateTotalFee priced with block 2's post-state

    // Block 3 (= N+1): deposit only → token_ratio stays at the post-update value (stable
    // reference).
    let payload3 = node.advance_block().await.expect("mine block 3");
    node.sync_to(payload3.block().hash()).await.expect("settle block 3 as head");
    let total_post = estimate_head().await; // estimateTotalFee priced with block 3's post-state
    let l2_gas = request_retry(
        &client,
        "eth_estimateGas",
        vec![req.clone(), serde_json::json!("latest")],
        "estimateGas at head",
    )
    .await;
    let l2_only = l2_gas.saturating_mul(U256::from(0xba43b7400u64));

    // Guard: the oracle call must actually have changed `token_ratio` between N-1 and N (else the
    // transition never happened and the core assertion would pass vacuously).
    assert_ne!(
        ratio_pre, ratio_n,
        "transition block must have changed token_ratio away from {INITIAL_RATIO}"
    );

    // Pure-L2 reference: a zero-ratio block would have no L1 data fee. Guard that the L1 path is
    // live at *both* sampled blocks (strictly positive L1 data fee) — this keeps the test covering
    // the bug and makes the `total - l2_only` subtractions below underflow-safe.
    assert!(
        total_pre > l2_only,
        "guard: L1/token_ratio path must be live at block 1 (total {total_pre} > L2-only {l2_only})"
    );
    assert!(
        total_n > l2_only,
        "guard: L1/token_ratio path must be live at block 2 (total {total_n} > L2-only {l2_only})"
    );

    // Operator fee is configured to zero and the L2 component (gas * gasPrice) is block-independent
    // for this fixed request, so subtracting the pure-L2 fee isolates the L1 data fee at each
    // block.
    let l1_pre = total_pre - l2_only;
    let l1_n = total_n - l2_only;

    // Absolute assertion (not just N-vs-N+1 consistency): the Arsia L1 data fee is *exactly linear*
    // in token_ratio (the ratio is its last, multiplicative factor), and `estimate_total_fee` reads
    // that ratio from the target block's post-state. So the L1 component of each estimate must be
    // exactly proportional to the on-chain ratio read at that same block:
    //     l1_pre / ratio_pre == l1_n / ratio_n   ⟺   l1_pre * ratio_n == l1_n * ratio_pre
    // This pins the estimate to the real on-chain ratio, not merely to another estimate. The buggy
    // node prices block N with the parent's stale ratio (== ratio_pre), making l1_n == l1_pre, so
    // the cross-product breaks by exactly the ratio delta.
    assert_eq!(
        l1_pre * ratio_n,
        l1_n * ratio_pre,
        "L1 data fee must be exactly proportional to the on-chain token_ratio at each block \
         (block 1: fee {l1_pre} @ ratio {ratio_pre}; block 2: fee {l1_n} @ ratio {ratio_n}); \
         a buggy node prices block 2 with the parent's stale ratio, breaking proportionality"
    );

    // Secondary consistency: the transition block N and the next stable block N+1 carry the same
    // (post-update) ratio, so their estimates must be identical. Orthogonal to the check above — it
    // also guards against N+1 itself being mispriced.
    assert_eq!(
        total_n, total_post,
        "estimateTotalFee at transition block 2 must equal stable block 3 (both use the \
         post-update token_ratio); a buggy node prices block 2 with the parent's stale ratio"
    );
}

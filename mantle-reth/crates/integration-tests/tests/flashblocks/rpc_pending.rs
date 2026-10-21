//! `eth_` overrides answering from the pending overlay.
//!
//! The override module is built over the launched node's own `EthApi` and the
//! test's [`FlashblocksState`], the same pair `MantleNode::add_ons` wires up, and
//! its methods are called directly rather than over JSON-RPC.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rpc_types_eth::{BlockNumberOrTag, BlockOverrides, Filter};
use mantle_reth_flashblocks::EthApiOverrideServer;

use crate::consumer_helpers::{
    DEPLOY_INIT_CODE, DEPLOYED_RUNTIME, FlashblockBuilder, LOGGER, SETTER, SETTER_SLOT,
    SETTER_UNWRITTEN_SLOT, SETTER_UNWRITTEN_VALUE, SETTER_VALUE, l1_info_deposit,
    launch_flashblocks_consumer, signed_call, signed_deploy, test_sender,
};

/// Value the overlay's user transaction moves into [`LOGGER`].
const VALUE: U256 = U256::from_limbs([7_000_000_000_000_000_000, 0, 0, 0]);

/// The override module bound to the node's `EthApi` and the harness's state.
macro_rules! eth_api {
    ($node:expr, $harness:expr) => {
        mantle_reth_flashblocks::EthApiExt::new(
            $node.rpc.inner.eth_api().clone(),
            $node.rpc.inner.eth_handlers().filter.clone(),
            std::sync::Arc::clone($harness.state()),
        )
    };
}

/// `"pending"` as the override trait's block tag.
fn pending_tag() -> mantle_reth_flashblocks::BlockNumberOrTagExt {
    serde_json::from_value(serde_json::json!("pending")).expect("pending is a valid tag")
}

/// Opens an overlay at block 1 holding the L1-attributes deposit and one call to
/// [`LOGGER`] carrying [`VALUE`]; returns the call's hash.
macro_rules! overlay_with_user_call {
    ($harness:expr) => {{
        let raw = signed_call(0, LOGGER, VALUE).await;
        let tx_hash = keccak256(&raw);
        $harness
            .send(FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw]).build())
            .await;
        assert!($harness.pending().is_some(), "the slice should open an overlay");
        tx_hash
    }};
}

/// Installs a small EVM probe without changing the canonical test contracts.
fn probe_override(code: &str) -> alloy_rpc_types_eth::state::StateOverride {
    serde_json::from_value(serde_json::json!({
        format!("{LOGGER:#x}"): { "code": code }
    }))
    .expect("valid probe bytecode")
}

/// A consumer answers pending nonce from its own overlay; transactions the
/// sequencer accepted but has not yet sliced are not visible to it.
#[tokio::test(flavor = "multi_thread")]
async fn consumer_pending_nonce_reads_only_the_local_overlay() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);
    node.rpc.inject_tx(signed_call(0, LOGGER, U256::ZERO).await).await.unwrap();
    assert_eq!(
        api.get_transaction_count(test_sender(), Some(BlockNumberOrTag::Pending.into()))
            .await
            .unwrap(),
        U256::ZERO,
        "without an overlay, pending is the canonical nonce"
    );
    overlay_with_user_call!(harness);
    assert_eq!(
        api.get_transaction_count(test_sender(), Some(BlockNumberOrTag::Pending.into()))
            .await
            .unwrap(),
        U256::from(1)
    );
}

/// Receipt fees use the token ratio immediately before each transaction.
#[tokio::test(flavor = "multi_thread")]
async fn pending_receipt_fees_follow_token_ratio_changes_across_slices() {
    use op_revm::constants::{GAS_ORACLE_CONTRACT, TOKEN_RATIO_SLOT};
    use reth_chainspec::EthChainSpec;
    let base = crate::consumer_helpers::flashblocks_test_chain_spec();
    let mut genesis = base.genesis().clone();
    genesis.alloc.insert(
        GAS_ORACLE_CONTRACT,
        alloy_genesis::GenesisAccount {
            // Update the ratio and emit TokenRatioUpdated(uint256,uint256), as the
            // canonical receipt converter uses its logs to track mid-block changes.
            code: Some(
                format!(
                    "0x6102bc7f{TOKEN_RATIO_SLOT:064x}556102bc6101f47f{:x}60006000a300",
                    keccak256("TokenRatioUpdated(uint256,uint256)"),
                )
                .parse()
                .unwrap(),
            ),
            storage: Some([(B256::from(TOKEN_RATIO_SLOT), B256::from(U256::from(500)))].into()),
            ..Default::default()
        },
    );
    let spec = std::sync::Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis));
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3, spec);
    let api = eth_api!(node, harness);
    let first = signed_call(0, LOGGER, U256::ZERO).await;
    let update = signed_call(1, GAS_ORACLE_CONTRACT, U256::ZERO).await;
    let after = signed_call(2, LOGGER, U256::ZERO).await;
    harness
        .send(
            FlashblockBuilder::new(1, 0)
                .transactions(vec![l1_info_deposit(1), first.clone(), update.clone()])
                .build(),
        )
        .await;
    harness
        .send(FlashblockBuilder::new(1, 1).prev(1, 0).transactions(vec![after.clone()]).build())
        .await;
    let mut fees = vec![];
    let mut previews = vec![];
    for (raw, ratio) in [(&first, 500), (&update, 500), (&after, 700)] {
        let receipt = api.get_transaction_receipt(keccak256(raw)).await.unwrap().unwrap();
        assert_eq!(receipt["tokenRatio"], format!("{ratio:#x}"));
        let fee: U256 = serde_json::from_value(receipt["l1Fee"].clone()).unwrap();
        assert!(fee > U256::ZERO, "a nonzero L1 fee must not disappear in preview receipts");
        fees.push(fee);
        previews.push(receipt);
    }
    assert!(fees[2] > fees[0], "the same call with a higher ratio costs more");
    for raw in [&first, &update, &after] {
        node.rpc.inject_tx(raw.clone()).await.unwrap();
    }
    crate::consumer_helpers::mine_canonical_block!(node);
    for (raw, preview) in [&first, &update, &after].into_iter().zip(previews) {
        let canonical = api.get_transaction_receipt(keccak256(raw)).await.unwrap().unwrap();
        for field in [
            "l1Fee",
            "l1GasUsed",
            "l1GasPrice",
            "l1BaseFeeScalar",
            "l1BlobBaseFeeScalar",
            "tokenRatio",
        ] {
            assert_eq!(preview[field], canonical[field], "receipt field {field}");
        }
    }
}

/// State and EVM block fields must describe the same pending snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn pending_execution_uses_the_overlay_block_environment() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);
    let mut slice = FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1)]).build();
    let base = slice.base.as_mut().unwrap();
    base.fee_recipient = Address::repeat_byte(0x11);
    base.prev_randao = B256::repeat_byte(0x22);
    base.base_fee_per_gas = U256::from(123);
    harness.send(slice).await;
    let header = harness.pending().expect("overlay").latest_header();
    // NUMBER, TIMESTAMP, GASLIMIT, COINBASE, PREVRANDAO, BASEFEE, BLOCKHASH(number-1).
    let state = probe_override(
        "0x43600052426020524560405241606052446080524860a052600143034060c05260e06000f3",
    );
    let request = serde_json::json!({ "from": test_sender(), "to": LOGGER, "gasPrice": "0x1" });
    let result = api
        .call(request.clone(), Some(BlockNumberOrTag::Pending.into()), Some(state.clone()), None)
        .await
        .expect("probe call");
    let words: Vec<_> = result.chunks_exact(32).map(U256::from_be_slice).collect();
    assert_eq!(
        words,
        vec![
            U256::from(header.number),
            U256::from(header.timestamp),
            U256::from(header.gas_limit),
            U256::from_be_slice(header.beneficiary.as_slice()),
            U256::from_be_slice(header.mix_hash.as_slice()),
            U256::from(123),
            U256::from_be_slice(header.parent_hash.as_slice()),
        ]
    );
    let user = BlockOverrides {
        number: Some(U256::from(99)),
        time: Some(header.timestamp + 10),
        ..Default::default()
    };
    let result = api
        .call(request, Some(BlockNumberOrTag::Pending.into()), Some(state), Some(Box::new(user)))
        .await
        .expect("user block overrides");
    assert_eq!(U256::from_be_slice(&result[..32]), U256::from(99));
    assert_eq!(U256::from_be_slice(&result[32..64]), U256::from(header.timestamp + 10));
}

/// Gas estimation must follow the pending contract's NUMBER-dependent branch.
#[tokio::test(flavor = "multi_thread")]
async fn pending_estimation_uses_the_overlay_block_number() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);
    overlay_with_user_call!(harness);
    // Revert unless NUMBER == 1.
    let state = probe_override("0x43600114600c5760006000fd5b00");
    let request = serde_json::json!({ "to": LOGGER });
    let gas = api
        .estimate_gas(
            request.clone(),
            Some(BlockNumberOrTag::Pending.into()),
            Some(state.clone()),
            None,
        )
        .await
        .expect("pending NUMBER is 1");
    assert!(gas >= U256::from(21_000));
    assert!(
        api.estimate_gas(request, Some(BlockNumberOrTag::Latest.into()), Some(state), None)
            .await
            .is_err()
    );
}

/// Pending total-fee estimates work with no consumer snapshot or pending provider block.
#[tokio::test(flavor = "multi_thread")]
async fn pending_total_fee_falls_back_without_a_preview() {
    use jsonrpsee::core::client::ClientT;
    use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
    use op_revm::constants::{GAS_ORACLE_CONTRACT, TOKEN_RATIO_SLOT};
    use reth_chainspec::EthChainSpec;
    let mut genesis = crate::consumer_helpers::flashblocks_test_chain_spec().genesis().clone();
    genesis.alloc.insert(
        GAS_ORACLE_CONTRACT,
        alloy_genesis::GenesisAccount {
            storage: Some([(B256::from(TOKEN_RATIO_SLOT), B256::from(U256::from(500)))].into()),
            ..Default::default()
        },
    );
    let spec = std::sync::Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis));
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3, spec);
    let api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        None,
        None,
    )
    .with_pending_overrides(Some(harness.state().clone()));
    let request = serde_json::json!({
        "from": test_sender(), "to": Address::repeat_byte(0x77), "gasPrice": "0x1"
    });
    let latest = api
        .estimate_total_fee(
            serde_json::from_value(request.clone()).unwrap(),
            Some(BlockNumberOrTag::Latest.into()),
        )
        .await
        .unwrap();
    assert_eq!(latest, U256::from(21_000));
    assert_eq!(
        api.estimate_total_fee(
            serde_json::from_value(request.clone()).unwrap(),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("pending without a consumer snapshot"),
        latest
    );
    // The node's registered Mantle RPC has no attached consumer overlay either.
    let total: U256 = node
        .inner
        .rpc_server_handle()
        .http_client()
        .unwrap()
        .request("eth_estimateTotalFee", jsonrpsee::rpc_params![request.clone(), "pending"])
        .await
        .expect("registered RPC without a consumer overlay");
    assert_eq!(total, latest);
    crate::consumer_helpers::mine_canonical_block!(node);
    let latest = api
        .estimate_total_fee(
            serde_json::from_value(request.clone()).unwrap(),
            Some(BlockNumberOrTag::Latest.into()),
        )
        .await
        .unwrap();
    assert!(latest > U256::from(21_000), "the canonical L1 fee must be nonzero");
    assert_eq!(
        api.estimate_total_fee(
            serde_json::from_value(request).unwrap(),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("fallback with canonical L1 fee parameters"),
        latest
    );
}

/// A provider pending block remains usable when local pending construction is disabled.
#[tokio::test(flavor = "multi_thread")]
async fn pending_total_fee_uses_actual_pending_when_local_pending_is_disabled() {
    use jsonrpsee::core::client::ClientT;
    use reth_provider::BlockReader;
    let (mut node, client, _, _) = mantle_reth_integration_tests::launch_mantle_node!(
        crate::consumer_helpers::flashblocks_test_chain_spec(),
        mantle_reth_cli::node::MantleNode::default(),
        crate::consumer_helpers::flashblocks_payload_attributes,
        persist_each_block = false,
        rpc_pending_block = "none".parse().unwrap()
    )
    .await;
    let init = "600e600c600039600e6000f343600114600c5760006000fd5b00".parse().unwrap();
    node.rpc.inject_tx(signed_deploy(0, init).await).await.unwrap();
    node.build_and_submit_payload().await.expect("build and execute without canonicalizing");
    assert_eq!(
        node.inner
            .provider
            .pending_block()
            .unwrap()
            .expect("real provider pending")
            .header()
            .number,
        1
    );
    let request = serde_json::json!({
        "from": test_sender(), "to": test_sender().create(0), "gasPrice": "0x1"
    });
    let pending: U256 = client
        .request("eth_estimateTotalFee", jsonrpsee::rpc_params![request.clone(), "pending"])
        .await
        .expect("estimate against the actual pending deployment and NUMBER");
    assert!(pending > U256::from(21_000), "pending contract must execute");
    let latest: U256 = client
        .request("eth_estimateTotalFee", jsonrpsee::rpc_params![request, "latest"])
        .await
        .unwrap();
    assert_eq!(latest, U256::from(21_000), "deployment is not canonical");
}

/// Deployed preview code must execute using its own block's NUMBER.
#[tokio::test(flavor = "multi_thread")]
async fn pending_total_fee_uses_preview_code_and_block_environment() {
    use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    // Runtime reverts unless NUMBER == 1. This contract exists only in the preview.
    let init = "600e600c600039600e6000f343600114600c5760006000fd5b00".parse().unwrap();
    let mut slice = FlashblockBuilder::new(1, 0)
        .transactions(vec![l1_info_deposit(1), signed_deploy(0, init).await])
        .build();
    slice.base.as_mut().unwrap().base_fee_per_gas = U256::from(123);
    harness.send(slice).await;
    let api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        None,
        None,
    )
    .with_pending_overrides(Some(harness.state().clone()));
    let request = serde_json::json!({
        "from": test_sender(), "to": test_sender().create(0), "gasPrice": "0x1"
    });
    let gas = eth_api!(node, harness)
        .estimate_gas(request.clone(), Some(BlockNumberOrTag::Pending.into()), None, None)
        .await
        .expect("existing pending gas estimator");
    assert!(gas > U256::from(21_000), "the deployed contract must actually run");
    assert_eq!(
        api.estimate_total_fee(
            serde_json::from_value(request.clone()).unwrap(),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("total fee at preview state and NUMBER"),
        gas
    );
    let dynamic_fee = serde_json::json!({
        "from": test_sender(), "to": test_sender().create(0),
        "maxFeePerGas": "0x3e8", "maxPriorityFeePerGas": "0x0"
    });
    assert_eq!(
        api.estimate_total_fee(
            serde_json::from_value(dynamic_fee).unwrap(),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("preview base fee"),
        gas * U256::from(123)
    );
    assert_eq!(
        api.estimate_total_fee(
            serde_json::from_value(request).unwrap(),
            Some(BlockNumberOrTag::Latest.into()),
        )
        .await
        .unwrap(),
        U256::from(21_000),
        "latest still sees an undeployed address"
    );
}

/// L1 attributes come from the preview tip and tokenRatio from its post-state.
#[tokio::test(flavor = "multi_thread")]
async fn pending_total_fee_tracks_ratio_and_fee_parameters_across_slices() {
    use alloy_network::eip2718::{Decodable2718, Encodable2718};
    use mantle_reth_flashblocks::{PendingExecutionContext, PendingStateOverrides};
    use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
    use op_revm::constants::{GAS_ORACLE_CONTRACT, TOKEN_RATIO_SLOT};
    use reth_chainspec::EthChainSpec;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Debug)]
    struct AdvancingPreview {
        first: PendingExecutionContext,
        next: PendingExecutionContext,
        reads: AtomicUsize,
    }
    impl PendingStateOverrides for AdvancingPreview {
        fn pending_execution_context(&self) -> Option<PendingExecutionContext> {
            Some(if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first.clone()
            } else {
                self.next.clone()
            })
        }
    }

    fn fee_deposit(origin: u64, scalar: u32, constant: u64, l1_base_fee: u64) -> Bytes {
        let raw = l1_info_deposit(origin);
        let op_alloy_consensus::OpTxEnvelope::Deposit(tx) =
            op_alloy_consensus::OpTxEnvelope::decode_2718(&mut raw.as_ref()).unwrap()
        else {
            panic!("L1 attributes must be a deposit")
        };
        let mut tx = tx.inner().clone();
        let mut input = tx.input.to_vec();
        input[36..68].copy_from_slice(&U256::from(l1_base_fee).to_be_bytes::<32>());
        input[164..168].copy_from_slice(&scalar.to_be_bytes());
        input[168..176].copy_from_slice(&constant.to_be_bytes());
        tx.input = input.into();
        tx.encoded_2718().into()
    }

    let mut genesis = crate::consumer_helpers::flashblocks_test_chain_spec().genesis().clone();
    genesis.alloc.insert(
        GAS_ORACLE_CONTRACT,
        alloy_genesis::GenesisAccount {
            code: Some("0x6102bc60005500".parse().unwrap()), // slot 0 = 700
            storage: Some([(B256::from(TOKEN_RATIO_SLOT), B256::from(U256::from(500)))].into()),
            ..Default::default()
        },
    );
    let poor_sender = Address::repeat_byte(0x66);
    genesis.alloc.insert(
        poor_sender,
        alloy_genesis::GenesisAccount { balance: U256::from(1_000_000), ..Default::default() },
    );
    let spec = std::sync::Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis));
    let (harness, node) = launch_flashblocks_consumer!(3, 3, spec);
    let api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        None,
        None,
    )
    .with_pending_overrides(Some(harness.state().clone()));
    let request = serde_json::json!({ "from": test_sender(), "to": Address::repeat_byte(0x77), "gasPrice": "0x1" });
    let estimate = || {
        api.estimate_total_fee(
            serde_json::from_value(request.clone()).unwrap(),
            Some(BlockNumberOrTag::Pending.into()),
        )
    };
    harness
        .send(
            FlashblockBuilder::new(1, 0)
                .transactions(vec![fee_deposit(1, 2, 7, 1_000_000_000)])
                .build(),
        )
        .await;
    let first = estimate().await.expect("first preview fee");
    let captured = harness.state().pending_execution_context().unwrap();
    // Transfer costs exactly 21,000 gas; operator fee = 21,000 * 2 * 100 + 7.
    let fixed = U256::from(21_000 + 4_200_007);
    assert!(first > fixed, "nonzero L1 fee must be included");
    let error = api
        .estimate_total_fee(
            serde_json::from_value(serde_json::json!({
                "from": poor_sender, "to": Address::repeat_byte(0x77), "gasPrice": "0x1"
            }))
            .unwrap(),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect_err("L2 gas alone is affordable, but operator fee is not");
    assert!(error.message().contains("insufficient funds"), "{error}");
    harness
        .send(
            FlashblockBuilder::new(1, 1)
                .prev(1, 0)
                .transactions(vec![signed_call(0, GAS_ORACLE_CONTRACT, U256::ZERO).await])
                .build(),
        )
        .await;
    let updated = estimate().await.expect("updated preview ratio");
    assert_eq!(
        (first - fixed) * U256::from(700),
        (updated - fixed) * U256::from(500),
        "L1 fee must follow the preview's updated tokenRatio"
    );
    assert!(updated > first);
    harness
        .send(
            FlashblockBuilder::new(2, 0)
                .prev(1, 1)
                .transactions(vec![fee_deposit(2, 3, 11, 2_000_000_000)])
                .build(),
        )
        .await;
    let next = estimate().await.expect("next preview block's fee parameters");
    assert_eq!(
        next - U256::from(21_000 + 6_300_011),
        (updated - fixed) * U256::from(2),
        "latest preview L1 base fee and operator parameters must replace the preceding block's"
    );
    // The source offers a different real snapshot on the second read. A request
    // must retain its first capture for both gas estimation and fee parameters.
    let advancing = Arc::new(AdvancingPreview {
        first: captured,
        next: harness.state().pending_execution_context().unwrap(),
        reads: AtomicUsize::new(0),
    });
    let captured_api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        None,
        None,
    )
    .with_pending_overrides(Some(advancing.clone()));
    assert_eq!(
        captured_api
            .estimate_total_fee(
                serde_json::from_value(request.clone()).unwrap(),
                Some(BlockNumberOrTag::Pending.into()),
            )
            .await
            .expect("captured snapshot after newer slices"),
        first
    );
    assert_eq!(advancing.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        api.estimate_total_fee(
            serde_json::from_value(request).unwrap(),
            Some(BlockNumberOrTag::Latest.into()),
        )
        .await
        .unwrap(),
        U256::from(21_000),
        "preview must not affect canonical estimates"
    );
}

/// Without a provider pending block, the simulator must use the consumer snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn pending_simulation_starts_after_the_overlay_tip() {
    use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    overlay_with_user_call!(harness);
    let api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        None,
        None,
    )
    .with_pending_overrides(Some(harness.state().clone()));
    // Return NUMBER, TIMESTAMP and the overlay-funded LOGGER balance.
    let state = probe_override("0x43600052426020524760405260606000f3");
    let result = api
        .simulate_v1(
            serde_json::json!({
                "blockStateCalls": [{ "stateOverrides": state, "calls": [{ "to": LOGGER }] }]
            }),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("pending simulation");
    assert_eq!(result[0]["number"], "0x2");
    let output: Bytes =
        serde_json::from_value(result[0]["calls"][0]["returnData"].clone()).expect("return data");
    assert_eq!(U256::from_be_slice(&output[..32]), U256::from(2));
    assert_eq!(
        U256::from_be_slice(&output[32..64]),
        U256::from(crate::consumer_helpers::l2_ts(1) + 2)
    );
    // The user's code override replaces LOGGER's overlay account override, so probe a separate
    // call without it to verify that simulation also receives the pending state.
    let balances = api
        .simulate_v1(
            serde_json::json!({
                "blockStateCalls": [{ "calls": [{ "to": LOGGER }] }]
            }),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("pending balance simulation");
    let output: Bytes =
        serde_json::from_value(balances[0]["calls"][0]["returnData"].clone()).expect("balance");
    assert_eq!(U256::from_be_slice(&output), VALUE);
    let balances = api
        .simulate_v1(
            serde_json::json!({ "blockStateCalls": [{
        "stateOverrides": { format!("{LOGGER:#x}"): { "balance": "0xff" } },
        "calls": [{ "to": LOGGER }]
    }] }),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("user state overrides");
    let output: Bytes =
        serde_json::from_value(balances[0]["calls"][0]["returnData"].clone()).unwrap();
    assert_eq!(U256::from_be_slice(&output), U256::from(255));
    let canonical = api
        .simulate_v1(
            serde_json::json!({ "blockStateCalls": [{
        "calls": [{ "to": LOGGER }]
    }] }),
            Some(BlockNumberOrTag::Latest.into()),
        )
        .await
        .expect("canonical simulation");
    assert_eq!(canonical[0]["number"], "0x1");
    let output: Bytes =
        serde_json::from_value(canonical[0]["calls"][0]["returnData"].clone()).unwrap();
    assert_eq!(U256::from_be_slice(&output), U256::ZERO);
}

/// Pending simulation also works before the first slice and preserves simulated writes.
#[tokio::test(flavor = "multi_thread")]
async fn pending_simulation_falls_back_and_preserves_writes_between_blocks() {
    use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        None,
        None,
    )
    .with_pending_overrides(Some(harness.state().clone()));
    let result = api
        .simulate_v1(
            serde_json::json!({
                "blockStateCalls": [{ "calls": [{ "to": LOGGER }] }]
            }),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("fallback without a slice");
    assert_eq!(result[0]["number"], "0x1");
    harness
        .send(
            FlashblockBuilder::new(1, 0)
                .transactions(vec![l1_info_deposit(1), signed_call(0, SETTER, U256::ZERO).await])
                .build(),
        )
        .await;
    // Write calldata to slot 1 when nonempty; otherwise just read the slot.
    let code = "0x3615600b576000356001555b60015460005260206000f3";
    let user = serde_json::json!({ format!("{SETTER:#x}"): { "code": code } });
    let result = api.simulate_v1(serde_json::json!({ "blockStateCalls": [
        { "stateOverrides": user, "calls": [{ "to": SETTER, "data": format!("{:#066x}", U256::from(9)) }] },
        { "stateOverrides": user, "calls": [{ "to": SETTER }] }
    ] }), Some(BlockNumberOrTag::Pending.into())).await.expect("sequential simulation");
    for block in result.as_array().unwrap() {
        assert_eq!(block["calls"][0]["status"], "0x1");
        let output: Bytes =
            serde_json::from_value(block["calls"][0]["returnData"].clone()).unwrap();
        assert_eq!(
            U256::from_be_slice(&output),
            U256::from(9),
            "the next block must retain simulated storage"
        );
    }
}

/// A two-block preview remains anchored to genesis while executing at its latest header.
#[tokio::test(flavor = "multi_thread")]
async fn pending_execution_and_simulation_follow_a_multi_block_overlay() {
    use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    overlay_with_user_call!(harness);
    let published_parent = B256::repeat_byte(0x77);
    harness
        .send(
            FlashblockBuilder::new(2, 0)
                .prev(1, 0)
                .parent_hash(published_parent)
                .transactions(vec![l1_info_deposit(2)])
                .build(),
        )
        .await;
    let pending = harness.pending().expect("two-block overlay");
    assert_eq!(pending.earliest_block_number(), 1);
    assert_eq!(pending.latest_block_number(), 2);
    let api = eth_api!(node, harness);
    let result = api
        .call(
            serde_json::json!({ "to": LOGGER }),
            Some(BlockNumberOrTag::Pending.into()),
            Some(probe_override("0x4360005260014060205260406000f3")),
            None,
        )
        .await
        .expect("multi-block call");
    assert_eq!(U256::from_be_slice(&result[..32]), U256::from(2));
    assert_eq!(&result[32..], published_parent.as_slice());
    let user_hash = B256::repeat_byte(0x88);
    let result = api
        .call(
            serde_json::json!({ "to": LOGGER }),
            Some(BlockNumberOrTag::Pending.into()),
            Some(probe_override("0x60014060005260206000f3")),
            Some(Box::new(BlockOverrides {
                block_hash: Some([(1, user_hash)].into()),
                ..Default::default()
            })),
        )
        .await
        .expect("user block hash override");
    assert_eq!(result.as_ref(), user_hash.as_slice());
    let api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        None,
        None,
    )
    .with_pending_overrides(Some(harness.state().clone()));
    let result = api
        .simulate_v1(
            serde_json::json!({ "blockStateCalls": [{
        "calls": [{ "to": LOGGER }]
    }] }),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("multi-block simulation");
    assert_eq!(
        result.as_array().unwrap().len(),
        1,
        "preview blocks are not simulated again as fillers"
    );
    assert_eq!(result[0]["number"], "0x3");
    let output: Bytes =
        serde_json::from_value(result[0]["calls"][0]["returnData"].clone()).unwrap();
    assert_eq!(U256::from_be_slice(&output), VALUE);
}

/// A request can retain its snapshot while canonicalization advances. BLOCKHASH
/// must continue describing that captured preview, including its unsealed tip.
#[tokio::test(flavor = "multi_thread")]
async fn pending_simulation_keeps_captured_block_hashes_after_canonical_advances() {
    use crate::consumer_helpers::mine_canonical_block;
    use mantle_reth_flashblocks::{PendingExecutionContext, PendingStateOverrides};
    use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
    use reth_provider::{BlockHashReader, StateProviderFactory};
    #[derive(Debug)]
    struct Captured(PendingExecutionContext);
    impl PendingStateOverrides for Captured {
        fn pending_execution_context(&self) -> Option<PendingExecutionContext> {
            Some(self.0.clone())
        }
    }
    let (harness, mut node) = launch_flashblocks_consumer!(
        3,
        3,
        crate::consumer_helpers::flashblocks_test_chain_spec(),
        persist_each_block = true
    );
    overlay_with_user_call!(harness);
    let captured = harness.state().pending_execution_context().expect("captured preview");
    assert_eq!(captured.header.hash(), B256::ZERO, "the preview tip is not sealed");
    let canonical = mine_canonical_block!(node);
    assert_ne!(canonical.hash(), B256::ZERO);
    // Let the engine persist its own block. Writing static files here would
    // race its shutdown flush and append the same header twice.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let persisted = node
                .inner
                .provider
                .state_by_block_hash(captured.base_block.as_block_hash().unwrap())
                .unwrap()
                .block_hash(1)
                .unwrap();
            if persisted == Some(canonical.hash()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("canonical header persisted by the engine");
    assert_eq!(
        node.inner
            .provider
            .state_by_block_hash(captured.base_block.as_block_hash().unwrap())
            .unwrap()
            .block_hash(1)
            .unwrap(),
        Some(canonical.hash()),
        "the historical provider must expose the persisted canonical hash"
    );
    let api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        None,
        None,
    )
    .with_pending_overrides(Some(std::sync::Arc::new(Captured(captured))));
    let result = api
        .simulate_v1(
            serde_json::json!({ "blockStateCalls": [{
        "stateOverrides": probe_override("0x60014060005260206000f3"),
        "calls": [{ "to": LOGGER }]
    }] }),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("captured simulation");
    let output: Bytes =
        serde_json::from_value(result[0]["calls"][0]["returnData"].clone()).unwrap();
    assert_eq!(
        output.as_ref(),
        B256::ZERO.as_slice(),
        "BLOCKHASH(1) must not change to the newly canonical hash"
    );
    let user_hash = B256::repeat_byte(0x55);
    let result = api
        .simulate_v1(
            serde_json::json!({ "blockStateCalls": [{
                "blockOverrides": { "blockHash": { "1": user_hash } },
                "stateOverrides": probe_override("0x60014060005260206000f3"),
                "calls": [{ "to": LOGGER }]
            }] }),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("user block hash override");
    let output: Bytes =
        serde_json::from_value(result[0]["calls"][0]["returnData"].clone()).unwrap();
    assert_eq!(output.as_ref(), user_hash.as_slice());
}

/// The Arsia guard must inspect the preview parent and actual simulated timestamps,
/// including empty blocks inserted for number gaps.
#[tokio::test(flavor = "multi_thread")]
async fn pending_simulation_checks_the_actual_arsia_boundary() {
    use crate::consumer_helpers::{flashblocks_test_chain_spec, l2_ts};
    use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
    let activation = l2_ts(6);
    let mut spec = flashblocks_test_chain_spec().as_ref().clone();
    spec.inner.hardforks.insert(
        mantle_reth_chainspec::MantleHardfork::Arsia,
        reth_chainspec::ForkCondition::Timestamp(activation),
    );
    let (harness, node) = launch_flashblocks_consumer!(3, 3, std::sync::Arc::new(spec));
    harness.send(FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1)]).build()).await;
    assert!(harness.pending().is_some(), "pre-Arsia preview");
    let api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        None,
        None,
    )
    .with_pending_overrides(Some(harness.state().clone()));
    let result = api
        .simulate_v1(
            serde_json::json!({ "blockStateCalls": [{ "calls": [] }] }),
            Some(BlockNumberOrTag::Pending.into()),
        )
        .await
        .expect("default time is preview + 2, before activation");
    assert_eq!(result[0]["timestamp"], format!("{:#x}", l2_ts(3)));
    for block in [
        serde_json::json!({ "blockOverrides": { "time": format!("{activation:#x}") }, "calls": [] }),
        // Preview 1, fillers 2/3 at +2/+4, requested block 4 at +6 crosses activation.
        serde_json::json!({ "blockOverrides": { "number": "0x4" }, "calls": [] }),
    ] {
        let error = api
            .simulate_v1(
                serde_json::json!({ "blockStateCalls": [block] }),
                Some(BlockNumberOrTag::Pending.into()),
            )
            .await
            .expect_err("crossing Arsia is rejected");
        assert!(error.message().contains("Arsia activation boundary"), "{error}");
    }
}

/// Switching to a preview EVM must retain Mantle's additional operator-fee funds check.
#[tokio::test(flavor = "multi_thread")]
async fn pending_estimation_retains_the_arsia_funds_check() {
    use alloy_network::eip2718::{Decodable2718, Encodable2718};
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let raw = l1_info_deposit(1);
    let op_alloy_consensus::OpTxEnvelope::Deposit(deposit) =
        op_alloy_consensus::OpTxEnvelope::decode_2718(&mut raw.as_ref()).unwrap()
    else {
        panic!("deposit")
    };
    let mut deposit = deposit.inner().clone();
    let mut data = deposit.input.to_vec();
    data[168..176].copy_from_slice(&1_000_000_000_000u64.to_be_bytes());
    deposit.input = data.into();
    harness
        .send(
            FlashblockBuilder::new(1, 0).transactions(vec![deposit.encoded_2718().into()]).build(),
        )
        .await;
    let api = eth_api!(node, harness);
    let request =
        serde_json::json!({ "from": test_sender(), "to": Address::ZERO, "gasPrice": "0x1" });
    let balance = |value: &str| {
        serde_json::from_value(serde_json::json!({
            format!("{:#x}", test_sender()): { "balance": value }
        }))
        .unwrap()
    };
    let error = api
        .estimate_gas(
            request.clone(),
            Some(BlockNumberOrTag::Pending.into()),
            Some(balance("0xf4240")),
            None,
        )
        .await
        .expect_err("L2 gas is affordable, the operator fee is not");
    assert!(error.message().contains("insufficient funds"), "{error}");
    api.estimate_gas(
        request,
        Some(BlockNumberOrTag::Pending.into()),
        Some(balance("0xde0b6b3a7640000")),
        None,
    )
    .await
    .expect("the user balance override funds all fees");
}

/// `eth_getBlockByNumber("pending")` serves the overlay, and passes `pending`
/// through to the standard implementation when there is none.
#[tokio::test(flavor = "multi_thread")]
async fn get_block_by_number_serves_the_overlay_and_passes_pending_through() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let fallback = api
        .block_by_number(pending_tag(), false)
        .await
        .expect("query succeeds")
        .expect("the standard implementation resolves the tag");
    assert_eq!(
        fallback["number"], "0x0",
        "without an overlay, op-reth resolves `pending` to latest, as op-geth does"
    );

    let tx_hash = overlay_with_user_call!(harness);

    let block = api
        .block_by_number(pending_tag(), false)
        .await
        .expect("query succeeds")
        .expect("the overlay is served");
    assert_eq!(block["number"], "0x1");
    let hashes = block["transactions"].as_array().expect("transaction hashes");
    assert_eq!(hashes.len(), 2, "the L1-attributes deposit and the user call");
    assert_eq!(hashes[1].as_str().expect("hash"), format!("{tx_hash:#x}"));
}

/// `eth_getBlockTransactionCountByNumber("pending")` counts the overlay's tip block.
#[tokio::test(flavor = "multi_thread")]
async fn get_block_transaction_count_counts_the_overlay_tip() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let count = api
        .get_block_transaction_count_by_number(BlockNumberOrTag::Pending)
        .await
        .expect("query succeeds")
        .expect("the overlay is served");
    assert_eq!(count, U256::from(2));
}

/// A transaction that exists only in the overlay is served by hash, with a receipt.
#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_in_the_overlay_is_served_by_hash_and_has_a_receipt() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let tx_hash = overlay_with_user_call!(harness);

    let transaction = api
        .transaction_by_hash(tx_hash)
        .await
        .expect("query succeeds")
        .expect("the overlay holds the transaction");
    assert_eq!(transaction["hash"].as_str().expect("hash"), format!("{tx_hash:#x}"));
    assert_eq!(transaction["from"].as_str().expect("sender"), format!("{:#x}", test_sender()));

    let receipt = api
        .get_transaction_receipt(tx_hash)
        .await
        .expect("query succeeds")
        .expect("the overlay holds the receipt");
    assert_eq!(receipt["status"], "0x1", "the call must have succeeded");
}

/// `eth_getBalance` sees the overlay's transfer under `pending` only.
#[tokio::test(flavor = "multi_thread")]
async fn get_balance_sees_the_overlay_only_under_pending() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let pending =
        api.get_balance(LOGGER, Some(BlockNumberOrTag::Pending.into())).await.expect("query");
    assert_eq!(pending, VALUE);

    let latest =
        api.get_balance(LOGGER, Some(BlockNumberOrTag::Latest.into())).await.expect("query");
    assert_eq!(latest, U256::ZERO, "canonical state is untouched");
}

/// `eth_getTransactionCount` adds the overlay's nonce bumps to the canonical count.
#[tokio::test(flavor = "multi_thread")]
async fn get_transaction_count_adds_the_overlay_to_canonical() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let pending = api
        .get_transaction_count(test_sender(), Some(BlockNumberOrTag::Pending.into()))
        .await
        .expect("query");
    assert_eq!(pending, U256::from(1));

    let latest = api
        .get_transaction_count(test_sender(), Some(BlockNumberOrTag::Latest.into()))
        .await
        .expect("query");
    assert_eq!(latest, U256::ZERO, "canonical state is untouched");
}

/// `eth_getLogs` appends the overlay's logs when the range ends at `pending`.
#[tokio::test(flavor = "multi_thread")]
async fn get_logs_appends_the_overlay_when_the_range_ends_at_pending() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let pending_filter = Filter::new()
        .address(LOGGER)
        .from_block(BlockNumberOrTag::Earliest)
        .to_block(BlockNumberOrTag::Pending);
    let logs = api.get_logs(pending_filter).await.expect("query");
    assert_eq!(logs.len(), 1, "the overlay's call emits one log");
    assert_eq!(logs[0].address(), LOGGER);

    let canonical_filter = Filter::new()
        .address(LOGGER)
        .from_block(BlockNumberOrTag::Earliest)
        .to_block(BlockNumberOrTag::Latest);
    assert!(
        api.get_logs(canonical_filter).await.expect("query").is_empty(),
        "canonical state has no logs"
    );
}

/// A log range starting beyond the preview must not return older logs.
#[tokio::test(flavor = "multi_thread")]
async fn get_logs_excludes_the_overlay_before_a_future_start_block() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);
    overlay_with_user_call!(harness);

    for start in [2, 101] {
        let filter =
            Filter::new().address(LOGGER).from_block(start).to_block(BlockNumberOrTag::Pending);
        assert!(api.get_logs(filter).await.expect("query").is_empty());
    }
    let filter = Filter::new().address(LOGGER).from_block(1).to_block(BlockNumberOrTag::Pending);
    let logs = api.get_logs(filter).await.expect("inclusive preview boundary");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].block_number, Some(1));
}

/// A multi-block overlay must respect numeric and `pending` start boundaries.
#[tokio::test(flavor = "multi_thread")]
async fn get_logs_filters_older_blocks_from_a_multi_block_overlay() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);
    overlay_with_user_call!(harness);
    harness
        .send(
            FlashblockBuilder::new(2, 0)
                .prev(1, 0)
                .transactions(vec![l1_info_deposit(2), signed_call(1, LOGGER, VALUE).await])
                .build(),
        )
        .await;
    assert_eq!(harness.pending().expect("overlay").latest_block_number(), 2);

    for start in [BlockNumberOrTag::Number(2), BlockNumberOrTag::Pending] {
        let filter =
            Filter::new().address(LOGGER).from_block(start).to_block(BlockNumberOrTag::Pending);
        let logs = api.get_logs(filter).await.expect("query");
        assert_eq!(logs.len(), 1, "only block 2 belongs to this range");
        assert_eq!(logs[0].block_number, Some(2));
    }
    let filter = Filter::new()
        .address(LOGGER)
        .from_block(BlockNumberOrTag::Earliest)
        .to_block(BlockNumberOrTag::Pending);
    let logs = api.get_logs(filter).await.expect("full range");
    assert_eq!(logs.iter().map(|log| log.block_number).collect::<Vec<_>>(), vec![Some(1), Some(2)]);
}

/// Resolved start tags apply to retained preview history as well as canonical logs.
#[tokio::test(flavor = "multi_thread")]
async fn get_logs_resolves_latest_and_deduplicates_canonical_preview_overlap() {
    use crate::consumer_helpers::mine_canonical_block;

    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);
    for number in 1..=3 {
        let raw = signed_call(number - 1, LOGGER, VALUE).await;
        harness
            .send(
                FlashblockBuilder::new(number, 0)
                    .prev(number - 1, 0)
                    .transactions(vec![l1_info_deposit(number), raw.clone()])
                    .build(),
            )
            .await;
        if number < 3 {
            node.rpc.inject_tx(raw).await.expect("canonical call");
            let block = mine_canonical_block!(node);
            assert_eq!(block.number, number);
        }
    }
    let pending = harness.pending().expect("three-block overlay");
    assert_eq!((pending.earliest_block_number(), pending.latest_block_number()), (1, 3));

    for start in [None, Some(BlockNumberOrTag::Latest), Some(BlockNumberOrTag::Number(2))] {
        let mut filter = Filter::new().address(LOGGER).to_block(BlockNumberOrTag::Pending);
        if let Some(start) = start {
            filter = filter.from_block(start);
        }
        let logs = api.get_logs(filter).await.expect("mixed range");
        assert_eq!(
            logs.iter().map(|log| log.block_number).collect::<Vec<_>>(),
            vec![Some(2), Some(3)],
            "block 1 is below the start and canonical block 2 must appear only once"
        );
    }
    let filter = Filter::new()
        .address(LOGGER)
        .from_block(BlockNumberOrTag::Earliest)
        .to_block(BlockNumberOrTag::Pending);
    let logs = api.get_logs(filter).await.expect("complete mixed range");
    assert_eq!(
        logs.iter().map(|log| log.block_number).collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(3)]
    );
}

/// A code-less account touched by the overlay must not gain code in the overrides.
///
/// REVM pads bytecode for jump-destination analysis, so the execution-time form of
/// empty code is a trailing `STOP`. Serialising that into the pending override makes
/// `EXTCODESIZE` report 1 for a plain wallet, which flips every `isContract()` guard.
#[tokio::test(flavor = "multi_thread")]
async fn a_touched_account_without_code_stays_without_code_in_the_overrides() {
    let (harness, _node) = launch_flashblocks_consumer!(3, 3);

    overlay_with_user_call!(harness);

    let overrides = harness
        .pending()
        .expect("overlay")
        .get_state_overrides()
        .expect("the overlay publishes state overrides");
    let sender = overrides.get(&test_sender()).expect("the sender was touched by the call");

    assert!(
        sender.code.as_ref().is_none_or(|code| code.is_empty()),
        "a wallet must not gain code, got {:?}",
        sender.code,
    );
}

/// `eth_getLogs` at `pending` falls through to the standard implementation with no overlay.
///
/// Without an overlay the override must not answer from an empty snapshot: op-reth
/// resolves `pending` to latest, so the canonical block's logs are the right answer.
#[tokio::test(flavor = "multi_thread")]
async fn get_logs_at_pending_without_an_overlay_passes_through() {
    use crate::consumer_helpers::mine_canonical_block;

    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let raw = signed_call(0, LOGGER, VALUE).await;
    node.rpc.inject_tx(raw).await.expect("the pool takes the call");
    mine_canonical_block!(node);

    assert!(harness.pending().is_none(), "no slice was sent, so there is no overlay");

    let filter = Filter::new()
        .address(LOGGER)
        .from_block(BlockNumberOrTag::Pending)
        .to_block(BlockNumberOrTag::Pending);
    let logs = api.get_logs(filter).await.expect("query");

    assert_eq!(logs.len(), 1, "the standard implementation resolves `pending` to latest");
    assert_eq!(logs[0].address(), LOGGER);
}

/// `eth_call` at `pending` executes against the overlay's state overrides.
#[tokio::test(flavor = "multi_thread")]
async fn call_at_pending_executes_against_the_overlay_state() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    // `LOGGER` returns its own balance, which only the overlay has credited.
    let request = serde_json::json!({ "to": format!("{LOGGER:#x}") });

    let pending = api
        .call(request.clone(), Some(BlockNumberOrTag::Pending.into()), None, None)
        .await
        .expect("call succeeds");
    assert_eq!(U256::from_be_slice(&pending), VALUE);

    let latest = api
        .call(request, Some(BlockNumberOrTag::Latest.into()), None, None)
        .await
        .expect("call succeeds");
    assert_eq!(U256::from_be_slice(&latest), U256::ZERO);
}

/// `eth_estimateGas` at `pending` resolves against the overlay, so a sender
/// funded only there can be estimated for.
#[tokio::test(flavor = "multi_thread")]
async fn estimate_gas_at_pending_sees_the_overlay_balance() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let request = serde_json::json!({
        "from": format!("{LOGGER:#x}"),
        "to": format!("{:#x}", Address::ZERO),
        "value": format!("{VALUE:#x}"),
    });

    let gas = api
        .estimate_gas(request.clone(), Some(BlockNumberOrTag::Pending.into()), None, None)
        .await
        .expect("the overlay funds the sender");
    assert!(gas >= U256::from(21_000), "a plain transfer costs at least the intrinsic gas");

    assert!(
        api.estimate_gas(request, Some(BlockNumberOrTag::Latest.into()), None, None).await.is_err(),
        "canonically the sender cannot cover the transfer"
    );
}

/// The override version of `eth_estimateGas` has to honour `blockOverrides`.
///
/// It once declared three parameters where upstream takes four, so a caller's
/// fourth argument never reached the estimate — no error, just an answer
/// computed against the unmodified block. Pinned with a block gas limit below
/// the intrinsic cost of a transfer, which can only change the outcome if the
/// override arrived.
#[tokio::test(flavor = "multi_thread")]
async fn estimate_gas_honours_a_block_override() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let request = serde_json::json!({
        "from": format!("{LOGGER:#x}"),
        "to": format!("{:#x}", Address::ZERO),
        "value": format!("{VALUE:#x}"),
    });

    // Sanity: without the override the estimate succeeds, so a failure below
    // is the override and not the request.
    api.estimate_gas(request.clone(), Some(BlockNumberOrTag::Pending.into()), None, None)
        .await
        .expect("premise: the overlay funds this transfer");

    let starved = BlockOverrides { gas_limit: Some(20_000), ..Default::default() };
    assert!(
        api.estimate_gas(
            request,
            Some(BlockNumberOrTag::Pending.into()),
            None,
            Some(Box::new(starved)),
        )
        .await
        .is_err(),
        "a block gas limit under the 21000 intrinsic cost must make the estimate fail; \
         succeeding means the override was dropped on the way in",
    );
}

/// `eth_sendRawTransactionSync` rejects a timeout above the configured ceiling.
#[tokio::test(flavor = "multi_thread")]
async fn send_raw_transaction_sync_rejects_an_overlong_timeout() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let raw = signed_call(0, LOGGER, VALUE).await;
    let error =
        api.send_raw_transaction_sync(raw, Some(6_001)).await.expect_err("the ceiling is 6000 ms");
    assert!(error.message().contains("time out too long"), "unexpected error: {error}");
}

/// `eth_sendRawTransactionSync` returns once the transaction shows up in a slice.
#[tokio::test(flavor = "multi_thread")]
async fn send_raw_transaction_sync_returns_the_flashblock_receipt() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let raw = signed_call(0, LOGGER, VALUE).await;
    let tx_hash = keccak256(&raw);
    let slice =
        FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw.clone()]).build();

    let announce = async {
        // The call subscribes to the flashblock broadcast before waiting; the
        // broadcast has no replay, so the slice must not land before it does.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        harness.send(slice).await;
    };
    let (receipt, ()) = tokio::join!(api.send_raw_transaction_sync(raw, None), announce);

    let receipt = receipt.expect("the slice carries the transaction");
    assert_eq!(receipt["transactionHash"].as_str().expect("hash"), format!("{tx_hash:#x}"));
    assert_eq!(receipt["status"], "0x1");
}

/// Opens an overlay at block 1 whose user transaction calls [`SETTER`], so the
/// overlay — and only the overlay — holds [`SETTER_SLOT`].
macro_rules! overlay_with_storage_write {
    ($harness:expr) => {{
        let raw = signed_call(0, SETTER, U256::ZERO).await;
        $harness
            .send(FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw]).build())
            .await;
        assert!($harness.pending().is_some(), "the slice should open an overlay");
    }};
}

/// `eth_getStorageAt` sees the overlay's write under `pending` only.
#[tokio::test(flavor = "multi_thread")]
async fn get_storage_at_sees_the_overlay_only_under_pending() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_storage_write!(harness);

    let pending = api
        .storage_at(SETTER, SETTER_SLOT.into(), Some(BlockNumberOrTag::Pending.into()))
        .await
        .expect("query");
    assert_eq!(pending, B256::with_last_byte(SETTER_VALUE));

    let latest = api
        .storage_at(SETTER, SETTER_SLOT.into(), Some(BlockNumberOrTag::Latest.into()))
        .await
        .expect("query");
    assert_eq!(latest, B256::ZERO, "canonical state is untouched");
}

/// A slot the overlay never wrote falls back to canonical rather than reading
/// `0` off an absent entry — the account is in the overlay, the slot is not.
///
/// Guards the wrong-answer case, not the wiring: canonical holds the same
/// value, so this stays green with the override removed entirely. The wiring
/// proof is `get_storage_at_sees_the_overlay_only_under_pending`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unwritten_slot_of_a_touched_account_falls_back_to_canonical() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_storage_write!(harness);

    let pending = api
        .storage_at(SETTER, SETTER_UNWRITTEN_SLOT.into(), Some(BlockNumberOrTag::Pending.into()))
        .await
        .expect("query");

    assert_eq!(
        pending,
        B256::with_last_byte(SETTER_UNWRITTEN_VALUE),
        "the overlay never wrote this slot, so canonical's value stands",
    );
}

/// `eth_getCode` at `pending` serves a contract that exists only in the overlay.
#[tokio::test(flavor = "multi_thread")]
async fn get_code_serves_a_contract_the_overlay_deployed() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let raw = signed_deploy(0, DEPLOY_INIT_CODE).await;
    harness
        .send(FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw]).build())
        .await;
    assert!(harness.pending().is_some(), "the slice should open an overlay");

    let deployed = test_sender().create(0);

    let pending =
        api.get_code(deployed, Some(BlockNumberOrTag::Pending.into())).await.expect("query");
    assert_eq!(pending, DEPLOYED_RUNTIME);

    let latest =
        api.get_code(deployed, Some(BlockNumberOrTag::Latest.into())).await.expect("query");
    assert_eq!(latest, Bytes::new(), "canonical state has no such contract");
}

/// A wallet the overlay touched must still report no code.
///
/// Guards the same REVM-padding trap as
/// `a_touched_account_without_code_stays_without_code_in_the_overrides`, one
/// layer further out: there the padding would be written into the override,
/// here it would be served straight to a client by `eth_getCode`. Canonical
/// agrees with the overlay here, so this cannot fail on a missing override —
/// only on a wrong one.
#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_touched_by_the_overlay_still_has_no_code_at_pending() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let pending =
        api.get_code(test_sender(), Some(BlockNumberOrTag::Pending.into())).await.expect("query");
    assert_eq!(pending, Bytes::new());
}

/// Reads a hex-quantity field off a JSON-RPC object.
fn hex_u64(value: &serde_json::Value) -> u64 {
    let text = value.as_str().expect("a hex quantity");
    u64::from_str_radix(text.trim_start_matches("0x"), 16).expect("a hex quantity")
}

/// A gap mid-block tears the overlay down whole: nothing it held is served any
/// more, and `pending` goes back to meaning `latest`.
///
/// The consumer cannot tell what the missing slices did, so a receipt or a
/// transaction from the torn overlay would be an answer with nothing behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_mid_block_leaves_no_orphan_receipt_and_pending_falls_back_to_latest() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let tx_hash = overlay_with_user_call!(harness);
    api.get_transaction_receipt(tx_hash)
        .await
        .expect("query succeeds")
        .expect("premise: the overlay serves the receipt before the gap");

    // Index 3 straight after index 0: the slices in between never arrived.
    harness.send(FlashblockBuilder::new(1, 3).prev(1, 0).build()).await;
    assert!(harness.pending().is_none(), "a skipped index clears the overlay");

    assert!(
        api.get_transaction_receipt(tx_hash).await.expect("query succeeds").is_none(),
        "a receipt from the torn overlay must not survive it",
    );
    assert!(
        api.transaction_by_hash(tx_hash).await.expect("query succeeds").is_none(),
        "nor the transaction",
    );
    let block = api
        .block_by_number(pending_tag(), false)
        .await
        .expect("query succeeds")
        .expect("the standard implementation resolves the tag");
    assert_eq!(block["number"], "0x0", "with the overlay gone, `pending` is `latest` again");
}

/// A transaction that reverts is still a transaction the block holds: its
/// receipt reports status 0 and its nonce is spent.
///
/// Only an invalid transaction fails a slice; a revert is an outcome the EVM
/// produced and the sequencer sealed.
#[tokio::test(flavor = "multi_thread")]
async fn a_reverted_transaction_in_the_overlay_has_a_status_zero_receipt() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    // Init code `PUSH0 PUSH0 REVERT`: the deployment reverts at once.
    let raw = signed_deploy(0, Bytes::from_static(&[0x5f, 0x5f, 0xfd])).await;
    let tx_hash = keccak256(&raw);
    harness
        .send(FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw]).build())
        .await;
    assert!(harness.pending().is_some(), "a revert must not fail the slice");

    let receipt = api
        .get_transaction_receipt(tx_hash)
        .await
        .expect("query succeeds")
        .expect("a reverted transaction still has a receipt");
    assert_eq!(receipt["status"], "0x0", "the revert must be reported, not papered over");
    assert!(hex_u64(&receipt["gasUsed"]) > 0, "a revert still burns gas");

    let block = api
        .block_by_number(pending_tag(), false)
        .await
        .expect("query succeeds")
        .expect("the overlay is served");
    let hashes = block["transactions"].as_array().expect("transaction hashes");
    assert_eq!(hashes.len(), 2, "the deposit and the reverted deployment are both in the block");
    assert_eq!(hashes[1].as_str().expect("hash"), format!("{tx_hash:#x}"));

    let nonce = api
        .get_transaction_count(test_sender(), Some(BlockNumberOrTag::Pending.into()))
        .await
        .expect("query");
    assert_eq!(nonce, U256::from(1), "a reverted transaction still spends its nonce");
}

/// Receipt positions are block-global, so a transaction in a later slice
/// continues from where the earlier slices left off: its index, its
/// cumulative gas and its logs' indices all carry the earlier slices' totals.
///
/// Each slice executes on its own, so the offsets have to be handed across
/// explicitly; without that, every slice would report positions from zero.
#[tokio::test(flavor = "multi_thread")]
async fn receipt_positions_continue_across_slices() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let first_hash = overlay_with_user_call!(harness);
    let second = signed_call(1, LOGGER, VALUE).await;
    let second_hash = keccak256(&second);
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).transactions(vec![second]).build()).await;
    assert!(harness.pending().is_some(), "the second slice extends the overlay");

    let receipt = |hash| {
        let api = &api;
        async move {
            api.get_transaction_receipt(hash)
                .await
                .expect("query succeeds")
                .expect("the overlay holds the receipt")
        }
    };
    let first = receipt(first_hash).await;
    let second = receipt(second_hash).await;

    assert_eq!(first["blockNumber"], "0x1");
    assert_eq!(second["blockNumber"], "0x1");
    // The L1-attributes deposit is index 0, so the first user call is 1.
    assert_eq!(hex_u64(&first["transactionIndex"]), 1);
    assert_eq!(
        hex_u64(&second["transactionIndex"]),
        2,
        "the second slice's transaction follows the first slice's, not index 0 again",
    );
    assert_eq!(
        hex_u64(&second["cumulativeGasUsed"]),
        hex_u64(&first["cumulativeGasUsed"]) + hex_u64(&second["gasUsed"]),
        "cumulative gas carries the earlier slice's total",
    );
    let log_index = |receipt: &serde_json::Value| {
        let logs = receipt["logs"].as_array().expect("logs");
        assert_eq!(logs.len(), 1, "each call to LOGGER emits one log");
        hex_u64(&logs[0]["logIndex"])
    };
    assert_eq!(log_index(&first), 0);
    assert_eq!(log_index(&second), 1, "log indices are block-global too");
}

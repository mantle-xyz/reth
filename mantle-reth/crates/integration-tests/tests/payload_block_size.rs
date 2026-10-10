//! Real Mantle builders must not publish blocks exceeding the Osaka RLP cap.

use crate::helpers::{mantle_payload_attributes, with_configured_mantle_node};
use alloy_consensus::{Transaction, TxEnvelope, TxReceipt, transaction::SignerRecoverable};
use alloy_genesis::Genesis;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::{AccessList, AccessListItem, TransactionInput, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use mantle_reth_cli::node::MantleNode;
use mantle_reth_preconf::{
    PreconfConfig, PreconfServiceBuilder, PreconfStatus, types::PreconfSource,
};
use reth_basic_payload_builder::{BuildArguments, PayloadBuilder, PayloadConfig};
use reth_chainspec::EthChainSpec;
use reth_consensus::Consensus;
use reth_e2e_test_utils::{NodeHelperType, transaction::TransactionTestContext, wallet::Wallet};
use reth_node_api::{
    BuiltPayload, PayloadAttributes, PayloadBuilderError, PayloadKind, TreeConfig,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_consensus::OpBeaconConsensus;
use reth_optimism_node::{OpBuiltPayload, payload::OpPayloadAttrs};
use reth_optimism_payload_builder::OpPayloadBuilder;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const BLOCK_GAS_LIMIT: u64 = 80_000_000;
const LARGE_TX_GAS: u64 = 1_021_000;
const HARD_CAP: usize = 8_388_608;
const PACKING_TARGET: usize = HARD_CAP - 1_000_000;
const RECIPIENT: Address = address!("000000000000000000000000000000000000cafe");
const PROBE_RECIPIENT: Address = address!("000000000000000000000000000000000000beef");

fn chain_spec(reverting: bool) -> Arc<OpChainSpec> {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("assets/genesis.json")).unwrap();
    value["gasLimit"] = serde_json::Value::from(format!("0x{BLOCK_GAS_LIMIT:x}"));
    if reverting {
        value["alloc"][format!("{RECIPIENT:#x}")] =
            serde_json::json!({"balance": "0x0", "code": "0x60006000fd"});
    }
    let genesis: Genesis = serde_json::from_value(value).unwrap();
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

fn attributes(timestamp: u64) -> OpPayloadAttrs {
    let mut attrs = mantle_payload_attributes(timestamp);
    attrs.0.gas_limit = Some(BLOCK_GAS_LIMIT);
    attrs
}

async fn signed_tx(signer: PrivateKeySigner, chain_id: u64, nonce: u64, large: bool) -> TxEnvelope {
    signed_tx_to(signer, chain_id, nonce, large, RECIPIENT).await
}

async fn signed_tx_to(
    signer: PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    large: bool,
    recipient: Address,
) -> TxEnvelope {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(recipient)),
        gas: Some(if large { LARGE_TX_GAS } else { 21_000 }),
        max_fee_per_gas: Some(20_000_000_000),
        max_priority_fee_per_gas: Some(20_000_000_000),
        value: Some(U256::ZERO),
        input: TransactionInput::new(if large { vec![0; 100_000].into() } else { Bytes::new() }),
        access_list: large.then(|| {
            AccessList(vec![AccessListItem {
                address: Address::ZERO,
                storage_keys: vec![B256::ZERO; 300],
            }])
        }),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(signer, request).await
}

/// Four transactions per funded sender stay below the pool's account slot and byte limits.
/// EIP-7623 charges 1,021,000 floor gas per tx: all 78 fit 80M gas, but their real RLP does
/// not fit 8 MiB. Repeated calldata compresses well, so the DA budget is not the binding cap.
async fn large_transactions(chain_id: u64, count: usize) -> Vec<TxEnvelope> {
    let signers = Wallet::new(20).with_chain_id(chain_id).wallet_gen();
    let mut txs = Vec::with_capacity(count);
    for index in 0..count {
        txs.push(signed_tx(signers[index / 4].clone(), chain_id, (index % 4) as u64, true).await);
    }
    txs
}

fn mandatory_attributes(
    node: &mut NodeHelperType<MantleNode>,
    txs: &[TxEnvelope],
) -> OpPayloadAttrs {
    let mut attrs = node.payload.next_attributes();
    attrs.0.transactions = Some(txs.iter().map(|tx| tx.encoded_2718().into()).collect());
    attrs.0.no_tx_pool = Some(true);
    attrs
}

async fn build(
    node: &mut NodeHelperType<MantleNode>,
    attrs: OpPayloadAttrs,
) -> Result<OpBuiltPayload, PayloadBuilderError> {
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(node.current_forkchoice_state().unwrap(), Some(attrs))
        .await
        .expect("FCU starts payload job")
        .payload_id
        .expect("payload id");
    tokio::time::timeout(
        Duration::from_secs(30),
        node.inner.payload_builder_handle.resolve_kind(payload_id, PayloadKind::Earliest),
    )
    .await
    .expect("builder finishes")
    .expect("payload job exists")
}

/// Invoke the ordinary OP builder explicitly against the live node's real pool, provider and EVM.
/// A node with preconf disabled also delegates its FCU builds to this ordinary builder. This
/// helper only builds on genesis; later-block tests use FCU.
fn build_normal(
    node: &NodeHelperType<MantleNode>,
    attrs: OpPayloadAttrs,
) -> Result<OpBuiltPayload, PayloadBuilderError> {
    let parent_header = Arc::new(node.inner.chain_spec().sealed_genesis_header());
    let payload_id = attrs.payload_id(&parent_header.hash());
    OpPayloadBuilder::new(
        node.inner.pool.clone(),
        node.inner.provider.clone(),
        node.inner.evm_config.clone(),
    )
    .try_build(BuildArguments::new(
        Default::default(),
        None,
        None,
        PayloadConfig { parent_header, attributes: attrs, payload_id },
        Default::default(),
        None,
    ))
    .map(|outcome| outcome.into_payload().expect("normal builder produced a payload"))
}

fn assert_valid_size(node: &NodeHelperType<MantleNode>, payload: &OpBuiltPayload) {
    let size = payload.block().rlp_length();
    assert!(size <= HARD_CAP, "builder published {size} bytes, exceeding {HARD_CAP}");
    OpBeaconConsensus::new(node.inner.chain_spec())
        .validate_block_pre_execution(payload.block())
        .expect("built payload must pass full pre-execution consensus validation");
}

fn assert_reverting_receipts(payload: &OpBuiltPayload) {
    let executed = payload.executed_block().expect("executed payload");
    assert_eq!(
        executed.execution_output.result.receipts.len(),
        payload.block().body().transactions.len()
    );
    assert!(
        executed.execution_output.result.receipts.iter().all(|receipt| !receipt.status()),
        "fixture transactions must really revert while remaining included"
    );
}

async fn preconf_service() -> (PreconfServiceBuilder, tempfile::TempDir) {
    preconf_service_with_filter(None).await
}

async fn preconf_service_with_filter(
    probe_sender: Option<Address>,
) -> (PreconfServiceBuilder, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = PreconfConfig {
        enabled: true,
        all_preconfs: probe_sender.is_none(),
        from_preconfs: probe_sender.into_iter().collect(),
        to_preconfs: probe_sender.map(|_| PROBE_RECIPIENT).into_iter().collect(),
        preconf_timeout: Duration::from_secs(30),
        preconf_max_gas_per_tx: 2_000_000,
        preconf_max_gas_per_block: BLOCK_GAS_LIMIT,
        journal_path: Some(dir.path().join("preconf.journal")),
        ..Default::default()
    };
    (PreconfServiceBuilder::from_config(cfg).await.unwrap(), dir)
}

/// An invalid-nonce RPC probe observes the enabled builder's current raw-size headroom. Its
/// envelope is as large as a fixture tx, but its nonce is always invalid, so it can never execute
/// successfully or contribute bytes. Wait for actual RLP-capacity rejection before resolving the
/// job: an immediate Earliest resolve would cancel before the pool arm has scanned its candidates.
async fn build_preconf_pool(
    node: &mut NodeHelperType<MantleNode>,
    attrs: OpPayloadAttrs,
    fifo: &mantle_reth_preconf::PreconfTxSet,
    probe: TxEnvelope,
) -> OpBuiltPayload {
    let hash = *probe.tx_hash();
    let sender = probe.recover_signer().unwrap();
    let probe = Arc::new(probe);
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(node.current_forkchoice_state().unwrap(), Some(attrs))
        .await
        .unwrap()
        .payload_id
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (send, recv) = tokio::sync::oneshot::channel();
            fifo.attach_responder(hash, Instant::now(), send).await.unwrap();
            fifo.push_if_absent(probe.clone(), sender, PreconfSource::Rpc).await;
            let error =
                recv.await.unwrap().expect_err("invalid nonce probe must never be included");
            let reason = error.to_string();
            if reason.contains("RLP size headroom exhausted") {
                break;
            }
            assert!(
                reason.contains("nonce"),
                "probe rejection must be its invalid nonce: {reason}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observe actual pool packing capacity before resolving");
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, PayloadKind::Earliest)
        .await
        .expect("live payload job")
        .expect("completed preconf pool payload");
    assert!(!payload.block().body().transactions.iter().any(|tx| tx.tx_hash() == hash));
    assert!(
        fifo.entries()
            .await
            .iter()
            .all(|entry| entry.hash == hash && entry.source == PreconfSource::Rpc),
        "fixture transactions must never enter the FIFO or become Replay"
    );
    payload
}

async fn pool_case(preconf: bool, reverting: bool) {
    let spec = chain_spec(reverting);
    let chain_id = spec.chain().id();
    let txs = large_transactions(chain_id, 78).await;
    let candidate_bytes: usize = txs.iter().map(Encodable::length).sum();
    assert!(candidate_bytes > HARD_CAP, "fixture must exceed the protocol cap");
    assert_eq!(txs.iter().map(Transaction::gas_limit).sum::<u64>(), 79_638_000);
    let signer = Wallet::new(20).with_chain_id(chain_id).wallet_gen()[19].clone();
    let probe = signed_tx_to(signer.clone(), chain_id, 10_000, true, PROBE_RECIPIENT).await;
    let (service, _dir) = preconf_service_with_filter(Some(signer.address())).await;
    let fifo = service.fifo().clone();
    let node_type =
        if preconf { MantleNode::default().with_preconf(service) } else { MantleNode::default() };
    with_configured_mantle_node(
        node_type,
        spec,
        attributes,
        TreeConfig::default(),
        move |mut node, _| async move {
            for tx in &txs {
                node.rpc
                    .inject_tx(tx.encoded_2718().into())
                    .await
                    .expect("funded pool tx accepted");
            }
            assert!(fifo.entries().await.is_empty(), "pool fixtures must not enter the FIFO");
            let attrs = node.payload.next_attributes();
            let payload = if preconf {
                build_preconf_pool(&mut node, attrs, &fifo, probe).await
            } else {
                build_normal(&node, attrs).expect("normal payload")
            };
            let included = payload.block().body().transactions.len();
            assert!(
                included > 0 && included < txs.len(),
                "must truncate oversized candidate; included={included}"
            );
            assert_eq!(included, 67, "pool scan must reach the actual packing capacity");
            assert!(
                payload
                    .block()
                    .body()
                    .transactions
                    .iter()
                    .all(|tx| txs.iter().any(|pool_tx| *pool_tx.tx_hash() == tx.tx_hash())),
                "all included transactions must originate in the original pool fixture"
            );
            assert_valid_size(&node, &payload);
            if reverting {
                assert_reverting_receipts(&payload);
            }
            assert!(payload.block().rlp_length() < PACKING_TARGET);
            eprintln!(
                "{} pool block: {included} txs, {} RLP bytes",
                if preconf { "preconf" } else { "normal" },
                payload.block().rlp_length()
            );
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn normal_pool_stops_before_the_rlp_cap() {
    pool_case(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn normal_pool_counts_reverting_included_transactions() {
    pool_case(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_pool_stops_before_the_rlp_cap() {
    pool_case(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_pool_counts_reverting_included_transactions() {
    pool_case(true, true).await;
}

async fn preconf_replay_case(reverting: bool) {
    let spec = chain_spec(reverting);
    let txs = large_transactions(spec.chain().id(), 78).await;
    let (service, _dir) = preconf_service().await;
    let fifo = service.fifo().clone();
    with_configured_mantle_node(
        MantleNode::default().with_preconf(service),
        spec,
        attributes,
        TreeConfig::default(),
        move |mut node, _| async move {
            for tx in &txs {
                fifo.push_if_absent(
                    Arc::new(tx.clone()),
                    tx.recover_signer().unwrap(),
                    PreconfSource::Replay,
                )
                .await;
            }
            let attrs = node.payload.next_attributes();
            let first = build(&mut node, attrs).await.expect("preconf payload");
            let first_count = first.block().body().transactions.len();
            assert!(
                first_count > 0 && first_count < txs.len(),
                "Replay must respect raw size budget; included={first_count}"
            );
            assert_valid_size(&node, &first);
            if reverting {
                assert_reverting_receipts(&first);
            }
            let deferred = fifo
                .entries()
                .await
                .into_iter()
                .filter(|entry| entry.status == PreconfStatus::Waiting)
                .collect::<Vec<_>>();
            assert_eq!(deferred.len(), txs.len() - first_count);
            let hash = node.submit_payload(first.clone()).await.unwrap();
            node.update_forkchoice(hash, hash).await.unwrap();
            let attrs = node.payload.next_attributes();
            let second = build(&mut node, attrs).await.expect("deferred Replay payload");
            assert_valid_size(&node, &second);
            if reverting {
                assert_reverting_receipts(&second);
            }
            for entry in &deferred {
                assert!(
                    second.block().body().transactions.iter().any(|tx| tx.tx_hash() == entry.hash),
                    "deferred commitment must land next block"
                );
            }
            assert_eq!(first_count + second.block().body().transactions.len(), txs.len());
            eprintln!(
                "preconf blocks: {first_count} txs / {} RLP bytes + {} txs / {} RLP bytes",
                first.block().rlp_length(),
                second.block().body().transactions.len(),
                second.block().rlp_length()
            );
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_replay_defers_until_the_next_block() {
    preconf_replay_case(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_replay_counts_reverting_included_transactions() {
    preconf_replay_case(true).await;
}

async fn mandatory_case(preconf: bool, count: usize) {
    let spec = chain_spec(false);
    let txs = large_transactions(spec.chain().id(), count).await;
    let (service, _dir) = preconf_service().await;
    let node_type =
        if preconf { MantleNode::default().with_preconf(service) } else { MantleNode::default() };
    with_configured_mantle_node(
        node_type,
        spec,
        attributes,
        TreeConfig::default(),
        move |mut node, _| async move {
            let attrs = mandatory_attributes(&mut node, &txs);
            let result =
                if preconf { build(&mut node, attrs).await } else { build_normal(&node, attrs) };
            if count == 78 {
                assert!(
                    result.is_err(),
                    "oversized mandatory attributes must fail before publishing an executed payload"
                );
            } else {
                let payload = result.expect("legal mandatory attributes");
                assert_eq!(payload.block().body().transactions.len(), count);
                assert!(
                    payload.block().rlp_length() > PACKING_TARGET,
                    "fixture must exceed optional packing target"
                );
                assert_valid_size(&node, &payload);
                eprintln!(
                    "{} mandatory block: {count} txs / {} RLP bytes",
                    if preconf { "preconf" } else { "normal" },
                    payload.block().rlp_length()
                );
            }
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn normal_mandatory_above_packing_target_is_legal() {
    mandatory_case(false, 68).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_mandatory_above_packing_target_is_legal() {
    mandatory_case(true, 68).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn normal_oversized_mandatory_attributes_fail() {
    mandatory_case(false, 78).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_oversized_mandatory_attributes_fail() {
    mandatory_case(true, 78).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_rpc_is_rejected_before_a_receipt_is_promised() {
    let spec = chain_spec(false);
    let chain_id = spec.chain().id();
    let attrs_txs = large_transactions(chain_id, 68).await;
    let signer = Wallet::new(20).with_chain_id(chain_id).wallet_gen()[19].clone();
    // Finish expensive signing before attaching the responder's deadline clock.
    let candidate = signed_tx(signer.clone(), chain_id, 0, false).await;
    let hash = *candidate.tx_hash();
    let (service, _dir) = preconf_service().await;
    let fifo = service.fifo().clone();
    with_configured_mantle_node(
        MantleNode::default().with_preconf(service),
        spec,
        attributes,
        TreeConfig::default(),
        move |mut node, _| async move {
            let (send, recv) = tokio::sync::oneshot::channel();
            fifo.attach_responder(hash, Instant::now(), send).await.unwrap();
            fifo.push_if_absent(Arc::new(candidate), signer.address(), PreconfSource::Rpc).await;
            let mut attrs = mandatory_attributes(&mut node, &attrs_txs);
            attrs.0.no_tx_pool = Some(false);
            let payload = build(&mut node, attrs).await.expect("legal mandatory block");
            let reply = tokio::time::timeout(Duration::from_secs(30), recv).await.unwrap().unwrap();
            assert!(reply.is_err(), "block-full RPC must be rejected before success/receipt");
            assert!(
                reply.unwrap_err().to_string().contains("RLP size headroom exhausted"),
                "rejection must come from raw size admission"
            );
            assert!(!payload.block().body().transactions.iter().any(|tx| tx.tx_hash() == hash));
            assert_eq!(fifo.find_by_hash(&hash).await.unwrap().status, PreconfStatus::Canceled);
            assert_valid_size(&node, &payload);
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_size_defer_preserves_same_sender_successors() {
    let spec = chain_spec(false);
    let chain_id = spec.chain().id();
    // 67 mandatory txs leave room for the small nonce-1 tx but not the large nonce-0 tx.
    // The successor must inherit the predecessor's defer instead of nonce-too-high failing.
    let attrs_txs = large_transactions(chain_id, 67).await;
    let signer = Wallet::new(20).with_chain_id(chain_id).wallet_gen()[19].clone();
    let large = signed_tx(signer.clone(), chain_id, 0, true).await;
    let small = signed_tx(signer.clone(), chain_id, 1, false).await;
    let hashes = [*large.tx_hash(), *small.tx_hash()];
    let (service, _dir) = preconf_service().await;
    let fifo = service.fifo().clone();
    with_configured_mantle_node(
        MantleNode::default().with_preconf(service),
        spec,
        attributes,
        TreeConfig::default(),
        move |mut node, _| async move {
            for tx in [large, small] {
                fifo.push_if_absent(Arc::new(tx), signer.address(), PreconfSource::Replay).await;
            }
            let mut attrs = mandatory_attributes(&mut node, &attrs_txs);
            attrs.0.no_tx_pool = Some(false);
            let first = build(&mut node, attrs).await.expect("legal mandatory block");
            assert_eq!(first.block().body().transactions.len(), attrs_txs.len());
            for hash in &hashes {
                assert_eq!(
                    fifo.find_by_hash(hash).await.unwrap().status,
                    PreconfStatus::Waiting,
                    "size-deferred sender chain must remain Waiting"
                );
            }
            assert_valid_size(&node, &first);
            let hash = node.submit_payload(first.clone()).await.unwrap();
            node.update_forkchoice(hash, hash).await.unwrap();
            let attrs = node.payload.next_attributes();
            let second = build(&mut node, attrs).await.expect("deferred sender chain");
            assert_eq!(second.block().body().transactions.len(), 2);
            assert_eq!(
                second
                    .block()
                    .body()
                    .transactions
                    .iter()
                    .map(|tx| tx.tx_hash())
                    .collect::<Vec<_>>(),
                hashes
            );
            assert_valid_size(&node, &second);
        },
    )
    .await;
}

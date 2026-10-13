//! Real Mantle builders must not publish blocks exceeding the Osaka RLP cap.
//!
//! Every case here drives a fixture set three orders of magnitude larger than
//! the rest of the suite — 78 transactions of ~110 KB each, against a 7.39 MB
//! packing target — because that is the only size at which the budget stops
//! being inert. A block of kilobyte transactions exercises the budget's code
//! and none of its decisions.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p mantle-reth-integration-tests --test it -- payload_block_size
//! ```

use crate::helpers::{mantle_payload_attributes, with_configured_mantle_node};
use alloy_consensus::{
    Transaction, TxEnvelope, TxReceipt, Typed2718, transaction::SignerRecoverable,
};
use alloy_genesis::Genesis;
use alloy_network::eip2718::Encodable2718;
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::{AccessList, AccessListItem, TransactionInput, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use mantle_reth_cli::node::MantleNode;
use mantle_reth_preconf::{
    PreconfConfig, PreconfError, PreconfReceipt, PreconfServiceBuilder, PreconfStatus,
    preconf_tx_set::{AdmitRequest, Capacity},
    types::PreconfSource,
};
use reth_basic_payload_builder::{BuildArguments, PayloadBuilder, PayloadConfig};
use reth_chainspec::{EthChainSpec, ForkCondition};
use reth_consensus::Consensus;
use reth_e2e_test_utils::{NodeHelperType, transaction::TransactionTestContext, wallet::Wallet};
use reth_node_api::{
    BuiltPayload, PayloadAttributes, PayloadBuilderError, PayloadKind, TreeConfig,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_consensus::OpBeaconConsensus;
use reth_optimism_node::{OpBuiltPayload, args::RollupArgs, payload::OpPayloadAttrs};
use reth_optimism_payload_builder::{
    OpPayloadBuilder,
    block_size::{BLOCK_SIZE_PACKING_TARGET, BlockSizeBudget, RawSizeAdmission},
};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

const BLOCK_GAS_LIMIT: u64 = 80_000_000;
const LARGE_TX_GAS: u64 = 1_021_000;
const HARD_CAP: usize = 8_388_608;
const PACKING_TARGET: usize = HARD_CAP - 1_000_000;
const RECIPIENT: Address = address!("000000000000000000000000000000000000cafe");
const PROBE_RECIPIENT: Address = address!("000000000000000000000000000000000000beef");

/// Tick cadence the preconf builder paces its pool arm with in these tests.
///
/// The pool arm opens with a drained quota and gains one `gas_per_batch` share
/// per tick, so nothing is admitted before the first tick fires. Shortened from
/// the 200 ms production default so the quota climbs in fine steps rather than
/// in four coarse ones.
const SWEEP_INTERVAL: Duration = Duration::from_millis(50);

/// Slot length the pool quota schedule divides the block gas limit across.
///
/// Left at the production default because two separate clocks hang off it: the
/// quota reaches the full block gas limit exactly one slot after the build
/// starts, and the job's stall watchdog abandons an unresolved build after
/// three. [`BUILD_SETTLE`] has to sit between them.
const SLOT_DURATION: Duration = Duration::from_secs(2);

/// How long a preconf build is left running before `getPayload` resolves it.
///
/// `PayloadKind::Earliest` cancels the job, so resolving straight after the FCU
/// would seal whatever the builder had reached — for the paced pool arm, an
/// empty block. Past one [`SLOT_DURATION`] (the pool quota is fully open) and
/// well short of three (the stall watchdog abandons the job and `getPayload`
/// comes back empty), with comparable slack either side rather than crowding
/// the lower bound. The margin is not load-bearing: every case below asserts an
/// exact inclusion count and fails loudly rather than silently shrinking.
const BUILD_SETTLE: Duration = Duration::from_millis(3_500);

/// Queue ceilings for a directly-admitted probe. Deliberately far above
/// anything these tests queue: the subject is the block's RLP budget, and a
/// queue-capacity refusal would mask it.
const PROBE_CAPACITY: Capacity = Capacity {
    max_txs: 10_000,
    max_size: 64 * 1024 * 1024,
    max_account_slots: 64,
    max_inflight_delegated: 16,
    max_queued_gas: u64::MAX,
};

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

/// [`chain_spec`] with Lagoon active at genesis — the protocol half of the SDM
/// production gate.
///
/// Inserted into the fork schedule rather than written into the fixture JSON:
/// Lagoon is not on Mantle's schedule at all, so `from_mantle_genesis` has no
/// `lagoonTime` to read.
///
/// The insert appends rather than slotting the fork into canonical order, so do
/// not reuse this helper for a test that reads fork IDs.
fn sdm_chain_spec() -> Arc<OpChainSpec> {
    let mut spec = (*chain_spec(false)).clone();
    spec.inner.hardforks.insert(OpHardfork::Lagoon, ForkCondition::Timestamp(0));
    Arc::new(spec)
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
    tokio::time::sleep(BUILD_SETTLE).await;
    tokio::time::timeout(
        Duration::from_secs(60),
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
        PayloadConfig { parent_header, parent_block_info: None, attributes: attrs, payload_id },
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

/// Hashes of every transaction the payload sealed.
fn sealed_hashes(payload: &OpBuiltPayload) -> HashSet<B256> {
    payload.block().body().transactions.iter().map(|tx| tx.tx_hash()).collect()
}

/// A preconf service with every transaction eligible.
///
/// Upstream filtered eligibility with `PreconfConfig::from_preconfs` /
/// `to_preconfs`. Those lists are gone: the allowlist now lives in an on-chain
/// `PreconfWhitelist` the sequencer reads at cold start, and seeding one here
/// would say nothing about block size. `all_preconfs` is the mode that never
/// reads the contract at all, and it is what the eligibility filter was
/// standing in for — the fixture transactions reach the queue only by the
/// explicit pushes below, never by being classified.
async fn preconf_service() -> (PreconfServiceBuilder, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = PreconfConfig {
        enabled: true,
        all_preconfs: true,
        preconf_timeout: Duration::from_secs(30),
        sweep_interval: SWEEP_INTERVAL,
        slot_duration: SLOT_DURATION,
        preconf_max_gas_per_tx: 2_000_000,
        preconf_max_gas_per_block: BLOCK_GAS_LIMIT,
        journal_path: Some(dir.path().join("preconf.journal")),
        ..Default::default()
    };
    (PreconfServiceBuilder::from_config(cfg).await.unwrap(), dir)
}

/// Queue `tx` as a live RPC request with its client's channel attached, the way
/// the preconf RPC handler does.
///
/// Upstream used `attach_responder` + `push_if_absent`; the queue now decides
/// and enqueues under one lock, so the channel goes in with the entry. The
/// `chain_nonce` the caller supplies is the sender's nonce as of the parent
/// block — the same value the RPC handler reads off the validator's outcome.
async fn admit_rpc(
    fifo: &mantle_reth_preconf::PreconfTxSet,
    tx: TxEnvelope,
    from: Address,
    chain_nonce: u64,
) -> tokio::sync::oneshot::Receiver<Result<PreconfReceipt, PreconfError>> {
    let (send, recv) = tokio::sync::oneshot::channel();
    fifo.admit(
        AdmitRequest {
            tx: Arc::new(tx),
            from,
            source: PreconfSource::Rpc,
            responder: Some((Instant::now(), send)),
            chain_nonce,
            bytecode_hash: None,
        },
        PROBE_CAPACITY,
    )
    .await
    .expect("the queue admits the probe");
    recv
}

async fn pool_case(preconf: bool, reverting: bool) {
    let spec = chain_spec(reverting);
    let chain_id = spec.chain().id();
    let txs = large_transactions(chain_id, 78).await;
    let candidate_bytes: usize = txs.iter().map(Encodable::length).sum();
    assert!(candidate_bytes > HARD_CAP, "fixture must exceed the protocol cap");
    assert_eq!(txs.iter().map(Transaction::gas_limit).sum::<u64>(), 79_638_000);
    let (service, _dir) = preconf_service().await;
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
                build(&mut node, attrs).await.expect("preconf pool payload")
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
            if preconf {
                // Upstream asserted the fixtures never reach the queue at all.
                // They do now, and after the fact: the pool arm stages every
                // transaction it executes as an announced commitment so slicing
                // can replay it. What still has to hold is that the queue learnt
                // nothing the block does not contain — a fixture admitted
                // *through* the preconf arm would have been one the RLP budget
                // was meant to refuse.
                let sealed = sealed_hashes(&payload);
                for entry in fifo.entries().await {
                    assert!(
                        sealed.contains(&entry.hash),
                        "queue holds {:?}, which the block does not contain",
                        entry.hash
                    );
                }
            }
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
    // Finish expensive signing before the responder's deadline clock starts.
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
            let recv = admit_rpc(&fifo, candidate, signer.address(), 0).await;
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
            // Upstream parked the refused entry as `PreconfStatus::Canceled`.
            // There is no such status now: a commitment that cannot be honoured
            // is removed and its client told why in one operation
            // (`PreconfTxSet::complete_failure`), so absence from the queue is
            // the same fact.
            assert!(
                fifo.find_by_hash(&hash).await.is_none(),
                "a refused commitment leaves no entry behind"
            );
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

// ───────────────────────── which byte measure ─────────────────────────
//
// Every assertion above survives swapping `Encodable::length` for
// `Encodable2718::encode_2718_len` at the builder's budget sites: a typed
// transaction's RLP string wrapper is a few bytes against a ~110 KB
// transaction, so the inclusion counts do not move. The two cases below are
// sized so that they do.

/// Replay `txs` into a budget shaped exactly as the builder shapes one for a
/// Mantle Osaka block with SDM production off, measuring each item with `measure`.
fn replay_budget(txs: &[TxEnvelope], measure: fn(&TxEnvelope) -> usize) -> BlockSizeBudget {
    let mut budget = BlockSizeBudget::new(true, true, 1, false);
    for tx in txs {
        budget.record_transaction(measure(tx), true);
    }
    budget
}

/// The largest item `budget` still admits, found by bisection on its own
/// verdict rather than by restating its arithmetic.
fn largest_admissible(budget: &BlockSizeBudget) -> usize {
    let (mut fits, mut full) = (0usize, BLOCK_SIZE_PACKING_TARGET);
    assert_eq!(budget.admission(fits, true), RawSizeAdmission::Fits);
    while full - fits > 1 {
        let mid = fits + (full - fits) / 2;
        if budget.admission(mid, true) == RawSizeAdmission::Fits {
            fits = mid;
        } else {
            full = mid;
        }
    }
    fits
}

/// Sign a transaction whose block-body RLP item length lands inside
/// `window`, by adjusting its calldata a byte at a time.
///
/// A signature with a leading zero byte shifts the length, so this converges by
/// measuring rather than by computing — and panics rather than returning a
/// near-miss, because a probe outside the window proves nothing.
async fn probe_sized_into(
    signer: PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    window: std::ops::RangeInclusive<usize>,
) -> TxEnvelope {
    let target = *window.start() + (*window.end() - *window.start()) / 2;
    let mut data_len = target.saturating_sub(120);
    for _ in 0..8 {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(nonce),
            to: Some(TxKind::Call(PROBE_RECIPIENT)),
            // EIP-7623 floor: 21,000 plus 10 per zero calldata byte.
            gas: Some(21_000 + 10 * data_len as u64 + 10_000),
            max_fee_per_gas: Some(20_000_000_000),
            max_priority_fee_per_gas: Some(20_000_000_000),
            value: Some(U256::ZERO),
            input: TransactionInput::new(vec![0; data_len].into()),
            ..Default::default()
        };
        let tx = TransactionTestContext::sign_tx(signer.clone(), request).await;
        let length = tx.length();
        if window.contains(&length) {
            return tx;
        }
        data_len = (data_len as isize + target as isize - length as isize) as usize;
    }
    panic!("no calldata length put the probe inside {window:?}");
}

/// The budget counts a transaction's **block-body RLP item**, not its EIP-2718
/// payload: for a typed transaction the two differ by the outer string wrapper.
///
/// The probe is sized into the gap between the two measures — above what the
/// block-body measure leaves room for, and below what the 2718 measure would
/// think it has — so the pool arm's verdict on it says which measure the
/// builder used. Both ends of that gap are asserted before the node starts, so
/// a future fixture that closes it fails loudly instead of passing vacuously.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_admission_counts_the_block_body_item_not_the_2718_payload() {
    let spec = chain_spec(false);
    let chain_id = spec.chain().id();
    let mandatory = large_transactions(chain_id, 67).await;

    let exact = replay_budget(&mandatory, |tx| tx.length());
    let undercount = replay_budget(&mandatory, |tx| tx.encode_2718_len());
    let headroom = largest_admissible(&exact);
    let undercount_headroom = largest_admissible(&undercount);
    assert!(
        undercount_headroom > headroom,
        "the 2718 measure must under-count, or there is no gap to probe"
    );

    let signer = Wallet::new(20).with_chain_id(chain_id).wallet_gen()[19].clone();
    // One byte above what the block-body measure allows, up to what the 2718
    // measure would allow. The lower half of that span keeps the probe clear of
    // the far edge, where its own wrapper decides the verdict.
    let span = undercount_headroom - headroom;
    let probe =
        probe_sized_into(signer.clone(), chain_id, 0, headroom + 1..=headroom + span / 2).await;

    // The pin: both verdicts, against the budget's own accounting.
    assert_eq!(
        exact.admission(probe.length(), true),
        RawSizeAdmission::BlockFull,
        "the block-body measure must refuse this probe"
    );
    assert_eq!(
        undercount.admission(probe.encode_2718_len(), true),
        RawSizeAdmission::Fits,
        "the 2718 measure must admit it — that disagreement is what the build decides"
    );
    let probe_hash = *probe.tx_hash();

    let (service, _dir) = preconf_service().await;
    with_configured_mantle_node(
        MantleNode::default().with_preconf(service),
        spec,
        attributes,
        TreeConfig::default(),
        move |mut node, _| async move {
            node.rpc
                .inject_tx(probe.encoded_2718().into())
                .await
                .expect("funded probe accepted by the pool");
            let mut attrs = mandatory_attributes(&mut node, &mandatory);
            attrs.0.no_tx_pool = Some(false);
            let payload = build(&mut node, attrs).await.expect("legal mandatory block");

            let sealed = sealed_hashes(&payload);
            assert_eq!(
                sealed.len(),
                mandatory.len(),
                "the block must hold the mandatory transactions and nothing else"
            );
            assert!(
                !sealed.contains(&probe_hash),
                "the pool arm admitted a probe only the 2718 measure has room for"
            );
            assert_valid_size(&node, &payload);

            // The other half of the pin: the block the builder actually sealed
            // accounts, item for item, to what the budget was tracking.
            let body_sum: usize =
                payload.block().body().transactions.iter().map(Encodable::length).sum();
            let mandatory_sum: usize = mandatory.iter().map(Encodable::length).sum();
            assert_eq!(
                body_sum, mandatory_sum,
                "sealed block-body item lengths must equal what the budget recorded"
            );

            // Why it was left out, not just that it was. Same probe, same node,
            // same pool, against a block whose budget has room for it: anything
            // else that could have refused it — its gas, its DA footprint, its
            // nonce, the pool never having held it — refuses it here too, and
            // this suite's only guard against a crossed byte measure would
            // otherwise keep passing while guarding nothing.
            let head = node.submit_payload(payload.clone()).await.unwrap();
            node.update_forkchoice(head, head).await.unwrap();
            let attrs = node.payload.next_attributes();
            let control = build(&mut node, attrs).await.expect("control block");
            assert!(
                sealed_hashes(&control).contains(&probe_hash),
                "the probe never became includable, so its absence above says nothing \
                 about the byte measure"
            );
            assert_valid_size(&node, &control);
        },
    )
    .await;
}

// ───────────────────────── SDM post-exec refund ─────────────────────────

/// Stage 4 appends an SDM post-exec refund transaction and swaps the budget's
/// per-transaction reserve for that transaction's real RLP item
/// (`BlockSizeBudget::record_post_exec`). It never runs on a node built from
/// this repository: entries are only accumulated for a transaction whose refund
/// is non-zero, and the refund policy the open-source builder installs —
/// `alloy_op_evm::post_exec::NullRefundPolicy` — refunds nothing, so the
/// entry list is always empty and no transaction is appended.
///
/// Asserted rather than left as a note, because the coverage of that line rests
/// on it. Both halves of the production gate are switched on here, so a refund
/// policy that actually refunds turns this test red — which is exactly the
/// change that makes the post-exec budget swap reachable and owing a
/// behavioural test of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdm_production_appends_no_post_exec_transaction_under_the_shipped_refund_policy() {
    let spec = sdm_chain_spec();
    let chain_id = spec.chain().id();
    assert!(
        spec.is_lagoon_active_at_timestamp(u64::MAX),
        "the protocol half of the SDM gate must really be on, or this proves nothing"
    );
    let mandatory = large_transactions(chain_id, 4).await;
    let (service, _dir) = preconf_service().await;
    // The operator half of the gate — the same flag `admin_setOperatorSdmOptIn`
    // toggles at runtime.
    let node = MantleNode::new(RollupArgs { operator_sdm_opt_in: true, ..Default::default() })
        .with_preconf(service);
    with_configured_mantle_node(
        node,
        spec,
        attributes,
        TreeConfig::default(),
        move |mut node, _| async move {
            let mut attrs = mandatory_attributes(&mut node, &mandatory);
            // Entries are only produced while the node sequences its own block:
            // `no_tx_pool` means "reproduce what the consensus layer derived",
            // and that branch verifies an embedded post-exec transaction rather
            // than building one.
            attrs.0.no_tx_pool = Some(false);
            let payload = build(&mut node, attrs).await.expect("SDM-enabled block");
            let body = &payload.block().body().transactions;
            assert_eq!(body.len(), mandatory.len(), "the block holds the mandatory txs alone");
            assert!(
                !body.iter().any(|tx| tx.ty() == op_alloy_consensus::POST_EXEC_TX_TYPE_ID),
                "a post-exec transaction here means the refund policy now refunds, and \
                 `record_post_exec` needs behavioural coverage"
            );
            assert_valid_size(&node, &payload);
        },
    )
    .await;
}

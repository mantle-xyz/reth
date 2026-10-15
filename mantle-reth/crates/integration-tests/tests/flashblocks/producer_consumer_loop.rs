//! The producer and the consumer, over the socket, in one test.
//!
//! Everything else in this directory holds one end and a stub of the other:
//! `producer_e2e` reads frames with a raw websocket client and no consumer,
//! the consumer suites push hand-built slices straight into a
//! [`FlashblocksState`] with no producer, and `subscriber_resume` drives a stub
//! server that ignores the resume parameters it is sent. So "can the consumer
//! read what this producer emits" is a question none of them asks — which is
//! how a rebuilt block came to blind the overlay for its whole duration
//! without a single test noticing.
//!
//! The consumer here is bound to the producer's own chain, because that is the
//! arrangement in production: the overlay runs ahead of the same canonical
//! state it will later be reconciled against.

use std::time::Duration;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, address, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_flashblocks::{
    FlashblocksAPI, FlashblocksReceiver, FlashblocksState, FlashblocksSubscriber,
};
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use mantle_reth_preconf::flashblocks::FlashblockProducerConfig;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

use crate::helpers::PreconfCfgBuilder;

const RECIPIENT: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");

/// Slice faster than production so one block yields several slices inside a
/// test's patience, and let the OS pick the port so concurrent tests do not
/// collide. Everything else is the shipped default.
fn producer_cfg() -> FlashblockProducerConfig {
    FlashblockProducerConfig {
        addr: std::net::Ipv4Addr::LOCALHOST.into(),
        port: 0,
        block_time: Duration::from_millis(100),
        ..Default::default()
    }
}

async fn signed_transfer(wallet: &Wallet, chain_id: u64, nonce: u64) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT)),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// A real consumer on the producer's socket, reconciling against the
/// producer's own chain.
///
/// A macro rather than a function so the provider's bounds stay where the node
/// builder already satisfies them, the same reason `launch_flashblocks_node!`
/// is one. The subscriber is dropped on return: `start` spawns the connection
/// and the delivery loop, which hold what they need.
macro_rules! attach_consumer {
    ($node:expr, $addr:expr) => {{
        let state = std::sync::Arc::new(FlashblocksState::new(3, 3, 5));
        state.start($node.inner.provider.clone());

        let tee = std::sync::Arc::new(Tee::new(std::sync::Arc::clone(&state)));
        let url = format!("ws://{}", $addr).parse().expect("publisher url");
        let mut subscriber =
            FlashblocksSubscriber::new(std::sync::Arc::clone(&tee), url, Duration::from_secs(2));
        subscriber.start();

        (state, tee)
    }};
}

/// Counts what crosses the socket on its way to the state.
///
/// Splits "the producer never published it" from "the consumer could not use
/// it" — the same symptom at the overlay, different bugs.
#[derive(Debug)]
struct Tee {
    state: std::sync::Arc<FlashblocksState>,
    seen: std::sync::Mutex<Vec<(u64, u64)>>,
}

impl Tee {
    fn new(state: std::sync::Arc<FlashblocksState>) -> Self {
        Self { state, seen: std::sync::Mutex::new(Vec::new()) }
    }

    /// `(block_number, index)` of every slice delivered so far.
    fn seen(&self) -> Vec<(u64, u64)> {
        self.seen.lock().expect("not poisoned").clone()
    }
}

impl FlashblocksReceiver for Tee {
    fn on_flashblock_received(&self, flashblock: MantleFlashblockPayload) {
        self.seen
            .lock()
            .expect("not poisoned")
            .push((flashblock.metadata.block_number, flashblock.index));
        self.state.on_flashblock_received(flashblock);
    }
}

/// Polls until the overlay holds `tx_hash`, rather than sleeping a guess.
///
/// Two asynchronous hops stand between the producer publishing and the overlay
/// showing it — the socket, then the processor's queue — so the wait is on the
/// outcome, not on a duration.
async fn wait_for_overlay_tx(state: &FlashblocksState, tee: &Tee, tx_hash: B256) {
    for _ in 0..200 {
        if let Some(pending) = state.get_pending_blocks().as_ref() &&
            pending.has_transaction_hash(&tx_hash)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let overlay = state.get_pending_blocks().as_ref().map(|p| {
        format!("block {}, index {}", p.latest_block_number(), p.latest_flashblock_index())
    });
    panic!(
        "the overlay never picked up {tx_hash:?}; slices delivered: {:?}; overlay: {overlay:?}",
        tee.seen(),
    );
}

/// What the whole subsystem is for: a transaction the producer sliced is
/// visible on the consumer's `pending` overlay before the block is sealed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sliced_transaction_reaches_the_consumer_s_overlay() {
    reth_tracing::init_test_tracing();
    // Not `all_preconfs`: the pool arm is what slices, and it skips
    // preconf-eligible transactions on purpose — see `producer_e2e`.
    let sender = Wallet::default().inner.address();
    let cfg = PreconfCfgBuilder::new().whitelist_pair(sender, RECIPIENT).build();
    let (node, _http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, producer_cfg()).await;

    let (consumer, tee) = attach_consumer!(node, fb_addr);
    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    let tx_hash = keccak256(&raw);
    node.rpc.inject_tx(raw).await.expect("inject transfer");
    // Pool validation is asynchronous; give it a moment before the build starts.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // `l1_attrs`, not the harness's default attributes: the consumer reads the
    // L1-attributes deposit out of every block it executes, so a block built
    // without one is a block it cannot use. The hand-built slices in the
    // consumer suites carry that deposit for the same reason.
    //
    // Not canonicalized: the overlay is the subject here, and the chain
    // catching up to it is exactly what clears it.
    let pid = crate::fcu_v3_start!(node, genesis, crate::helpers::l1_attrs(1, 1));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let payload = crate::get_payload_v5!(node, pid);

    assert!(
        payload.block().body().transactions().any(|tx| keccak256(tx.encoded_2718()) == tx_hash),
        "the premise: the producer put the transaction in the block it sliced",
    );

    wait_for_overlay_tx(&consumer, &tee, tx_hash).await;

    let pending = consumer.get_pending_blocks().as_ref().cloned().expect("an overlay");
    assert_eq!(
        pending.latest_block_number(),
        1,
        "the overlay tracks the block the producer is building, not the canonical tip",
    );
}

/// The producer restarts a block's slice indices at zero when it builds that
/// height again. The consumer has to follow it across that, which is the pair
/// of defects this file exists to keep fixed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_overlay_survives_a_second_build_of_one_height() {
    reth_tracing::init_test_tracing();
    let sender = Wallet::default().inner.address();
    let cfg = PreconfCfgBuilder::new().whitelist_pair(sender, RECIPIENT).build();
    let (node, _http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, producer_cfg()).await;

    let (consumer, tee) = attach_consumer!(node, fb_addr);
    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;

    // First attempt at height 1, sliced and then left behind.
    let _abandoned = crate::fcu_v3_start!(node, genesis, crate::helpers::l1_attrs(1, 100));
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Second attempt at the same height against a different L1 origin, which
    // is what an L1 reorg produces. Its slice indices restart at zero.
    let raw = signed_transfer(&wallet, chain_id, 0).await;
    let tx_hash = keccak256(&raw);
    node.rpc.inject_tx(raw).await.expect("inject transfer");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let pid = crate::fcu_v3_start!(node, genesis, crate::helpers::l1_attrs(1, 200));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let payload = crate::get_payload_v5!(node, pid);

    assert!(
        payload.block().body().transactions().any(|tx| keccak256(tx.encoded_2718()) == tx_hash),
        "the premise: the rebuild is the one that carries the transaction",
    );

    let restarts = tee.seen().iter().filter(|(_, index)| *index == 0).count();
    assert!(restarts >= 2, "the premise: the producer restarted the indices; saw {:?}", tee.seen());

    wait_for_overlay_tx(&consumer, &tee, tx_hash).await;
}

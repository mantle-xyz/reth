//! What a slice says the block's DA footprint is.
//!
//! Post-Jovian a header's `blob_gas_used` stops meaning blob gas and starts
//! meaning the block's DA footprint. The sealed block gets it from the
//! executor, which accumulates a per-transaction estimate it does not hand out
//! through any trait — so the slice path, which assembles its own
//! `BlockExecutionResult`, has nothing to copy and used to leave the field at
//! zero. Subscribers reconstructing a header from a slice would then read zero
//! for a block that will seal with a real number: on the devnet 12% of blocks
//! sealed with a non-zero footprint.
//!
//! It does not have to come from the executor. The builder already accumulates
//! the same per-transaction estimate for its own limit checks, and the scalar
//! is a per-block constant it already reads — which is how base does it, in
//! `blob_fields`: `cumulative_da_bytes_used * scalar`.
//!
//! The shared test genesis activates Jovian and seeds the `L1Block` predeploy
//! with a DA footprint gas scalar, so this is reachable without a genesis of
//! its own — and so is every other suite, which is the point: the scalar
//! normally arrives in each block's L1 info transaction, and this harness sends
//! none.

use std::time::Duration;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, address};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use futures_util::StreamExt;
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use mantle_reth_preconf::flashblocks::FlashblockProducerConfig;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::helpers::PreconfCfgBuilder;

const RECIPIENT: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");

/// Calldata that compresses badly, so the DA estimate climbs off its 100-byte
/// floor and the footprint is visibly larger than a bare transfer's.
async fn bulky_transaction(wallet: &Wallet, chain_id: u64, nonce: u64) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT)),
        gas: Some(200_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        input: TransactionInput::new(
            (0..2_000u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
                .collect::<Vec<u8>>()
                .into(),
        ),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

macro_rules! build_one_block {
    ($node:expr, $pause:expr) => {{
        let attrs = $node.payload.next_attributes();
        let fcu_state = $node.current_forkchoice_state().expect("forkchoice state");
        let payload_id = $node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(fcu_state, Some(attrs))
            .await
            .expect("FCU must succeed")
            .payload_id
            .expect("payload_id present");

        tokio::time::sleep($pause).await;

        $node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload build")
    }};
}

/// A block that carries transactions seals with a non-zero DA footprint.
///
/// Guards the premise the slice assertion rests on. Without it a genesis that
/// quietly failed to activate Jovian, or a scalar that failed to reach the
/// contract's storage, would leave every footprint at zero and the comparison
/// downstream would hold for the wrong reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sealed_block_carries_its_da_footprint() {
    let cfg = PreconfCfgBuilder::new().build();
    let (mut node, _http, wallet, chain_id) = crate::launch_preconf_node!(cfg).await;

    for nonce in 0..3u64 {
        let raw = bulky_transaction(&wallet, chain_id, nonce).await;
        node.rpc.inject_tx(raw).await.expect("inject");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let payload = build_one_block!(node, Duration::from_millis(400));
    let header = payload.block().header();

    // No deposit: this harness sends no L1 info transaction, which is also why
    // the scalar had to be seeded into genesis storage.
    assert_eq!(
        payload.block().body().transactions().count(),
        3,
        "the three injected transactions and nothing else",
    );
    assert!(
        header.blob_gas_used.is_some_and(|footprint| footprint > 0),
        "post-Jovian a block with transactions must seal with a DA footprint; got {:?}",
        header.blob_gas_used,
    );
}

/// Slice fast enough that a held-open block produces several, and let the OS
/// pick the port so concurrent tests never collide.
fn flashblocks_cfg() -> FlashblockProducerConfig {
    FlashblockProducerConfig {
        addr: std::net::Ipv4Addr::LOCALHOST.into(),
        port: 0,
        block_time: Duration::from_millis(100),
        ..Default::default()
    }
}

/// Every slice reports the DA footprint of the block as far as it has been
/// built, and the last one agrees with the block that seals.
///
/// The final slice covers the same transactions as the sealed block, so the
/// two numbers describe the same thing and have to match. Earlier slices cover
/// a prefix, so theirs may only be smaller — never larger, and never zero once
/// the block has transactions in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slice_reports_the_da_footprint_of_what_it_carries() {
    let cfg = PreconfCfgBuilder::new().build();
    let (mut node, _http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let (mut subscriber, _) =
        connect_async(format!("ws://{fb_addr}")).await.expect("subscriber connects");

    for nonce in 0..3u64 {
        let raw = bulky_transaction(&wallet, chain_id, nonce).await;
        node.rpc.inject_tx(raw).await.expect("inject");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let payload = build_one_block!(node, Duration::from_millis(500));
    let sealed = payload.block().header().blob_gas_used.expect("post-Jovian header carries it");

    let mut slices = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, subscriber.next()).await {
        if let Message::Text(text) = message {
            slices.push(
                serde_json::from_str::<MantleFlashblockPayload>(&text).expect("decodable slice"),
            );
        }
    }

    // The premise, stated rather than assumed: without a non-zero sealed
    // footprint every comparison below would hold vacuously.
    assert!(sealed > 0, "the sealed block was supposed to have a DA footprint");
    assert!(!slices.is_empty(), "the block was supposed to be sliced");

    let reported: Vec<Option<u64>> = slices.iter().map(|s| s.diff.blob_gas_used).collect();
    assert!(
        reported.iter().all(|footprint| footprint.is_some_and(|f| f <= sealed)),
        "a slice covers a prefix of the block, so its footprint cannot exceed the block's \
         ({sealed}); slices reported {reported:?}",
    );
    assert_eq!(
        reported.last().copied().flatten(),
        Some(sealed),
        "the last slice covers what the block sealed with, so it has to report the same \
         footprint; slices reported {reported:?}",
    );
}

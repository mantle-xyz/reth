//! DA-footprint (H3) gate on the preconf apply path.
//!
//! The preconf builder applies preconf txs against the same in-flight
//! `State<DB>` that gets sealed. If a preconf tx's data-availability
//! footprint would push the block past a configured DA limit, the sealed
//! block would be DA-invalid and op-node would reject it — silently
//! breaking the "receipt returned → tx lands" commitment. The builder
//! therefore runs a DA pre-check (`payload_builder::preconf_da_check`)
//! before applying, mirroring the pool best-tx path's
//! `ExecutionInfo::is_tx_over_limits` DA logic. See design §5.5.1.
//!
//! These tests install a tight `OpDAConfig` per-tx limit via the
//! `launch_preconf_node!(cfg, spec, da_config = ...)` macro variant and
//! verify:
//!
//! - `preconf_tx_over_da_limit_rejected_and_not_on_chain` — a large-calldata
//!   preconf tx (estimated DA well over the limit) is rejected by the gate;
//!   the client sees an error and the tx never lands.
//! - `preconf_tx_within_da_limit_lands` — a plain transfer (estimated DA at
//!   the 100-byte floor) under the same config lands normally, guarding
//!   against the gate over-rejecting.
//!
//! DA estimate scale (`op_alloy_flz::tx_estimated_size_fjord_bytes`):
//! every tx has a 100-byte floor (`MIN_TX_SIZE_SCALED / 1e6`); the estimate
//! only climbs above that for txs whose fastlz-compressed size exceeds
//! ~170 bytes. A plain 21k-gas transfer sits at the 100-byte floor; a few
//! KB of poorly-compressible calldata pushes the estimate into the
//! thousands. With `max_da_tx_size = 1000` the two cases straddle the
//! limit unambiguously.

use super::helpers::{mantle_test_chain_spec, send_preconf, PreconfCfgBuilder};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{keccak256, Address, B256, Bytes, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_optimism_payload_builder::config::OpDAConfig;

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Per-tx DA limit (bytes) used by both tests. Chosen to sit above the
/// 100-byte floor (so a plain transfer passes) but well below a few-KB
/// calldata tx's estimate (so the large tx is rejected).
const MAX_DA_TX_SIZE: u64 = 1_000;

/// Sign a transfer to `RECIPIENT` carrying `calldata`. Gas is sized to
/// cover intrinsic + calldata cost so pool validation never rejects on
/// gas grounds — the only interesting gate is the builder DA check.
async fn signed_call_with_calldata(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
    calldata: Vec<u8>,
    gas: u64,
) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse::<Address>().unwrap())),
        gas: Some(gas),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(0u64)),
        input: TransactionInput::new(calldata.into()),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Large, effectively incompressible calldata so the fastlz DA estimate
/// lands far above `MAX_DA_TX_SIZE`. A keccak-derived byte stream is
/// uniformly random, so fastlz cannot shrink it (unlike a short repeating
/// pattern, which compresses back toward the 100-byte floor).
fn incompressible_calldata(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut seed = keccak256(b"mantle-preconf-da-footprint-test");
    while out.len() < len {
        seed = keccak256(seed.as_slice());
        out.extend_from_slice(seed.as_slice());
    }
    out.truncate(len);
    out
}

/// A preconf tx whose DA footprint exceeds the configured per-tx limit is
/// rejected by the builder's DA gate: the client observes an error (not
/// `Success`), and the tx is absent from the sealed block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_tx_over_da_limit_rejected_and_not_on_chain() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let sender = Wallet::default().with_chain_id(chain_id).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        // Per-tx gas cap must clear the calldata-heavy tx so it is not
        // rejected by the preconf per-tx gas gate before reaching DA.
        .max_gas_per_tx(5_000_000)
        .max_gas_per_block(10_000_000)
        .build();

    // ~4 KB of incompressible calldata → DA estimate in the thousands of
    // bytes, far over MAX_DA_TX_SIZE (1000).
    let da_config = OpDAConfig::new(MAX_DA_TX_SIZE, 30_000_000);
    let (mut node, http, wallet, _chain_id) =
        launch_preconf_node!(cfg, mantle_test_chain_spec(), da_config = da_config).await;

    let raw_tx =
        signed_call_with_calldata(chain_id, &wallet, 0, incompressible_calldata(4_096), 300_000)
            .await;
    let tx_hash = keccak256(&raw_tx);

    let attrs = node.payload.next_attributes();
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU must succeed")
        .payload_id
        .expect("payload_id present");

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw_tx).await });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    // The DA gate rejects before apply → dispatch marks the entry Failed
    // and sends the typed `DaLimitExceeded` error back, which the RPC
    // handler surfaces as a JSON-RPC error.
    let rpc_result = rpc_task.await.expect("rpc join");
    match rpc_result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("DA limit") || msg.contains("exceeds DA"),
                "expected a DA-limit rejection error, got: {msg}"
            );
        }
        Ok(event) => panic!(
            "expected DA rejection error, got Ok({:?}) reason={:?}",
            event.status, event.reason
        ),
    }

    // SLA: the over-DA tx must not have landed on chain.
    let sealed: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        !sealed.contains(&tx_hash),
        "over-DA preconf tx must NOT land in the sealed block; sealed={sealed:?}"
    );
}

/// Companion guard: under the same tight DA config, a plain transfer
/// (estimated DA at the 100-byte floor, well under the 1000-byte limit)
/// passes the gate and lands normally. Protects against the gate
/// over-rejecting legitimate small txs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_tx_within_da_limit_lands() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let sender = Wallet::default().with_chain_id(chain_id).inner.address();

    let cfg =
        PreconfCfgBuilder::new().whitelist_from(sender).whitelist_to(recipient).build();

    let da_config = OpDAConfig::new(MAX_DA_TX_SIZE, 30_000_000);
    let (mut node, http, wallet, _chain_id) =
        launch_preconf_node!(cfg, mantle_test_chain_spec(), da_config = da_config).await;

    // Empty calldata, 21k gas → DA estimate at the 100-byte floor.
    let raw_tx = signed_call_with_calldata(chain_id, &wallet, 0, Vec::new(), 21_000).await;
    let tx_hash = keccak256(&raw_tx);

    let attrs = node.payload.next_attributes();
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU must succeed")
        .payload_id
        .expect("payload_id present");

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw_tx).await });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("small preconf must succeed");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "small tx under the DA limit must succeed; got {:?} reason={:?}",
        event.status,
        event.reason
    );

    let sealed: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed.contains(&tx_hash),
        "within-DA preconf tx must land in the sealed block; sealed={sealed:?}"
    );
}

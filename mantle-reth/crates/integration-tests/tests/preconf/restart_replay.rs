//! Journal-replay behaviour on startup.
//!
//! When a preconf commitment has been persisted to the on-disk journal but
//! its block never made it to canonical (e.g. because the previous process
//! crashed), the next process must honour the promise: it opens the
//! journal, re-injects the tx into the pool, pushes a fifo entry with
//! `PreconfSource::Replay` and lands the tx in the first block it builds.
//!
//! The test constructs the journal file by hand (JSON Lines, one
//! `JournalEntry` per line) and launches the node against it — this is the
//! observable end of the restart path without needing to actually restart
//! a process.

use super::helpers::{mantle_test_chain_spec, PreconfCfgBuilder};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{keccak256, Address, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::JournalEntry;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

async fn signed_transfer(chain_id: u64, wallet: &Wallet, nonce: u64) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_replay_lands_promised_tx_in_next_block() {
    let recipient: Address = RECIPIENT.parse().unwrap();

    // The launch macro derives its wallet + chain id from the same chain
    // spec used here, so pre-signing against it produces bytes that match
    // whatever the launched node will see.
    let chain_id = mantle_test_chain_spec().chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let wallet_addr = wallet.inner.address();

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let tx_hash = keccak256(&raw_tx);

    // Pre-seed a JSON-Lines journal file with a single promised commitment.
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-journal-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&journal_dir).expect("mkdir journal_dir");
    let journal_file = journal_dir.join("preconf.journal");

    let entry = JournalEntry {
        hash: tx_hash,
        tx_rlp: raw_tx.clone().into(),
        block_height: 1,
        committed_at_ms: 0,
    };
    let mut line = serde_json::to_vec(&entry).expect("encode JournalEntry");
    line.push(b'\n');
    std::fs::write(&journal_file, &line).expect("write journal file");

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, _http, _node_wallet, _launched_chain_id) = launch_preconf_node!(cfg).await;

    // Drive one payload build. Journal restore pushed the entry into the
    // fifo with `Replay` source during startup; the dispatch loop must
    // apply it in this block.
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

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let block = payload.block();
    let sealed: Vec<alloy_primitives::B256> = block
        .body()
        .transactions()
        .map(|tx| keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed.contains(&tx_hash),
        "journal-restored tx must land in the first block after startup; \
         hash {tx_hash:?} not in sealed {sealed:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}

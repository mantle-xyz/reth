//! Isolated process: exact metric counts and error logs must not include other node tests.

#![recursion_limit = "1024"]
#![allow(missing_docs)]

#[path = "preconf/helpers.rs"]
#[allow(dead_code)]
pub mod helpers;

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use alloy_consensus::TxEnvelope;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use helpers::{PreconfCfgBuilder, mantle_test_chain_spec};
use mantle_reth_preconf::{ClaimKind, JournalEntry, types::PreconfSource};
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_node_metrics::recorder::install_prometheus_recorder;
use reth_optimism_payload_builder::config::OpDAConfig;

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capacity_rejections_and_expired_builds_are_observable() {
    let metrics = install_prometheus_recorder().handle();
    let logs = LogWriter(Arc::new(Mutex::new(Vec::new())));
    let writer = logs.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish(),
    )
    .unwrap();

    let chain_id = mantle_test_chain_spec().chain().id();
    let signers = Wallet::new(4).with_chain_id(chain_id).wallet_gen();
    let recipient: Address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8".parse().unwrap();
    let journal_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("preconf.journal");
    let mut entries = Vec::new();
    let mut live_tx = None;
    // Independent senders ensure each transaction reaches the capacity gate.
    for (signer, kind) in [
        (&signers[0], ClaimKind::Promised),
        (&signers[2], ClaimKind::Announced),
        (&signers[3], ClaimKind::Announced),
    ] {
        let mut input = Vec::new();
        let mut seed = keccak256(b"capacity-rejection");
        for _ in 0..128 {
            seed = keccak256(seed);
            input.extend_from_slice(seed.as_slice());
        }
        let tx: TxEnvelope = TransactionTestContext::sign_tx(
            signer.clone(),
            TransactionRequest {
                chain_id: Some(chain_id),
                nonce: Some(0),
                to: Some(TxKind::Call(recipient)),
                gas: Some(300_000),
                max_fee_per_gas: Some(20_000_000_000),
                max_priority_fee_per_gas: Some(20_000_000_000),
                input: TransactionInput::new(input.into()),
                ..Default::default()
            },
        )
        .await;
        if signer.address() == signers[3].address() {
            live_tx = Some(tx.clone());
        }
        let raw: Bytes = tx.encoded_2718().into();
        entries.push(JournalEntry {
            hash: keccak256(&raw),
            tx_rlp: raw,
            block_height: 1,
            committed_at_ms: 0,
            kind,
        });
    }
    let contents = entries[..2]
        .iter()
        .map(|entry| serde_json::to_string(entry).unwrap() + "\n")
        .collect::<String>();
    std::fs::write(&journal_path, contents).unwrap();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(signers[0].address())
        .whitelist_from(signers[2].address())
        .whitelist_from(signers[3].address())
        .whitelist_to(recipient)
        .max_gas_per_tx(5_000_000)
        .max_gas_per_block(10_000_000)
        .journal_path(journal_path)
        .build();
    let (mut node, _http, _wallet, _, _classifier, fifo, _) = launch_preconf_node!(
        @build cfg, mantle_test_chain_spec(),
        |svc| mantle_reth_cli::node::MantleNode::default().with_preconf(svc)
            .with_da_config(OpDAConfig::new(1_000, 30_000_000))
    )
    .await;
    // A fresh request has no published claim and must not count as either kind of breach.
    fifo.push_if_absent(Arc::new(live_tx.unwrap()), signers[3].address(), PreconfSource::Rpc).await;
    let attrs = node.payload.next_attributes();
    let forkchoice = node.current_forkchoice_state().unwrap();
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(forkchoice, Some(attrs))
        .await
        .unwrap()
        .payload_id
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !metrics.render().lines().any(|line| line == "reth_preconf_fifo_da_rejected_total 3")
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all entries must reach the permanent capacity rejection");
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .unwrap()
        .unwrap();
    let sealed = payload
        .block()
        .body()
        .transactions()
        .map(|tx| keccak256(tx.encoded_2718()))
        .collect::<Vec<_>>();
    assert!(entries.iter().all(|entry| !sealed.contains(&entry.hash)));

    let rendered = metrics.render();
    for (name, count) in [
        ("reth_preconf_fifo_da_rejected_total", 3),
        ("reth_preconf_tx_commitment_broken_total", 1),
        ("reth_preconf_tx_announced_dropped_total", 1),
    ] {
        assert!(
            rendered.lines().any(|line| line == format!("{name} {count}")),
            "expected {name} = {count}"
        );
    }
    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    for (entry, level) in entries.iter().zip(["ERROR", "WARN"]) {
        assert!(
            output.lines().any(|line| line.contains(level) &&
                line.contains("capacity") &&
                line.contains(&format!("{:?}", entry.hash))),
            "missing {level} log for {:?}:\n{output}",
            entry.hash
        );
    }
    // Historical attributes deterministically start past the slot deadline.
    // Also cover leeway that exhausts the fallback window: it must not count twice.
    for (index, leeway_ms) in [0, 200].into_iter().enumerate() {
        let cfg = PreconfCfgBuilder::new().all_preconfs().build();
        let fb = mantle_reth_preconf::FlashblockProducerConfig {
            port: 0,
            leeway_time: std::time::Duration::from_millis(leeway_ms),
            ..Default::default()
        };
        let (mut late_node, _late_http, _late_wallet, _, _addr) =
            launch_flashblocks_node!(cfg, fb).await;
        let attrs = late_node.payload.next_attributes();
        assert!(
            attrs.payload_attributes.timestamp + 2 <
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
        );
        let forkchoice = late_node.current_forkchoice_state().unwrap();
        let id = late_node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(forkchoice, Some(attrs))
            .await
            .unwrap()
            .payload_id
            .unwrap();
        let expected = format!("reth_flashblocks_deadline_already_passed_total {}", index + 1);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !metrics.render().lines().any(|line| line == expected) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("an expired build must increment its deadline counter once");
        late_node
            .inner
            .payload_builder_handle
            .resolve_kind(id, reth_node_api::PayloadKind::Earliest)
            .await
            .unwrap()
            .unwrap();
        assert!(
            metrics.render().lines().any(|line| line == expected),
            "one build must not double-count a missed deadline"
        );
    }
    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        output.lines().any(|line| line.contains("WARN") && line.contains("deadline")),
        "expired builds must be visible at warn level"
    );
}

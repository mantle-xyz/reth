//! Generate an offline replay fixture for `tests/replay.rs` from a Mantle mainnet tx.
//!
//! Fetches the transaction, receipt, block, raw RLP, `L1BlockInfo` system-contract
//! storage, and the `prestateTracer` dump, then writes a self-contained JSON
//! fixture that the replay test loads via `include_str!` (no RPC at test time).
//!
//! Usage:
//! ```text
//! cargo run -p mantle-reth-integration-tests --example gen_replay_fixture \
//!   -- <tx_hash> <out_path> [rpc_url]
//! ```

use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use serde_json::{Map, Value, json};

const DEFAULT_RPC: &str = "https://rpc.mantle.xyz";
const L1_BLOCK_CONTRACT: &str = "0x4200000000000000000000000000000000000015";
const GAS_ORACLE_CONTRACT: &str = "0x420000000000000000000000000000000000000F";
const DEPOSIT_TX_TYPE: &str = "0x7e";

/// `L1BlockInfo` source slots (mirror `tests/replay.rs` `L1BlockInfo` construction).
const L1_FEE_SLOTS: &[(&str, &str, u64)] = &[
    ("l1_base_fee", L1_BLOCK_CONTRACT, 1),
    ("token_ratio", GAS_ORACLE_CONTRACT, 0),
    ("ecotone_scalars", L1_BLOCK_CONTRACT, 3),
    ("l1_fee_overhead", L1_BLOCK_CONTRACT, 5),
    ("l1_fee_scalar", L1_BLOCK_CONTRACT, 6),
    ("blob_base_fee", L1_BLOCK_CONTRACT, 7),
    ("operator_fee_packed", L1_BLOCK_CONTRACT, 8),
];

fn hex_u64(v: &Value) -> u64 {
    u64::from_str_radix(v.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap_or(0)
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: gen_replay_fixture <tx_hash> <out_path> [rpc_url]");
        std::process::exit(1);
    }
    let tx_hash = args[1].clone();
    let out_path = args[2].clone();
    let rpc_url = args.get(3).map(String::as_str).unwrap_or(DEFAULT_RPC);

    let client = HttpClientBuilder::default().build(rpc_url)?;

    let tx: Value = client.request("eth_getTransactionByHash", rpc_params![&tx_hash]).await?;
    if tx.is_null() {
        eyre::bail!("tx not found: {tx_hash}");
    }
    let receipt: Value = client.request("eth_getTransactionReceipt", rpc_params![&tx_hash]).await?;
    let block_hex = receipt["blockNumber"].as_str().unwrap().to_string();
    let block: Value =
        client.request("eth_getBlockByNumber", rpc_params![&block_hex, false]).await?;
    let raw_tx: Value =
        client.request("eth_getRawTransactionByHash", rpc_params![&tx_hash]).await?;
    let prestate: Value = client
        .request(
            "debug_traceTransaction",
            rpc_params![&tx_hash, json!({ "tracer": "prestateTracer" })],
        )
        .await?;

    let is_deposit = tx["type"].as_str() == Some(DEPOSIT_TX_TYPE);
    // Deposit txs price gas at 0 and carry no 1559 fee fields.
    let (max_fee, max_priority) = if is_deposit {
        (0u64, 0u64)
    } else {
        let gas_price = &tx["gasPrice"];
        let max_fee = hex_u64(tx.get("maxFeePerGas").unwrap_or(gas_price));
        let max_priority = hex_u64(tx.get("maxPriorityFeePerGas").unwrap_or(gas_price));
        (max_fee, max_priority)
    };

    let mut l1_block_info = Map::new();
    for (name, addr, slot) in L1_FEE_SLOTS {
        let val: Value = client
            .request("eth_getStorageAt", rpc_params![*addr, format!("0x{slot:x}"), &block_hex])
            .await?;
        l1_block_info.insert((*name).to_string(), val);
    }

    let chain_id =
        tx.get("chainId").and_then(Value::as_str).map(|s| hex_u64(&json!(s))).unwrap_or(5000);

    let mut fixture = json!({
        "tx_hash": tx_hash,
        "block_number": hex_u64(&block_hex.clone().into()),
        "timestamp": hex_u64(&block["timestamp"]),
        "sender": tx["from"],
        "to": tx.get("to").cloned().unwrap_or(Value::Null),
        "raw_tx": raw_tx,
        "expected_gas_used": hex_u64(&receipt["gasUsed"]),
        "expected_status": receipt["status"].as_str() == Some("0x1"),
        "tx": {
            "gas_limit": hex_u64(&tx["gas"]),
            "max_fee_per_gas": max_fee,
            "max_priority_fee_per_gas": max_priority,
            "value": tx["value"],
            "input": tx["input"],
            "nonce": hex_u64(&tx["nonce"]),
            "chain_id": chain_id,
        },
        "block_env": {
            "beneficiary": block["miner"],
            "gas_limit": hex_u64(&block["gasLimit"]),
            "base_fee_per_gas": hex_u64(&block["baseFeePerGas"]),
            "mix_hash": block["mixHash"],
        },
        "l1_block_info": Value::Object(l1_block_info),
        "prestate": prestate,
    });

    // Receipt-derived L1 fee oracle (post-Arsia receipts only, absent on deposits). Lets the
    // replay test assert `L1BlockInfo` scalar extraction and the L1 fee against the chain's own
    // decoding, independent of EVM gas (which the maxed sender balance would otherwise mask).
    {
        let obj = fixture.as_object_mut().unwrap();
        for (out_key, receipt_key) in [
            ("expected_l1_fee", "l1Fee"),
            ("expected_l1_base_fee_scalar", "l1BaseFeeScalar"),
            ("expected_l1_blob_base_fee_scalar", "l1BlobBaseFeeScalar"),
            ("expected_operator_fee_scalar", "operatorFeeScalar"),
            ("expected_operator_fee_constant", "operatorFeeConstant"),
        ] {
            match receipt.get(receipt_key) {
                Some(v) if !v.is_null() => {
                    obj.insert(out_key.to_string(), v.clone());
                }
                _ => {}
            }
        }
        if let Some(v) = receipt.get("daFootprintGasScalar").filter(|v| !v.is_null()) {
            obj.insert("expected_da_footprint_gas_scalar".to_string(), json!(hex_u64(v)));
        }
    }

    if is_deposit {
        let obj = fixture.as_object_mut().unwrap();
        obj.insert("source_hash".to_string(), tx["sourceHash"].clone());
        obj.insert("mint".to_string(), json!(hex_u64(tx.get("mint").unwrap_or(&json!("0x0")))));
        obj.insert(
            "is_system_transaction".to_string(),
            json!(tx.get("isSystemTx").and_then(Value::as_bool).unwrap_or(false)),
        );
    }

    std::fs::write(&out_path, serde_json::to_string_pretty(&fixture)?)?;
    eprintln!(
        "wrote {out_path}: block={} gas={} status={} deposit={} accounts={}",
        fixture["block_number"],
        fixture["expected_gas_used"],
        fixture["expected_status"],
        is_deposit,
        fixture["prestate"].as_object().map(Map::len).unwrap_or(0),
    );
    Ok(())
}

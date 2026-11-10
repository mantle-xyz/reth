//! `eth_simulateV1` through a booted `MantleNode`.

use crate::helpers::with_mantle_rpc_client;
use jsonrpsee::{core::client::ClientT, rpc_params};
use serde_json::Value;

fn quantity(value: &Value) -> u64 {
    u64::from_str_radix(value.as_str().expect("hex quantity").trim_start_matches("0x"), 16)
        .expect("hex quantity")
}

/// Blocks simulated without a `time` land Mantle's 2s block time apart, and so do the empty
/// blocks a number gap leaves.
#[tokio::test]
async fn simulated_blocks_step_two_seconds_via_rpc() {
    with_mantle_rpc_client(|client| async move {
        let latest: Value = client
            .request("eth_getBlockByNumber", rpc_params!["latest", false])
            .await
            .expect("latest block");
        let (number, time) = (quantity(&latest["number"]), quantity(&latest["timestamp"]));

        let payload = serde_json::json!({
            "blockStateCalls": [
                {},
                { "blockOverrides": { "number": format!("0x{:x}", number + 4) } },
            ],
        });
        let blocks: Vec<Value> = client
            .request("eth_simulateV1", rpc_params![payload, "latest"])
            .await
            .expect("simulation runs");

        let numbers_and_times: Vec<_> = blocks
            .iter()
            .map(|block| (quantity(&block["number"]), quantity(&block["timestamp"])))
            .collect();
        assert_eq!(
            numbers_and_times,
            (1..=4).map(|i| (number + i, time + 2 * i)).collect::<Vec<_>>()
        );
    })
    .await;
}

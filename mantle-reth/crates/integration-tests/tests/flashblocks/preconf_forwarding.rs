//! Follower forwarding preserves sequencer RPC results and distinguishes transport failure.

use alloy_primitives::Bytes;
use jsonrpsee::{RpcModule, server::Server, types::ErrorObjectOwned};
use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt, PreconfTxEvent};
use reth_optimism_rpc::SequencerClient;

use crate::consumer_helpers::launch_flashblocks_consumer;

#[tokio::test(flavor = "multi_thread")]
async fn q05_forwarding_preserves_business_errors_including_data() {
    let (_harness, node) = launch_flashblocks_consumer!(3, 3);
    for expected in [
        ErrorObjectOwned::owned(-32000, "pool rejected: nonce too low", None::<()>),
        ErrorObjectOwned::owned(
            -32602,
            "invalid preconf transaction",
            Some(serde_json::json!({"reason": "unsupported type", "type": "0x4"})),
        ),
    ] {
        let server = Server::builder().build("127.0.0.1:0").await.unwrap();
        let client =
            SequencerClient::new(format!("http://{}", server.local_addr().unwrap())).await.unwrap();
        let mut module = RpcModule::new(expected.clone());
        module
            .register_method("eth_sendRawTransactionWithPreconf", |params, error, _| {
                let (raw,): (Bytes,) = params.parse()?;
                assert_eq!(raw.as_ref(), &[0xab, 0xcd]);
                Err::<serde_json::Value, _>(error.clone())
            })
            .unwrap();
        let server = server.start(module);
        let api = MantleRpcExt::new(
            node.inner.provider.clone(),
            node.rpc.inner.eth_api().clone().into(),
            Some(client),
            None,
        );
        let error = api
            .send_raw_transaction_with_preconf(Bytes::from_static(&[0xab, 0xcd]))
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        server.stop().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn q05_transport_failure_is_an_internal_error() {
    let (_harness, node) = launch_flashblocks_consumer!(3, 3);
    // Reserve the port, then close it: no sequencer can return a business rejection.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client =
        SequencerClient::new(format!("http://{}", listener.local_addr().unwrap())).await.unwrap();
    drop(listener);
    let api = MantleRpcExt::new(
        node.inner.provider.clone(),
        node.rpc.inner.eth_api().clone().into(),
        Some(client),
        None,
    );
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        api.send_raw_transaction_with_preconf(Bytes::new()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.code(), -32603);
    assert!(!error.message().contains("please try again"));
    assert!(error.data().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn q05_forwarding_preserves_success_and_rejects_malformed_events() {
    let (_harness, node) = launch_flashblocks_consumer!(3, 3);
    let valid = serde_json::json!({
        "txHash": format!("{:#x}", alloy_primitives::B256::with_last_byte(42)),
        "status": "success", "reason": "", "blockHeight": "0x2",
        "receipt": {"logs": []}
    });
    for raw in [valid.clone(), serde_json::json!({"invalid": true})] {
        let server = Server::builder().build("127.0.0.1:0").await.unwrap();
        let client =
            SequencerClient::new(format!("http://{}", server.local_addr().unwrap())).await.unwrap();
        let mut module = RpcModule::new(raw.clone());
        module
            .register_method("eth_sendRawTransactionWithPreconf", |_, raw, _| raw.clone())
            .unwrap();
        let server = server.start(module);
        let api = MantleRpcExt::new(
            node.inner.provider.clone(),
            node.rpc.inner.eth_api().clone().into(),
            Some(client),
            None,
        );
        let result = api.send_raw_transaction_with_preconf(Bytes::new()).await;
        if raw == valid {
            assert_eq!(
                result.unwrap(),
                serde_json::from_value::<PreconfTxEvent>(valid.clone()).unwrap()
            );
        } else {
            let error = result.unwrap_err();
            assert!(error.message().contains("failed to deserialise preconf event"));
        }
        server.stop().unwrap();
    }
}

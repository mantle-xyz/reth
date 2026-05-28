//! Helpers for optimism specific RPC implementations.

use crate::{SequencerClientError, SequencerMetrics};
use alloy_json_rpc::{RpcRecv, RpcSend};
use alloy_primitives::{hex, Bytes, B256};
use alloy_rpc_client::{BuiltInConnectionString, ClientBuilder, RpcClient as Client};
use alloy_rpc_types_eth::erc4337::TransactionConditional;
use alloy_transport::TransportErrorKind;
use alloy_transport_http::Http;
use futures::future::join_all;
use std::{str::FromStr, sync::Arc, time::Instant};
use thiserror::Error;
use tracing::{info, warn};

const BATCH_LOG_TARGET: &str = "rpc::sequencer::batch";

/// Sequencer client error
#[derive(Error, Debug)]
pub enum Error {
    /// Invalid scheme
    #[error("Invalid scheme of sequencer url: {0}")]
    InvalidScheme(String),
    /// Invalid header or value provided.
    #[error("Invalid header: {0}")]
    InvalidHeader(String),
    /// Invalid url
    #[error("Invalid sequencer url: {0}")]
    InvalidUrl(String),
    /// Establishing a connection to the sequencer endpoint resulted in an error.
    #[error("Failed to connect to sequencer: {0}")]
    TransportError(
        #[from]
        #[source]
        alloy_transport::TransportError,
    ),
    /// Reqwest failed to init client
    #[error("Failed to init reqwest client for sequencer: {0}")]
    ReqwestError(
        #[from]
        #[source]
        reqwest::Error,
    ),
}

/// A client to interact with a Sequencer
#[derive(Debug, Clone)]
pub struct SequencerClient {
    inner: Arc<SequencerClientInner>,
}

impl SequencerClientInner {
    /// Creates a new instance with the given endpoint and client.
    pub(crate) fn new(sequencer_endpoint: String, client: Client) -> Self {
        let metrics = SequencerMetrics::default();
        Self { sequencer_endpoint, client, metrics }
    }
}

impl SequencerClient {
    /// Creates a new [`SequencerClient`] for the given URL.
    ///
    /// If the URL is a websocket endpoint we connect a websocket instance.
    pub async fn new(sequencer_endpoint: impl Into<String>) -> Result<Self, Error> {
        Self::new_with_headers(sequencer_endpoint, Default::default()).await
    }

    /// Creates a new `SequencerClient` for the given URL with the given headers
    ///
    /// This expects headers in the form: `header=value`
    pub async fn new_with_headers(
        sequencer_endpoint: impl Into<String>,
        headers: Vec<String>,
    ) -> Result<Self, Error> {
        let sequencer_endpoint = sequencer_endpoint.into();
        let endpoint = BuiltInConnectionString::from_str(&sequencer_endpoint)?;
        if let BuiltInConnectionString::Http(url) = endpoint {
            let mut builder = reqwest::Client::builder()
                // we force use tls to prevent native issues
                .use_rustls_tls();

            if !headers.is_empty() {
                let mut header_map = reqwest::header::HeaderMap::new();
                for header in headers {
                    if let Some((key, value)) = header.split_once('=') {
                        header_map.insert(
                            key.trim()
                                .parse::<reqwest::header::HeaderName>()
                                .map_err(|err| Error::InvalidHeader(err.to_string()))?,
                            value
                                .trim()
                                .parse::<reqwest::header::HeaderValue>()
                                .map_err(|err| Error::InvalidHeader(err.to_string()))?,
                        );
                    }
                }
                builder = builder.default_headers(header_map);
            }

            let client = builder.build()?;
            Self::with_http_client(url, client)
        } else {
            let client = ClientBuilder::default().connect_with(endpoint).await?;
            let inner = SequencerClientInner::new(sequencer_endpoint, client);
            Ok(Self { inner: Arc::new(inner) })
        }
    }

    /// Creates a new [`SequencerClient`] with http transport with the given http client.
    pub fn with_http_client(
        sequencer_endpoint: impl Into<String>,
        client: reqwest::Client,
    ) -> Result<Self, Error> {
        let sequencer_endpoint: String = sequencer_endpoint.into();
        let url = sequencer_endpoint
            .parse()
            .map_err(|_| Error::InvalidUrl(sequencer_endpoint.clone()))?;

        let http_client = Http::with_client(client, url);
        let is_local = http_client.guess_local();
        let client = ClientBuilder::default().transport(http_client, is_local);

        let inner = SequencerClientInner::new(sequencer_endpoint, client);
        Ok(Self { inner: Arc::new(inner) })
    }

    /// Returns the network of the client
    pub fn endpoint(&self) -> &str {
        &self.inner.sequencer_endpoint
    }

    /// Returns the client
    pub fn client(&self) -> &Client {
        &self.inner.client
    }

    /// Returns a reference to the [`SequencerMetrics`] for tracking client metrics.
    fn metrics(&self) -> &SequencerMetrics {
        &self.inner.metrics
    }

    /// Sends a [`alloy_rpc_client::RpcCall`] request to the sequencer endpoint.
    pub async fn request<Params: RpcSend, Resp: RpcRecv>(
        &self,
        method: &str,
        params: Params,
    ) -> Result<Resp, SequencerClientError> {
        let resp =
            self.client().request::<Params, Resp>(method.to_string(), params).await.inspect_err(
                |err| {
                    warn!(
                        target: "rpc::sequencer",
                        %err,
                        "HTTP request to sequencer failed",
                    );
                },
            )?;
        Ok(resp)
    }

    /// Forwards a transaction to the sequencer endpoint.
    pub async fn forward_raw_transaction(&self, tx: &[u8]) -> Result<B256, SequencerClientError> {
        let start = Instant::now();
        let rlp_hex = hex::encode_prefixed(tx);
        let tx_hash =
            self.request("eth_sendRawTransaction", (rlp_hex,)).await.inspect_err(|err| {
                warn!(
                    target: "rpc::eth",
                    %err,
                    "Failed to forward transaction to sequencer",
                );
            })?;
        self.metrics().record_forward_latency(start.elapsed());
        Ok(tx_hash)
    }

    /// Forwards multiple raw transactions to the sequencer in a single outbound
    /// JSON-RPC batch.
    ///
    /// Returns one result per input transaction, preserving order. The outer `Result`
    /// is an error only if the batch envelope itself cannot be sent (transport
    /// failure); per-transaction failures from the sequencer are reported via the
    /// inner `Result`s.
    ///
    /// For a single input this falls back to [`Self::forward_raw_transaction`] to
    /// avoid the batch wrapper overhead.
    pub async fn forward_raw_transactions(
        &self,
        txs: &[Bytes],
    ) -> Result<Vec<Result<B256, SequencerClientError>>, SequencerClientError> {
        if txs.is_empty() {
            return Ok(Vec::new());
        }
        if txs.len() == 1 {
            let t = Instant::now();
            let res = self.forward_raw_transaction(&txs[0]).await;
            info!(
                target: BATCH_LOG_TARGET,
                n = 1,
                ok = res.is_ok(),
                total_us = t.elapsed().as_micros() as u64,
                "forward_raw_transactions: single-tx fast path"
            );
            return Ok(vec![res]);
        }

        let n = txs.len();
        let start = Instant::now();
        let t_build = Instant::now();
        let mut batch = self.client().new_batch();
        let mut waiters: Vec<Option<_>> = Vec::with_capacity(txs.len());
        for tx in txs {
            let params = (hex::encode_prefixed(tx),);
            match batch.add_call::<_, B256>("eth_sendRawTransaction", &params) {
                Ok(waiter) => waiters.push(Some(waiter)),
                Err(_) => waiters.push(None),
            }
        }
        let build_us = t_build.elapsed().as_micros() as u64;

        // Drive the batch send and the per-entry waiters concurrently. The send
        // future populates the oneshot channels the waiters resolve on; if the
        // send itself fails, the channels are dropped and the waiters resolve
        // with a transport error.
        let t_send = Instant::now();
        let send_fut = batch.send();
        let waiter_futs = waiters.into_iter().map(|w| async move {
            match w {
                Some(waiter) => Some(waiter.await),
                None => None,
            }
        });
        let (_send_res, results) = tokio::join!(send_fut, join_all(waiter_futs));
        let send_us = t_send.elapsed().as_micros() as u64;

        let elapsed = start.elapsed();
        self.metrics().record_forward_latency(elapsed);

        let mapped: Vec<Result<B256, SequencerClientError>> = results
            .into_iter()
            .map(|waiter_res| match waiter_res {
                Some(Ok(hash)) => Ok(hash),
                Some(Err(err)) => Err(SequencerClientError::HttpError(err)),
                None => Err(SequencerClientError::HttpError(TransportErrorKind::custom_str(
                    "failed to serialize batched eth_sendRawTransaction",
                ))),
            })
            .inspect(|res| {
                if let Err(err) = res {
                    warn!(target: "rpc::eth", %err, "Failed to forward batched transaction to sequencer");
                }
            })
            .collect();
        let ok_count = mapped.iter().filter(|r| r.is_ok()).count();
        info!(
            target: BATCH_LOG_TARGET,
            n,
            ok = ok_count,
            build_us,
            send_us,
            total_us = elapsed.as_micros() as u64,
            "forward_raw_transactions: batch sent to sequencer"
        );
        Ok(mapped)
    }

    /// Forwards a transaction conditional to the sequencer endpoint.
    pub async fn forward_raw_transaction_conditional(
        &self,
        tx: &[u8],
        condition: TransactionConditional,
    ) -> Result<B256, SequencerClientError> {
        let start = Instant::now();
        let rlp_hex = hex::encode_prefixed(tx);
        let tx_hash = self
            .request("eth_sendRawTransactionConditional", (rlp_hex, condition))
            .await
            .inspect_err(|err| {
                warn!(
                    target: "rpc::eth",
                    %err,
                    "Failed to forward transaction conditional for sequencer",
                );
            })?;
        self.metrics().record_forward_latency(start.elapsed());
        Ok(tx_hash)
    }

    /// Forwards a transaction with preconfirmation to the sequencer endpoint.
    pub async fn forward_raw_transaction_with_preconf(
        &self,
        tx: &[u8],
    ) -> Result<reth_rpc_eth_api::PreconfTxEvent, SequencerClientError> {
        let start = Instant::now();
        let rlp_hex = hex::encode_prefixed(tx);
        let preconf_event = self
            .request("eth_sendRawTransactionWithPreconf", (rlp_hex,))
            .await
            .inspect_err(|err| {
                warn!(
                    target: "rpc::eth",
                    %err,
                    "Failed to forward transaction with preconf to sequencer",
                );
            })?;
        self.metrics().record_forward_latency(start.elapsed());
        Ok(preconf_event)
    }
}

#[derive(Debug)]
struct SequencerClientInner {
    /// The endpoint of the sequencer
    sequencer_endpoint: String,
    /// The client
    client: Client,
    // Metrics for tracking sequencer forwarding
    metrics: SequencerMetrics,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U64;

    #[tokio::test]
    async fn test_http_body_str() {
        let client = SequencerClient::new("http://localhost:8545").await.unwrap();

        let request = client
            .client()
            .make_request("eth_getBlockByNumber", (U64::from(10),))
            .serialize()
            .unwrap()
            .take_request();
        let body = request.get();

        assert_eq!(
            body,
            r#"{"method":"eth_getBlockByNumber","params":["0xa"],"id":0,"jsonrpc":"2.0"}"#
        );

        let condition = TransactionConditional::default();

        let request = client
            .client()
            .make_request(
                "eth_sendRawTransactionConditional",
                (format!("0x{}", hex::encode("abcd")), condition),
            )
            .serialize()
            .unwrap()
            .take_request();
        let body = request.get();

        assert_eq!(
            body,
            r#"{"method":"eth_sendRawTransactionConditional","params":["0x61626364",{"knownAccounts":{}}],"id":1,"jsonrpc":"2.0"}"#
        );
    }

    #[tokio::test]
    async fn forward_raw_transactions_empty_returns_empty() {
        let client = SequencerClient::new("http://localhost:8545").await.unwrap();
        let results = client.forward_raw_transactions(&[]).await.unwrap();
        assert!(results.is_empty());
    }

    /// Minimal HTTP/1.1 single-shot server. Accepts one POST, returns the
    /// caller-supplied body, and hands back the captured request body so the
    /// test can assert on its shape.
    async fn one_shot_http_server(
        response_body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read until we have the full body. HTTP/1.1 with Content-Length.
            let mut buf = vec![0u8; 16 * 1024];
            let mut total = 0usize;
            let body = loop {
                let n = sock.read(&mut buf[total..]).await.unwrap();
                if n == 0 {
                    break String::from_utf8_lossy(&buf[..total]).to_string();
                }
                total += n;
                let text = std::str::from_utf8(&buf[..total]).unwrap_or("");
                if let Some(hdr_end) = text.find("\r\n\r\n") {
                    let content_length: usize = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    let body_start = hdr_end + 4;
                    if total >= body_start + content_length {
                        break text[body_start..body_start + content_length].to_string();
                    }
                }
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                response_body.len(),
                response_body,
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            body
        });
        (url, handle)
    }

    #[tokio::test]
    async fn forward_raw_transactions_sends_single_batch_post_for_multiple_txs() {
        // Two raw txs → exactly one HTTP POST with a JSON-RPC batch body
        // containing two eth_sendRawTransaction entries, in order.
        let canned_response = r#"[{"jsonrpc":"2.0","id":0,"result":"0x1111111111111111111111111111111111111111111111111111111111111111"},{"jsonrpc":"2.0","id":1,"result":"0x2222222222222222222222222222222222222222222222222222222222222222"}]"#;
        let (url, srv) = one_shot_http_server(canned_response).await;
        let client = SequencerClient::new(url).await.unwrap();

        let txs = vec![Bytes::from(vec![0xaa, 0xbb]), Bytes::from(vec![0xcc, 0xdd])];
        let results = client.forward_raw_transactions(&txs).await.unwrap();

        let body = srv.await.unwrap();
        assert!(
            body.starts_with('[') && body.trim_end().ends_with(']'),
            "expected JSON-RPC batch array, got: {body}"
        );
        assert_eq!(
            body.matches("\"method\":\"eth_sendRawTransaction\"").count(),
            2,
            "expected two eth_sendRawTransaction entries, got: {body}"
        );
        assert!(body.contains("0xaabb"), "missing first tx in body: {body}");
        assert!(body.contains("0xccdd"), "missing second tx in body: {body}");

        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(results[1].is_ok());
        assert_eq!(
            results[0].as_ref().unwrap().to_string(),
            "0x1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert_eq!(
            results[1].as_ref().unwrap().to_string(),
            "0x2222222222222222222222222222222222222222222222222222222222222222"
        );
    }

    #[tokio::test]
    async fn forward_raw_transactions_single_tx_uses_single_call_not_batch() {
        // N=1 short-circuits to forward_raw_transaction → body is a JSON
        // object, not an array.
        let canned_response = r#"{"jsonrpc":"2.0","id":0,"result":"0x3333333333333333333333333333333333333333333333333333333333333333"}"#;
        let (url, srv) = one_shot_http_server(canned_response).await;
        let client = SequencerClient::new(url).await.unwrap();

        let results =
            client.forward_raw_transactions(&[Bytes::from(vec![0x11, 0x22])]).await.unwrap();
        let body = srv.await.unwrap();

        assert!(body.starts_with('{'), "expected single JSON-RPC object (not batch), got: {body}");
        assert!(body.contains("\"method\":\"eth_sendRawTransaction\""));
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());
    }

    #[tokio::test]
    async fn forward_raw_transactions_propagates_per_item_errors() {
        // Sequencer returns error for one entry, success for the other —
        // the wrapper must report per-item Err/Ok without failing overall.
        let canned_response = r#"[{"jsonrpc":"2.0","id":0,"error":{"code":-32000,"message":"already known"}},{"jsonrpc":"2.0","id":1,"result":"0x4444444444444444444444444444444444444444444444444444444444444444"}]"#;
        let (url, srv) = one_shot_http_server(canned_response).await;
        let client = SequencerClient::new(url).await.unwrap();

        let results = client
            .forward_raw_transactions(&[Bytes::from(vec![0xaa]), Bytes::from(vec![0xbb])])
            .await
            .unwrap();
        let _body = srv.await.unwrap();

        assert_eq!(results.len(), 2);
        assert!(results[0].is_err(), "first entry should be error, got {:?}", results[0]);
        let err_str = results[0].as_ref().err().unwrap().to_string();
        assert!(
            err_str.contains("already known"),
            "error should preserve sequencer message: {err_str}"
        );
        assert!(results[1].is_ok(), "second entry should be ok, got {:?}", results[1]);
    }

    #[tokio::test]
    #[ignore = "Start if WS is reachable at ws://localhost:8546"]
    async fn test_ws_body_str() {
        let client = SequencerClient::new("ws://localhost:8546").await.unwrap();

        let request = client
            .client()
            .make_request("eth_getBlockByNumber", (U64::from(10),))
            .serialize()
            .unwrap()
            .take_request();
        let body = request.get();

        assert_eq!(
            body,
            r#"{"method":"eth_getBlockByNumber","params":["0xa"],"id":0,"jsonrpc":"2.0"}"#
        );

        let condition = TransactionConditional::default();

        let request = client
            .client()
            .make_request(
                "eth_sendRawTransactionConditional",
                (format!("0x{}", hex::encode("abcd")), condition),
            )
            .serialize()
            .unwrap()
            .take_request();
        let body = request.get();

        assert_eq!(
            body,
            r#"{"method":"eth_sendRawTransactionConditional","params":["0x61626364",{"knownAccounts":{}}],"id":1,"jsonrpc":"2.0"}"#
        );
    }
}

//! Helpers for optimism specific RPC implementations.

use crate::{SequencerClientError, SequencerMetrics};
use alloy_json_rpc::{RpcRecv, RpcSend};
use alloy_primitives::{hex, B256};
use alloy_rpc_client::{BuiltInConnectionString, ClientBuilder, RpcClient as Client};
use alloy_rpc_types_eth::erc4337::TransactionConditional;
use alloy_transport::TransportErrorKind;
use alloy_transport_http::Http;
use futures::future::join_all;
use std::{
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    time::sleep,
};
use tracing::warn;

/// Maximum number of pending forwards coalesced into a single outgoing batch RPC.
const FORWARD_BATCH_MAX_SIZE: usize = 100;

/// Maximum time to wait for additional forwards before flushing the current batch.
///
/// Trades a small per-tx latency penalty for amortizing HTTP round-trips when many
/// concurrent senders feed the RPC node. Picked to be well below typical L2 block time.
const FORWARD_BATCH_MAX_WAIT: Duration = Duration::from_millis(2);

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
    ///
    /// Spawns a background worker that coalesces concurrent `eth_sendRawTransaction`
    /// forwards into JSON-RPC batches sent to the sequencer endpoint.
    pub(crate) fn new(sequencer_endpoint: String, client: Client) -> Self {
        let metrics = SequencerMetrics::default();
        let (batch_tx, batch_rx) = mpsc::unbounded_channel();
        tokio::spawn(forward_batch_worker(client.clone(), batch_rx, metrics.clone()));
        Self { sequencer_endpoint, client, metrics, batch_tx }
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
    ///
    /// Concurrent calls are coalesced by the background worker into a single
    /// JSON-RPC batch (one HTTP POST) sent to the sequencer. If the worker has
    /// died, falls back to a direct per-tx request.
    pub async fn forward_raw_transaction(&self, tx: &[u8]) -> Result<B256, SequencerClientError> {
        let tx_hex = hex::encode_prefixed(tx);
        let (reply_tx, reply_rx) = oneshot::channel();
        let job = BatchJob { tx_hex, reply: reply_tx };

        if self.inner.batch_tx.send(job).is_err() {
            warn!(
                target: "rpc::eth",
                "sequencer batch forwarder worker terminated, falling back to direct request",
            );
            return self.forward_raw_transaction_direct(tx).await;
        }

        reply_rx.await.unwrap_or_else(|_| {
            Err(SequencerClientError::HttpError(TransportErrorKind::custom_str(
                "sequencer batch forwarder dropped the response channel",
            )))
        })
    }

    /// Direct (non-batched) forward, used as a fallback when the batch worker is gone.
    async fn forward_raw_transaction_direct(
        &self,
        tx: &[u8],
    ) -> Result<B256, SequencerClientError> {
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
    /// Channel to the background batch-coalescing worker. Dropping all senders
    /// (via dropping every `SequencerClient` clone) stops the worker.
    batch_tx: mpsc::UnboundedSender<BatchJob>,
}

/// A single pending `eth_sendRawTransaction` forward, awaiting batch dispatch.
struct BatchJob {
    tx_hex: String,
    reply: oneshot::Sender<Result<B256, SequencerClientError>>,
}

/// Background loop that drains pending forwards and ships them as JSON-RPC batches.
async fn forward_batch_worker(
    client: Client,
    mut rx: mpsc::UnboundedReceiver<BatchJob>,
    metrics: SequencerMetrics,
) {
    while let Some(first) = rx.recv().await {
        let start = Instant::now();
        let mut jobs: Vec<BatchJob> = Vec::with_capacity(FORWARD_BATCH_MAX_SIZE);
        jobs.push(first);

        // Drain additional pending jobs without yielding, up to the size cap.
        while jobs.len() < FORWARD_BATCH_MAX_SIZE {
            match rx.try_recv() {
                Ok(job) => jobs.push(job),
                Err(_) => break,
            }
        }

        // If we still have headroom and nothing else is queued, hold briefly to let
        // additional concurrent forwards accumulate before flushing.
        if jobs.len() < FORWARD_BATCH_MAX_SIZE {
            let deadline = sleep(FORWARD_BATCH_MAX_WAIT);
            tokio::pin!(deadline);
            loop {
                if jobs.len() >= FORWARD_BATCH_MAX_SIZE {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = &mut deadline => break,
                    next = rx.recv() => match next {
                        Some(job) => jobs.push(job),
                        None => break,
                    }
                }
            }
        }

        execute_batch(&client, jobs, &metrics, start).await;
    }
}

/// Build a JSON-RPC batch from the collected jobs, dispatch it, and reply to each caller.
async fn execute_batch(
    client: &Client,
    jobs: Vec<BatchJob>,
    metrics: &SequencerMetrics,
    start: Instant,
) {
    let mut batch = client.new_batch();
    let mut waiters: Vec<Option<_>> = Vec::with_capacity(jobs.len());

    for job in &jobs {
        let params = (job.tx_hex.as_str(),);
        match batch.add_call::<_, B256>("eth_sendRawTransaction", &params) {
            Ok(waiter) => waiters.push(Some(waiter)),
            Err(_) => waiters.push(None),
        }
    }

    // Drive the batch send and the per-entry waiters concurrently. The send future
    // populates the oneshot channels the waiters resolve on; if send itself fails,
    // the channels are dropped and the waiters resolve with a transport error.
    let send_fut = batch.send();
    let waiter_futs = waiters.into_iter().map(|w| async move {
        match w {
            Some(waiter) => Some(waiter.await),
            None => None,
        }
    });
    let (_send_res, results) = tokio::join!(send_fut, join_all(waiter_futs));

    let elapsed = start.elapsed();

    for (job, waiter_res) in jobs.into_iter().zip(results) {
        let reply: Result<B256, SequencerClientError> = match waiter_res {
            Some(Ok(hash)) => Ok(hash),
            Some(Err(err)) => Err(SequencerClientError::HttpError(err)),
            None => Err(SequencerClientError::HttpError(TransportErrorKind::custom_str(
                "failed to serialize batched eth_sendRawTransaction",
            ))),
        };
        if let Err(ref err) = reply {
            warn!(target: "rpc::eth", %err, "Failed to forward batched transaction to sequencer");
        }
        let _ = job.reply.send(reply);
        metrics.record_forward_latency(elapsed);
    }
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

    /// Spins up a tiny mock HTTP server that echoes back a synthetic
    /// `eth_sendRawTransaction` result for each entry in the request and
    /// counts how many distinct HTTP POSTs (and the max batch size in any
    /// one POST) it observes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn batches_concurrent_forwards_into_single_http_request() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let post_count = Arc::new(AtomicUsize::new(0));
        let max_batch_seen = Arc::new(AtomicUsize::new(0));

        let server_post_count = post_count.clone();
        let server_max_batch = max_batch_seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let pc = server_post_count.clone();
                let mb = server_max_batch.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(8192);
                    let mut tmp = [0u8; 4096];
                    let body = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        let Some(hdr_end) =
                            buf.windows(4).position(|w| w == b"\r\n\r\n")
                        else {
                            continue;
                        };
                        let headers = std::str::from_utf8(&buf[..hdr_end]).unwrap_or("");
                        let content_len: usize = headers
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        let body_start = hdr_end + 4;
                        if buf.len() >= body_start + content_len {
                            break buf[body_start..body_start + content_len].to_vec();
                        }
                    };

                    pc.fetch_add(1, Ordering::SeqCst);
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let zero_hash = format!("0x{}", "00".repeat(32));
                    let resp_body = if let Some(arr) = v.as_array() {
                        mb.fetch_max(arr.len(), Ordering::SeqCst);
                        let resp: Vec<serde_json::Value> = arr
                            .iter()
                            .map(|req| {
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": req["id"],
                                    "result": zero_hash,
                                })
                            })
                            .collect();
                        serde_json::to_string(&resp).unwrap()
                    } else {
                        mb.fetch_max(1, Ordering::SeqCst);
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": v["id"],
                            "result": zero_hash,
                        })
                        .to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp_body.len(),
                        resp_body
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        let client = SequencerClient::new(&url).await.unwrap();

        let n = 32usize;
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let c = client.clone();
            let tx = vec![i as u8; 64];
            handles.push(tokio::spawn(async move {
                c.forward_raw_transaction(&tx).await
            }));
        }
        for h in handles {
            h.await.unwrap().expect("forward should succeed");
        }

        let posts = post_count.load(Ordering::SeqCst);
        let max_batch = max_batch_seen.load(Ordering::SeqCst);
        assert!(
            posts < n,
            "expected fewer than {n} HTTP POSTs once batching kicks in, observed {posts}",
        );
        assert!(
            max_batch > 1,
            "expected at least one POST carrying multiple batched entries, observed max {max_batch}",
        );
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

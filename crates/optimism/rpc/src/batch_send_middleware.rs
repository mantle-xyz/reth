//! jsonrpsee middleware that rewrites a homogenous JSON-RPC batch of
//! `eth_sendRawTransaction` calls into a single `eth_sendRawTransactions`
//! invocation, then splits the per-item result back into individual batch
//! response entries.
//!
//! Inbound batches that mix methods (or that contain notifications, errors,
//! malformed params, fewer than two entries, etc.) pass through to the inner
//! service unchanged.

use alloy_primitives::Bytes;
use futures::FutureExt;
use jsonrpsee::{
    core::middleware::{Batch, BatchEntry, Notification},
    server::middleware::rpc::RpcServiceT,
    types::{Id, Request},
    MethodResponse,
};
use jsonrpsee_core::server::{BatchResponseBuilder, ResponsePayload};
use reth_rpc_eth_api::SendRawTxBatchItem;
use serde_json::value::RawValue;
use std::future::Future;
use tower::Layer;

const TARGET_METHOD: &str = "eth_sendRawTransaction";
const REWRITE_METHOD: &str = "eth_sendRawTransactions";

/// Layer that wraps an inner [`RpcServiceT`] with batch-rewriting behavior.
#[derive(Clone, Debug, Default)]
pub struct BatchSendRawTxLayer {
    /// Cap on the per-response body when synthesizing a batch response.
    /// Mirrors jsonrpsee's default of 10 MiB; bumped via [`Self::with_max_response_size`].
    max_response_size: usize,
}

impl BatchSendRawTxLayer {
    /// Creates a new layer with the default 10 MiB response cap.
    pub const fn new() -> Self {
        Self { max_response_size: 10 * 1024 * 1024 }
    }

    /// Overrides the cap used when synthesizing batch responses.
    pub const fn with_max_response_size(mut self, max: usize) -> Self {
        self.max_response_size = max;
        self
    }
}

impl<S> Layer<S> for BatchSendRawTxLayer {
    type Service = BatchSendRawTxService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BatchSendRawTxService { inner, max_response_size: self.max_response_size }
    }
}

/// Service that rewrites batches of `eth_sendRawTransaction` into a single
/// `eth_sendRawTransactions` call. See module docs for behavior.
#[derive(Clone, Debug)]
pub struct BatchSendRawTxService<S> {
    inner: S,
    max_response_size: usize,
}

impl<S> RpcServiceT for BatchSendRawTxService<S>
where
    S: RpcServiceT<
            MethodResponse = MethodResponse,
            BatchResponse = MethodResponse,
            NotificationResponse = MethodResponse,
        > + Send
        + Sync
        + Clone
        + 'static,
{
    type MethodResponse = MethodResponse;
    type BatchResponse = MethodResponse;
    type NotificationResponse = MethodResponse;

    fn call<'a>(&self, req: Request<'a>) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        self.inner.call(req)
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let inner = self.inner.clone();
        let max_response_size = self.max_response_size;
        async move {
            // Inspect the batch first; if it's not a homogenous bundle of
            // single-arg eth_sendRawTransaction calls, pass through.
            let Some(parsed) = parse_homogenous_batch(&batch) else {
                return inner.batch(batch).await;
            };
            let HomogenousBatch { ids, txs } = parsed;
            debug_assert_eq!(ids.len(), txs.len());

            // Build a synthetic single-call request for eth_sendRawTransactions.
            let synthetic = match build_synthetic_request(&txs) {
                Ok(req) => req,
                Err(_) => {
                    // Should not happen for valid Bytes; fall back to pass-through.
                    return inner.batch(batch).await;
                }
            };

            let synthetic_resp = inner.call(synthetic).await;
            split_batch_response(synthetic_resp, ids, max_response_size)
        }
        .boxed()
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.inner.notification(n)
    }
}

struct HomogenousBatch {
    ids: Vec<Id<'static>>,
    txs: Vec<Bytes>,
}

/// Returns `Some` only if every entry in the batch is a successful `Call` with
/// `method == eth_sendRawTransaction` and a single hex-string param. Returns
/// `None` (pass-through) for batches under two entries, mixed methods, errors,
/// or malformed params.
fn parse_homogenous_batch(batch: &Batch<'_>) -> Option<HomogenousBatch> {
    if batch.len() < 2 {
        return None;
    }
    let mut ids = Vec::with_capacity(batch.len());
    let mut txs = Vec::with_capacity(batch.len());
    for entry in batch.iter() {
        let req = match entry {
            Ok(BatchEntry::Call(req)) if req.method_name() == TARGET_METHOD => req,
            _ => return None,
        };
        let params_raw = req.params.as_ref()?;
        // Accept either positional `["0x..."]` or `"0x..."` (some clients).
        let tx = parse_single_bytes_param(params_raw.get())?;
        ids.push(req.id.clone().into_owned());
        txs.push(tx);
    }
    Some(HomogenousBatch { ids, txs })
}

fn parse_single_bytes_param(raw: &str) -> Option<Bytes> {
    if let Ok((bytes,)) = serde_json::from_str::<(Bytes,)>(raw) {
        return Some(bytes);
    }
    serde_json::from_str::<Bytes>(raw).ok()
}

fn build_synthetic_request(txs: &[Bytes]) -> serde_json::Result<Request<'static>> {
    // Serialize as positional [Vec<Bytes>] to match the trait signature
    // `send_raw_transactions(txs: Vec<Bytes>)`.
    let params_value = serde_json::to_value((txs,))?;
    let params_raw = RawValue::from_string(params_value.to_string())?;
    Ok(Request::owned(REWRITE_METHOD.to_string(), Some(params_raw), Id::Number(0)))
}

/// Parses the synthetic `eth_sendRawTransactions` response and rebuilds a
/// batch response carrying one entry per original request id. If the synthetic
/// call itself errored (e.g. method missing), the same error is fanned out to
/// every original id.
fn split_batch_response(
    synthetic: MethodResponse,
    ids: Vec<Id<'static>>,
    max_response_size: usize,
) -> MethodResponse {
    // The MethodResponse json is a full JSON-RPC response envelope. Parse it
    // structurally with serde_json::Value to side-step the multiple
    // ResponsePayload types floating around in jsonrpsee.
    let mut builder = BatchResponseBuilder::new_with_limit(max_response_size);
    let envelope: serde_json::Value = match serde_json::from_str(synthetic.as_ref()) {
        Ok(v) => v,
        Err(_) => {
            fan_out_error(&mut builder, ids, "failed to parse sendRawTransactions response");
            return MethodResponse::from_batch(builder.finish());
        }
    };

    if let Some(err) = envelope.get("error") {
        let err_owned: jsonrpsee_types::ErrorObjectOwned = match serde_json::from_value(err.clone())
        {
            Ok(e) => e,
            Err(_) => jsonrpsee_types::ErrorObjectOwned::owned::<()>(
                jsonrpsee_types::error::INTERNAL_ERROR_CODE,
                "sendRawTransactions error",
                None,
            ),
        };
        for id in ids {
            if builder.append(MethodResponse::error(id, err_owned.clone())).is_err() {
                break;
            }
        }
        return MethodResponse::from_batch(builder.finish());
    }

    let result = envelope.get("result");
    let items: Option<Vec<SendRawTxBatchItem>> =
        result.and_then(|r| serde_json::from_value(r.clone()).ok());

    let items = match items {
        Some(items) if items.len() == ids.len() => items,
        _ => {
            fan_out_error(&mut builder, ids, "malformed sendRawTransactions response payload");
            return MethodResponse::from_batch(builder.finish());
        }
    };

    for (id, item) in ids.into_iter().zip(items) {
        let mr = match (item.hash, item.error) {
            (Some(hash), _) => {
                MethodResponse::response(id, ResponsePayload::success(hash), max_response_size)
            }
            (None, Some(err)) => MethodResponse::error(id, err),
            (None, None) => MethodResponse::error(
                id,
                jsonrpsee_types::ErrorObjectOwned::owned::<()>(
                    jsonrpsee_types::error::INTERNAL_ERROR_CODE,
                    "empty sendRawTransactions item",
                    None,
                ),
            ),
        };
        if builder.append(mr).is_err() {
            break;
        }
    }

    MethodResponse::from_batch(builder.finish())
}

fn fan_out_error(builder: &mut BatchResponseBuilder, ids: Vec<Id<'static>>, msg: &str) {
    let err = jsonrpsee_types::ErrorObjectOwned::owned::<()>(
        jsonrpsee_types::error::INTERNAL_ERROR_CODE,
        msg,
        None,
    );
    for id in ids {
        if builder.append(MethodResponse::error(id, err.clone())).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(method: &str, params_json: &str, id: u64) -> Request<'static> {
        let params = RawValue::from_string(params_json.to_string()).unwrap();
        Request::owned(method.to_string(), Some(params), Id::Number(id))
    }

    #[test]
    fn parse_homogenous_batch_picks_up_well_formed_batch() {
        let mut batch = Batch::with_capacity(2);
        batch.push(make_request(TARGET_METHOD, "[\"0xdead\"]", 1));
        batch.push(make_request(TARGET_METHOD, "[\"0xbeef\"]", 2));
        let parsed = parse_homogenous_batch(&batch).expect("should detect");
        assert_eq!(parsed.ids.len(), 2);
        assert_eq!(parsed.txs.len(), 2);
        assert_eq!(parsed.txs[0].as_ref(), &[0xde, 0xad]);
        assert_eq!(parsed.txs[1].as_ref(), &[0xbe, 0xef]);
    }

    #[test]
    fn parse_homogenous_batch_passes_through_single_entry() {
        let mut batch = Batch::with_capacity(1);
        batch.push(make_request(TARGET_METHOD, "[\"0xdead\"]", 1));
        assert!(parse_homogenous_batch(&batch).is_none());
    }

    #[test]
    fn parse_homogenous_batch_passes_through_mixed_methods() {
        let mut batch = Batch::with_capacity(2);
        batch.push(make_request(TARGET_METHOD, "[\"0xdead\"]", 1));
        batch.push(make_request("eth_blockNumber", "[]", 2));
        assert!(parse_homogenous_batch(&batch).is_none());
    }

    #[test]
    fn parse_homogenous_batch_passes_through_malformed_params() {
        let mut batch = Batch::with_capacity(2);
        batch.push(make_request(TARGET_METHOD, "[\"0xdead\"]", 1));
        batch.push(make_request(TARGET_METHOD, "[true]", 2));
        assert!(parse_homogenous_batch(&batch).is_none());
    }

    #[test]
    fn build_synthetic_request_uses_positional_array_params() {
        let txs = vec![Bytes::from(vec![0xaa, 0xbb]), Bytes::from(vec![0xcc])];
        let req = build_synthetic_request(&txs).unwrap();
        assert_eq!(req.method_name(), REWRITE_METHOD);
        let raw = req.params.as_ref().unwrap().get();
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        let arr = value.as_array().expect("positional params");
        assert_eq!(arr.len(), 1);
        let inner = arr[0].as_array().expect("vec of bytes");
        assert_eq!(inner.len(), 2);
        assert_eq!(inner[0], serde_json::json!("0xaabb"));
        assert_eq!(inner[1], serde_json::json!("0xcc"));
    }

    // ----- split_batch_response tests -----

    use alloy_primitives::B256;

    fn synthetic_success(items: Vec<SendRawTxBatchItem>) -> MethodResponse {
        MethodResponse::response(Id::Number(0), ResponsePayload::success(items), 10 * 1024 * 1024)
    }

    fn synthetic_error(err: jsonrpsee_types::ErrorObjectOwned) -> MethodResponse {
        MethodResponse::error(Id::Number(0), err)
    }

    /// Parse the batch response envelope back to per-entry (id, result|error).
    /// Returns Vec<(id, Ok(hash) | Err((code, msg)))>.
    fn parse_batch_entries(
        resp: &MethodResponse,
    ) -> Vec<(serde_json::Value, Result<String, (i64, String)>)> {
        let parsed: serde_json::Value = serde_json::from_str(resp.as_ref()).unwrap();
        let arr = parsed.as_array().expect("batch response should be an array");
        arr.iter()
            .map(|entry| {
                let id = entry.get("id").cloned().unwrap_or(serde_json::Value::Null);
                if let Some(result) = entry.get("result") {
                    (id, Ok(result.as_str().unwrap().to_string()))
                } else if let Some(err) = entry.get("error") {
                    let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                    let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
                    (id, Err((code, msg)))
                } else {
                    panic!("entry has neither result nor error: {entry}")
                }
            })
            .collect()
    }

    #[test]
    fn split_batch_response_preserves_id_order_and_mixed_outcomes() {
        let hash_a = B256::from_slice(&[0xaa; 32]);
        let hash_c = B256::from_slice(&[0xcc; 32]);
        let items = vec![
            SendRawTxBatchItem::ok(hash_a),
            SendRawTxBatchItem::err(jsonrpsee_types::ErrorObjectOwned::owned::<()>(
                -32000,
                "already known",
                None,
            )),
            SendRawTxBatchItem::ok(hash_c),
        ];
        let synthetic = synthetic_success(items);

        // Use string/number/mixed ids to confirm fidelity.
        let ids = vec![Id::Number(7), Id::Str("alpha".into()), Id::Number(42)];
        let out = split_batch_response(synthetic, ids, 10 * 1024 * 1024);

        let entries = parse_batch_entries(&out);
        assert_eq!(entries.len(), 3);

        // First entry: id=7, result=hash_a
        assert_eq!(entries[0].0, serde_json::json!(7));
        assert_eq!(entries[0].1.as_ref().unwrap(), &format!("{hash_a}"));

        // Second entry: id="alpha", error (code/message preserved)
        assert_eq!(entries[1].0, serde_json::json!("alpha"));
        let (code, msg) = entries[1].1.as_ref().err().unwrap();
        assert_eq!(*code, -32000);
        assert_eq!(msg, "already known");

        // Third entry: id=42, result=hash_c
        assert_eq!(entries[2].0, serde_json::json!(42));
        assert_eq!(entries[2].1.as_ref().unwrap(), &format!("{hash_c}"));
    }

    #[test]
    fn split_batch_response_fans_out_inner_call_error_to_each_id() {
        // Synthetic call itself failed (e.g. method not found): every original
        // id should receive a copy of the error.
        let synthetic = synthetic_error(jsonrpsee_types::ErrorObjectOwned::owned::<()>(
            -32601,
            "Method not found",
            None,
        ));

        let ids = vec![Id::Number(1), Id::Number(2), Id::Number(3)];
        let out = split_batch_response(synthetic, ids, 10 * 1024 * 1024);
        let entries = parse_batch_entries(&out);
        assert_eq!(entries.len(), 3);
        for (i, (id, res)) in entries.iter().enumerate() {
            assert_eq!(*id, serde_json::json!(i as i64 + 1));
            let (code, msg) = res.as_ref().err().unwrap();
            assert_eq!(*code, -32601);
            assert_eq!(msg, "Method not found");
        }
    }

    #[test]
    fn split_batch_response_fans_out_when_result_length_mismatches_ids() {
        // Defensive: handler somehow returns fewer items than ids. Should not
        // panic; all ids get a uniform error.
        let items = vec![SendRawTxBatchItem::ok(B256::ZERO)]; // only 1 item
        let synthetic = synthetic_success(items);
        let ids = vec![Id::Number(1), Id::Number(2)]; // expected 2

        let out = split_batch_response(synthetic, ids, 10 * 1024 * 1024);
        let entries = parse_batch_entries(&out);
        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert!(entry.1.is_err(), "mismatched item count should fan out errors");
        }
    }
}

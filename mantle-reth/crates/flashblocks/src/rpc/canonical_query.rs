//! Detects canonical replacements while an RPC query reads multiple sources.

use alloy_primitives::B256;
use jsonrpsee_types::ErrorObjectOwned;
use reth_provider::ProviderResult;
use std::future::Future;

/// The query's canonical dependency was replaced or removed.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("Canonical chain reorganization detected during the query; please retry the request.")]
pub struct CanonicalChainReorgDetected;

impl From<CanonicalChainReorgDetected> for ErrorObjectOwned {
    fn from(error: CanonicalChainReorgDetected) -> Self {
        Self::owned(
            -32000,
            error.to_string(),
            Some(serde_json::json!({
                "kind": "CanonicalChainReorgDetected"
            })),
        )
    }
}

/// A canonical block whose identity must survive a multi-source query.
#[derive(Debug, Clone, Copy)]
pub struct CanonicalQueryAnchor {
    number: u64,
    hash: B256,
}

impl CanonicalQueryAnchor {
    /// Captures the canonical block used by the query.
    pub const fn new(number: u64, hash: B256) -> Self {
        Self { number, hash }
    }

    /// Checks this block before and after the query, without retrying it.
    pub async fn run<T>(
        self,
        mut canonical_hash: impl FnMut(u64) -> ProviderResult<Option<B256>>,
        query: impl Future<Output = Result<T, ErrorObjectOwned>>,
    ) -> Result<T, ErrorObjectOwned> {
        self.ensure_canonical(&mut canonical_hash)?;
        let result = query.await;
        self.ensure_canonical(canonical_hash)?;
        result
    }

    /// Verifies the original block, allowing the chain to grow above it.
    pub fn ensure_canonical(
        self,
        mut canonical_hash: impl FnMut(u64) -> ProviderResult<Option<B256>>,
    ) -> Result<(), ErrorObjectOwned> {
        let hash = canonical_hash(self.number).map_err(|error| {
            ErrorObjectOwned::owned(
                -32000,
                format!("failed to verify the query's canonical block: {error}"),
                None::<()>,
            )
        })?;
        if hash != Some(self.hash) {
            return Err(CanonicalChainReorgDetected.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap};

    #[tokio::test]
    async fn replacing_the_anchor_during_a_query_returns_a_reorg_error() {
        let original = B256::repeat_byte(1);
        let chain = RefCell::new(BTreeMap::from([(100, original)]));
        let result = CanonicalQueryAnchor::new(100, original)
            .run(|number| Ok(chain.borrow().get(&number).copied()), async {
                chain.borrow_mut().insert(100, B256::repeat_byte(2));
                Ok(42)
            })
            .await;
        let error = result.expect_err("mixed-branch results must not be returned");
        assert_eq!(
            error.message(),
            "Canonical chain reorganization detected during the query; please retry the request."
        );
        assert_eq!(error.data().unwrap().get(), r#"{"kind":"CanonicalChainReorgDetected"}"#);
    }

    #[tokio::test]
    async fn normal_chain_growth_keeps_the_query_result() {
        let original = B256::repeat_byte(1);
        let chain = RefCell::new(BTreeMap::from([(100, original)]));
        let result = CanonicalQueryAnchor::new(100, original)
            .run(|number| Ok(chain.borrow().get(&number).copied()), async {
                chain.borrow_mut().insert(101, B256::repeat_byte(2));
                Ok(42)
            })
            .await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn removing_the_anchor_during_a_query_returns_a_reorg_error() {
        let original = B256::repeat_byte(1);
        let hash = RefCell::new(Some(original));
        let result = CanonicalQueryAnchor::new(100, original)
            .run(|_| Ok(*hash.borrow()), async {
                *hash.borrow_mut() = None;
                Ok(42)
            })
            .await;
        assert!(result.is_err());
    }
    #[tokio::test]
    async fn a_stable_anchor_preserves_the_original_query_error() {
        let hash = B256::repeat_byte(1);
        let error = CanonicalQueryAnchor::new(100, hash)
            .run(|_| Ok(Some(hash)), async {
                Err::<(), _>(ErrorObjectOwned::owned(-32602, "invalid filter", None::<()>))
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), -32602);
        assert_eq!(error.message(), "invalid filter");
    }

    #[tokio::test]
    async fn a_reorg_also_invalidates_a_failed_query() {
        let original = B256::repeat_byte(1);
        let hash = RefCell::new(Some(original));
        let error = CanonicalQueryAnchor::new(100, original)
            .run(|_| Ok(*hash.borrow()), async {
                *hash.borrow_mut() = None;
                Err::<(), _>(ErrorObjectOwned::owned(-32000, "state unavailable", None::<()>))
            })
            .await
            .unwrap_err();
        assert_eq!(error.message(), CanonicalChainReorgDetected.to_string());
    }

    #[tokio::test]
    async fn an_already_replaced_anchor_does_not_execute_the_query() {
        let executed = std::cell::Cell::new(false);
        let result = CanonicalQueryAnchor::new(100, B256::repeat_byte(1))
            .run(|_| Ok(Some(B256::repeat_byte(2))), async {
                executed.set(true);
                Ok(42)
            })
            .await;
        assert!(result.is_err());
        assert!(!executed.get());
    }
    #[tokio::test]
    async fn a_provider_failure_is_not_reported_as_a_reorg() {
        let hash = B256::repeat_byte(1);
        let error = CanonicalQueryAnchor::new(100, hash)
            .run(|_| Err(reth_provider::ProviderError::BlockHashNotFound(hash)), async { Ok(42) })
            .await
            .unwrap_err();
        assert!(error.message().starts_with("failed to verify the query's canonical block:"));
        assert!(error.data().is_none());
    }
}

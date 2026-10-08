//! End-to-end regression guard for the Mantle `eth_getProof` override.
//!
//! The override mirrors upstream `EthState::get_proof` step-for-step and diverges only in
//! the conversion: geth reports zero `codeHash`/`storageHash` for accounts that do not
//! exist (go-ethereum#28357), not `KECCAK_EMPTY`/`EMPTY_ROOT_HASH`. This drives the real
//! RPC handler — override wired, registered, and served — so a later refactor of the RPC
//! wiring cannot pass silently, and covers the `keys != []` storage-proof branch plus the
//! omitted-block-id (→ latest) path. Cross-client value comparison stays in the external
//! `rpc_compat` suite (`eth_getProof_nonexistent_contract`).

use crate::helpers::with_mantle_rpc_client;
use alloy_primitives::{Address, B256, U256, address, b256};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use jsonrpsee::core::client::ClientT;
/// An address with no genesis allocation.
const NONEXISTENT: Address = address!("1234567890123456789012345678901234567890");
/// One storage key — exercises the `keys != []` storage-proof branch.
const STORAGE_KEY: B256 = b256!("0000000000000000000000000000000000000000000000000000000000000001");

/// `eth_getProof` on a nonexistent account (one storage key, block id omitted) returns the
/// geth shape: zeroed hashes and empty storage proofs. `accountProof` for a missing
/// account is the trie's boundary witness — a non-empty node path (the genesis trie has
/// funded accounts), identical to geth's `Prove` output for the same address; it collapses
/// to `[]` only when the trie itself is empty.
#[tokio::test]
async fn get_proof_nonexistent_account_zeroed_geth_shape_via_rpc() {
    with_mantle_rpc_client(|client| async move {
        let proof: EIP1186AccountProofResponse = client
            .request(
                "eth_getProof",
                vec![serde_json::json!(NONEXISTENT), serde_json::json!([STORAGE_KEY])],
            )
            .await
            .expect("eth_getProof should succeed for a nonexistent account");

        assert_eq!(proof.address, NONEXISTENT);
        assert_eq!(proof.balance, U256::ZERO);
        assert_eq!(proof.nonce, 0);
        assert_eq!(proof.code_hash, B256::ZERO, "codeHash must be zeroed (geth parity)");
        assert_eq!(proof.storage_hash, B256::ZERO, "storageHash must be zeroed (geth parity)");
        assert!(
            !proof.account_proof.is_empty(),
            "missing account in a non-empty trie must yield a boundary witness, not []"
        );
        assert_eq!(proof.storage_proof.len(), 1, "one entry per requested key");
        assert_eq!(proof.storage_proof[0].key.as_b256(), STORAGE_KEY);
        assert_eq!(proof.storage_proof[0].value, U256::ZERO);
        assert!(
            proof.storage_proof[0].proof.is_empty(),
            "storage proof for a missing account must be empty"
        );
    })
    .await;
}

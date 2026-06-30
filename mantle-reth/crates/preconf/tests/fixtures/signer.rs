//! Deterministic test signers shared across fork-builder tests.
//!
//! Three pre-funded addresses (`Addr1` / `Addr2` / `Addr3`) backed by
//! fixed private keys. Keys match op-geth's `tests/preconf/config/config.go`
//! verbatim so behavioural comparison stays meaningful when the same
//! scenarios get re-run in a devnet later.

use alloy_primitives::{Address, B256};
use alloy_signer_local::PrivateKeySigner;

/// Funder key — `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` (Anvil's
/// account #0). Used to pre-fund the other test addresses in the
/// initial state.
pub const FUNDER_PRIVATE_KEY: B256 = B256::new([
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
]);

/// `Addr1` — preconf-eligible sender. `0x6F18bEEF53452dC646C5221900F1EfE8b6B4BDc5`.
pub const ADDR1_PRIVATE_KEY: B256 = B256::new([
    0xe4, 0x74, 0xbf, 0xa0, 0xd1, 0x52, 0x0c, 0xf4, 0xb1, 0x61, 0xb3, 0x82, 0xdb, 0x9f, 0x52, 0x7c,
    0x39, 0xac, 0x16, 0xb6, 0xd9, 0xa8, 0x35, 0x1f, 0x09, 0x1b, 0xd4, 0x06, 0xf7, 0x39, 0xa6, 0x91,
]);

/// `Addr3` — second preconf-eligible sender (for compound-state and
/// front-running scenarios). `0x918a3880A91308279C06A89415d01ae47d64eC29`.
pub const ADDR3_PRIVATE_KEY: B256 = B256::new([
    0x65, 0x4c, 0x6b, 0x97, 0xf4, 0x00, 0xc2, 0xfa, 0xce, 0xc2, 0x8b, 0xcb, 0x2a, 0xe0, 0x4f, 0x2b,
    0xf9, 0x9e, 0x00, 0x7b, 0xd6, 0xe4, 0x1b, 0x2c, 0xe2, 0x21, 0x48, 0x1e, 0x30, 0x84, 0x0e, 0x49,
]);

/// `Addr2` — recipient-only (no priv key). `0x71920E3cb420fbD8Ba9a495E6f801c50375ea127`.
pub const ADDR2: Address = Address::new([
    0x71, 0x92, 0x0e, 0x3c, 0xb4, 0x20, 0xfb, 0xd8, 0xba, 0x9a, 0x49, 0x5e, 0x6f, 0x80, 0x1c, 0x50,
    0x37, 0x5e, 0xa1, 0x27,
]);

/// Bundle of test signers a test typically needs.
pub struct TestSigners {
    /// Funder — pre-loaded with a huge balance in test state.
    pub funder: PrivateKeySigner,
    /// First preconf-eligible sender.
    pub addr1: PrivateKeySigner,
    /// Second preconf-eligible sender.
    pub addr3: PrivateKeySigner,
}

impl TestSigners {
    /// Construct the deterministic bundle. Same key material every call.
    pub fn new() -> Self {
        Self {
            funder: PrivateKeySigner::from_bytes(&FUNDER_PRIVATE_KEY)
                .expect("FUNDER_PRIVATE_KEY is a valid secp256k1 scalar"),
            addr1: PrivateKeySigner::from_bytes(&ADDR1_PRIVATE_KEY)
                .expect("ADDR1_PRIVATE_KEY is a valid secp256k1 scalar"),
            addr3: PrivateKeySigner::from_bytes(&ADDR3_PRIVATE_KEY)
                .expect("ADDR3_PRIVATE_KEY is a valid secp256k1 scalar"),
        }
    }
}

impl Default for TestSigners {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience accessor matching the field name pattern used by
/// op-geth `config.go`.
pub fn test_signers() -> TestSigners {
    TestSigners::new()
}

/// Sign a legacy transfer transaction (`from → to`, ETH value, no
/// data). Returns an `alloy_consensus::TxEnvelope` wrapped in `Arc`,
/// matching the storage shape inside [`crate::PreconfTxSet`].
///
/// Defaults:
/// - `chain_id`: OP mainnet (10) — matches `test_chain_spec`
/// - `gas_price`: 1 gwei (well above the 1 gwei `base_fee_per_gas` the
///   parent header carries)
/// - `gas_limit`: `21_000` (intrinsic + 0 data = standard transfer)
///
/// The recovered signer of the returned envelope equals `from.address()`,
/// matching what `SignedTransaction::try_into_recovered` produces
/// inside `PreconfPayloadBuilder`'s apply closure.
pub fn sign_legacy_transfer(
    from: &PrivateKeySigner,
    nonce: u64,
    to: Address,
    value_wei: alloy_primitives::U256,
) -> std::sync::Arc<alloy_consensus::TxEnvelope> {
    use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
    use alloy_primitives::TxKind;
    use alloy_signer::SignerSync;

    let tx = TxLegacy {
        chain_id: Some(10),
        nonce,
        gas_price: 1_000_000_000,
        gas_limit: 21_000,
        to: TxKind::Call(to),
        value: value_wei,
        input: Default::default(),
    };
    let sig = from
        .sign_hash_sync(&tx.signature_hash())
        .expect("PrivateKeySigner can always sign");
    // `into_signed` computes the canonical tx hash from the RLP encoding —
    // matches what `Signed::hash()` would return on a fresh `new_unhashed`.
    let signed = tx.into_signed(sig);
    std::sync::Arc::new(TxEnvelope::Legacy(signed))
}

#[cfg(test)]
mod local_tests {
    use super::*;

    #[test]
    fn addresses_match_op_geth_config() {
        let s = TestSigners::new();
        // Addresses derived from the priv keys must match op-geth's
        // hardcoded constants; sanity check protects against accidental
        // key edits.
        assert_eq!(
            format!("{:?}", s.funder.address()).to_lowercase(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
        );
        assert_eq!(
            format!("{:?}", s.addr1.address()).to_lowercase(),
            "0x6f18beef53452dc646c5221900f1efe8b6b4bdc5",
        );
        assert_eq!(
            format!("{:?}", s.addr3.address()).to_lowercase(),
            "0x918a3880a91308279c06a89415d01ae47d64ec29",
        );
    }
}

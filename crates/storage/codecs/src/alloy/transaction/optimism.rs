//! Compact implementation for [`AlloyTxDeposit`]
//!
//! Supports two on-disk formats via auto-detection:
//! - V2 (2-byte bitfield): `eth_value: Option<u128>` — canonical format (v2.0.5 / v2.2.1+)
//! - V1 (3-byte bitfield): `eth_value: u128` — legacy format (v2.1.x / v2.2.0-beta)
//!
//! Detection: decode with V2, check if `input[0..4]` matches a known L1 info deposit
//! selector. If not, try V1. Result is cached in an `AtomicU8` — detection runs at most
//! once (on the first L1 info deposit tx read). Writes always use V2 (canonical).

use crate::{
    alloy::transaction::ethereum::{CompactEnvelope, Envelope, FromTxCompact, ToTxCompact},
    generate_tests,
    txtype::{
        COMPACT_EXTENDED_IDENTIFIER_FLAG, COMPACT_IDENTIFIER_EIP1559, COMPACT_IDENTIFIER_EIP2930,
        COMPACT_IDENTIFIER_LEGACY,
    },
    Compact,
};
use alloy_consensus::{
    constants::EIP7702_TX_TYPE_ID, Signed, TxEip1559, TxEip2930, TxEip7702, TxLegacy,
};
use alloy_primitives::{Address, Bytes, Sealed, Signature, TxKind, B256, U256};
use bytes::BufMut;
use op_alloy_consensus::{OpTxEnvelope, OpTxType, OpTypedTransaction, TxDeposit as AlloyTxDeposit};
use reth_codecs_derive::add_arbitrary_tests;
use std::sync::atomic::{AtomicU8, Ordering};

// =====================================================================================
//  Format auto-detection via L1 info deposit tx selectors
// =====================================================================================

/// Re-export from `op_alloy_consensus::L1_BLOCK_SELECTORS` once op-alloy v2.2.1+ is released.
/// Until then, kept in sync manually. Canonical source:
/// `op-alloy/crates/consensus/src/predeploys.rs`. See also: `reth/crates/optimism/evm/src/l1.rs`
/// which uses the same selectors.
const L1_BLOCK_SELECTORS: [[u8; 4]; 5] = [
    [0x01, 0x5d, 0x8e, 0xb9], // Bedrock   setL1BlockValues
    [0x44, 0x0a, 0x5e, 0x20], // Ecotone   setL1BlockValuesEcotone
    [0x09, 0x89, 0x99, 0xbe], // Isthmus   setL1BlockValuesIsthmus
    [0x3d, 0xb6, 0xbe, 0x2b], // Jovian    setL1BlockValuesJovian
    [0x49, 0xe7, 0x23, 0x83], // Arsia     setL1BlockValuesArsia
];

fn has_known_l1_info_selector(input: &[u8]) -> bool {
    input.len() >= 4 && L1_BLOCK_SELECTORS.iter().any(|s| input[..4] == *s)
}

const FORMAT_UNKNOWN: u8 = 0;
const FORMAT_V2: u8 = 1;
const FORMAT_V1: u8 = 2;

/// Cached per-process format detection. Set on the first successful selector-based detection.
/// Uses AtomicU8 instead of OnceLock so it can be reset in tests.
static DEPOSIT_COMPACT_FORMAT: AtomicU8 = AtomicU8::new(FORMAT_UNKNOWN);

// =====================================================================================
//  V2 struct (canonical, 2-byte bitfield) — used for ALL writes
// =====================================================================================

/// V2 Compact format for Mantle deposit transactions.
///
/// `eth_value: Option<u128>` → 1 bit → total 15 bits → 2-byte bitfield.
/// This is the canonical format. All new writes use this struct.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[cfg_attr(
    any(test, feature = "test-utils"),
    derive(arbitrary::Arbitrary, serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(feature = "test-utils", allow(unreachable_pub), visibility::make(pub))]
#[reth_codecs(crate = "crate")]
#[add_arbitrary_tests(crate, compact)]
pub(crate) struct TxDeposit {
    source_hash: B256,
    from: Address,
    to: TxKind,
    mint: Option<u128>,
    value: U256,
    gas_limit: u64,
    is_system_transaction: bool,
    eth_value: Option<u128>,
    eth_tx_value: Option<u128>,
    input: Bytes,
}

// =====================================================================================
//  V1 struct (legacy, 3-byte bitfield) — used to decode old DB data
// =====================================================================================

/// V1 Compact format: `eth_value: u128` → 5 bits → total 19 bits → 3-byte bitfield.
/// Only used by `from_compact` when auto-detection finds V1-formatted data.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[reth_codecs(crate = "crate")]
struct TxDepositV1 {
    source_hash: B256,
    from: Address,
    to: TxKind,
    mint: Option<u128>,
    value: U256,
    gas_limit: u64,
    is_system_transaction: bool,
    eth_value: u128,
    eth_tx_value: Option<u128>,
    input: Bytes,
}

// =====================================================================================
//  Decode helpers
// =====================================================================================

fn decode_v2(buf: &[u8], len: usize) -> (AlloyTxDeposit, &[u8]) {
    let (tx, remaining) = TxDeposit::from_compact(buf, len);
    let alloy_tx = AlloyTxDeposit {
        source_hash: tx.source_hash,
        from: tx.from,
        to: tx.to,
        mint: tx.mint.unwrap_or_default(),
        value: tx.value,
        gas_limit: tx.gas_limit,
        is_system_transaction: tx.is_system_transaction,
        input: tx.input,
        eth_value: tx.eth_value.unwrap_or_default(),
        eth_tx_value: tx.eth_tx_value,
    };
    (alloy_tx, remaining)
}

fn decode_v1(buf: &[u8], len: usize) -> (AlloyTxDeposit, &[u8]) {
    let (tx, remaining) = TxDepositV1::from_compact(buf, len);
    let alloy_tx = AlloyTxDeposit {
        source_hash: tx.source_hash,
        from: tx.from,
        to: tx.to,
        mint: tx.mint.unwrap_or_default(),
        value: tx.value,
        gas_limit: tx.gas_limit,
        is_system_transaction: tx.is_system_transaction,
        input: tx.input,
        eth_value: tx.eth_value,
        eth_tx_value: tx.eth_tx_value,
    };
    (alloy_tx, remaining)
}

/// Try both decoders, pick the one whose decoded `input` starts with a known selector.
///
/// # Why `catch_unwind`
///
/// Decoding with the wrong format produces a corrupted bitfield, which can pass an invalid
/// length (e.g. > 16) to `u128::from_compact`, causing a subtraction overflow panic inside
/// the compact codec. Since there is no fallible `from_compact` API, we use `catch_unwind`
/// to treat the panic as "wrong format, try the other one".
///
/// # `panic = "unwind"` requirement
///
/// `catch_unwind` only works when the panic strategy is `unwind` (the default).
/// If the binary is compiled with `panic = "abort"`, the process will terminate on the
/// first wrong-format attempt instead of falling through to the other decoder.
/// The workspace `Cargo.toml` sets `panic = "unwind"` in `[profile.release]`, so this
/// is safe for production builds. If the panic strategy ever changes to `abort`, this
/// detection logic must be replaced with a fallible decode path.
///
/// # Performance
///
/// This function runs at most once per process in production (`#[cfg(not(test))]` fast path
/// caches the result in `DEPOSIT_COMPACT_FORMAT`). `catch_unwind` overhead on the happy path
/// (no panic) is negligible — a single function pointer check.
fn detect_and_decode(buf: &[u8], len: usize) -> (AlloyTxDeposit, &[u8]) {
    // Try V2 (canonical) first — may panic if buf is actually V1 with certain field values.
    let v2_result = try_decode_v2(buf, len);
    if let Ok((ref tx_v2, _)) = v2_result {
        if has_known_l1_info_selector(&tx_v2.input) {
            DEPOSIT_COMPACT_FORMAT
                .compare_exchange(FORMAT_UNKNOWN, FORMAT_V2, Ordering::Relaxed, Ordering::Relaxed)
                .ok();
            return v2_result.unwrap();
        }
    }

    // Try V1 (legacy) — may panic if buf is actually V2 with certain field values.
    if let Ok((tx_v1, rem_v1)) = try_decode_v1(buf, len) {
        if has_known_l1_info_selector(&tx_v1.input) {
            DEPOSIT_COMPACT_FORMAT
                .compare_exchange(FORMAT_UNKNOWN, FORMAT_V1, Ordering::Relaxed, Ordering::Relaxed)
                .ok();
            return (tx_v1, rem_v1);
        }
    }

    // Neither matched a known selector (user deposit tx, random data in tests, etc.).
    // Do NOT cache — wait for a definitive selector match from the next L1 info tx.
    // Reuse the saved V2 result if available; otherwise try V1 (guarded).
    if let Ok(v2) = v2_result {
        return v2;
    }
    if let Ok(v1) = try_decode_v1(buf, len) {
        return v1;
    }
    panic!(
        "Failed to decode TxDeposit: both V1 and V2 Compact decoders panicked ({} bytes)",
        buf.len()
    );
}

/// Attempt V2 decode, catching panics from malformed bitfield lengths.
///
/// `&[u8]` is `UnwindSafe` (via `RefUnwindSafe`), so no unsafe is needed.
/// `decode_v2` is purely functional — no global state mutation before the potential panic
/// point — so catching the panic leaves no inconsistent state.
fn try_decode_v2(buf: &[u8], len: usize) -> Result<(AlloyTxDeposit, &[u8]), ()> {
    std::panic::catch_unwind(|| decode_v2(buf, len)).map_err(|_| ())
}

/// Attempt V1 decode, catching panics from malformed bitfield lengths.
/// Same safety rationale as [`try_decode_v2`].
fn try_decode_v1(buf: &[u8], len: usize) -> Result<(AlloyTxDeposit, &[u8]), ()> {
    std::panic::catch_unwind(|| decode_v1(buf, len)).map_err(|_| ())
}

// =====================================================================================
//  AlloyTxDeposit Compact impl — write V2 (canonical), read per-tx auto-detect
// =====================================================================================

impl Compact for AlloyTxDeposit {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        // Always write V2 (canonical 2-byte bitfield).
        // Over time, DB contents converge to V2 as old V1 blocks are pruned.
        let inner = TxDeposit {
            source_hash: self.source_hash,
            from: self.from,
            to: self.to,
            // 0 stored as None to save space: from_compact restores via unwrap_or_default()
            mint: match self.mint {
                0 => None,
                v => Some(v),
            },
            value: self.value,
            gas_limit: self.gas_limit,
            is_system_transaction: self.is_system_transaction,
            eth_value: match self.eth_value {
                0 => None,
                v => Some(v),
            },
            eth_tx_value: self.eth_tx_value,
            input: self.input.clone(),
        };
        inner.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        // Always run per-tx detection. This handles mixed-format DBs correctly:
        // - L1 info deposit txs: definitive selector match, updates the cache
        // - User deposit txs: uses the cache as fallback (set by the preceding L1 info tx)
        //
        // We intentionally do NOT skip detection via a cached fast path, because the DB
        // can contain both V1 (old) and V2 (new) data after an upgrade. Each block's
        // L1 info tx re-detects the correct format for that block's era.
        detect_and_decode(buf, len)
    }
}

// =====================================================================================
//  OpTxType, OpTypedTransaction, OpTxEnvelope — unchanged
// =====================================================================================

impl crate::Compact for OpTxType {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        use crate::txtype::*;

        match self {
            Self::Legacy => COMPACT_IDENTIFIER_LEGACY,
            Self::Eip2930 => COMPACT_IDENTIFIER_EIP2930,
            Self::Eip1559 => COMPACT_IDENTIFIER_EIP1559,
            Self::Eip7702 => {
                buf.put_u8(EIP7702_TX_TYPE_ID);
                COMPACT_EXTENDED_IDENTIFIER_FLAG
            }
            Self::Deposit => {
                buf.put_u8(op_alloy_consensus::DEPOSIT_TX_TYPE_ID);
                COMPACT_EXTENDED_IDENTIFIER_FLAG
            }
        }
    }

    fn from_compact(mut buf: &[u8], identifier: usize) -> (Self, &[u8]) {
        use bytes::Buf;
        (
            match identifier {
                COMPACT_IDENTIFIER_LEGACY => Self::Legacy,
                COMPACT_IDENTIFIER_EIP2930 => Self::Eip2930,
                COMPACT_IDENTIFIER_EIP1559 => Self::Eip1559,
                COMPACT_EXTENDED_IDENTIFIER_FLAG => {
                    let extended_identifier = buf.get_u8();
                    match extended_identifier {
                        EIP7702_TX_TYPE_ID => Self::Eip7702,
                        op_alloy_consensus::DEPOSIT_TX_TYPE_ID => Self::Deposit,
                        _ => panic!("Unsupported OpTxType identifier: {extended_identifier}"),
                    }
                }
                _ => panic!("Unknown identifier for TxType: {identifier}"),
            },
            buf,
        )
    }
}

impl Compact for OpTypedTransaction {
    fn to_compact<B>(&self, out: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let identifier = self.tx_type().to_compact(out);
        match self {
            Self::Legacy(tx) => tx.to_compact(out),
            Self::Eip2930(tx) => tx.to_compact(out),
            Self::Eip1559(tx) => tx.to_compact(out),
            Self::Eip7702(tx) => tx.to_compact(out),
            Self::Deposit(tx) => tx.to_compact(out),
        };
        identifier
    }

    fn from_compact(buf: &[u8], identifier: usize) -> (Self, &[u8]) {
        let (tx_type, buf) = OpTxType::from_compact(buf, identifier);
        match tx_type {
            OpTxType::Legacy => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Legacy(tx), buf)
            }
            OpTxType::Eip2930 => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Eip2930(tx), buf)
            }
            OpTxType::Eip1559 => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Eip1559(tx), buf)
            }
            OpTxType::Eip7702 => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Eip7702(tx), buf)
            }
            OpTxType::Deposit => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Deposit(tx), buf)
            }
        }
    }
}

impl ToTxCompact for OpTxEnvelope {
    fn to_tx_compact(&self, buf: &mut (impl BufMut + AsMut<[u8]>)) {
        match self {
            Self::Legacy(tx) => tx.tx().to_compact(buf),
            Self::Eip2930(tx) => tx.tx().to_compact(buf),
            Self::Eip1559(tx) => tx.tx().to_compact(buf),
            Self::Eip7702(tx) => tx.tx().to_compact(buf),
            Self::Deposit(tx) => tx.to_compact(buf),
        };
    }
}

impl FromTxCompact for OpTxEnvelope {
    type TxType = OpTxType;

    fn from_tx_compact(buf: &[u8], tx_type: OpTxType, signature: Signature) -> (Self, &[u8]) {
        match tx_type {
            OpTxType::Legacy => {
                let (tx, buf) = TxLegacy::from_compact(buf, buf.len());
                let tx = Signed::new_unhashed(tx, signature);
                (Self::Legacy(tx), buf)
            }
            OpTxType::Eip2930 => {
                let (tx, buf) = TxEip2930::from_compact(buf, buf.len());
                let tx = Signed::new_unhashed(tx, signature);
                (Self::Eip2930(tx), buf)
            }
            OpTxType::Eip1559 => {
                let (tx, buf) = TxEip1559::from_compact(buf, buf.len());
                let tx = Signed::new_unhashed(tx, signature);
                (Self::Eip1559(tx), buf)
            }
            OpTxType::Eip7702 => {
                let (tx, buf) = TxEip7702::from_compact(buf, buf.len());
                let tx = Signed::new_unhashed(tx, signature);
                (Self::Eip7702(tx), buf)
            }
            OpTxType::Deposit => {
                let (tx, buf) = op_alloy_consensus::TxDeposit::from_compact(buf, buf.len());
                let tx = Sealed::new(tx);
                (Self::Deposit(tx), buf)
            }
        }
    }
}

const DEPOSIT_SIGNATURE: Signature = Signature::new(U256::ZERO, U256::ZERO, false);

impl Envelope for OpTxEnvelope {
    fn signature(&self) -> &Signature {
        match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
            Self::Deposit(_) => &DEPOSIT_SIGNATURE,
        }
    }

    fn tx_type(&self) -> Self::TxType {
        Self::tx_type(self)
    }
}

impl Compact for OpTxEnvelope {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        CompactEnvelope::to_compact(self, buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        CompactEnvelope::from_compact(buf, len)
    }
}

generate_tests!(#[crate, compact] OpTypedTransaction, OpTypedTransactionTests);

// =====================================================================================
//  Tests
// =====================================================================================

#[cfg(test)]
mod mantle_compact_tests {
    use super::*;
    use alloy_primitives::{address, hex, Bytes, TxKind, B256, U256};
    use op_alloy_consensus::TxDeposit as AlloyTxDeposit;

    /// Reset the global format cache so tests don't interfere with each other.
    fn reset_format_cache() {
        DEPOSIT_COMPACT_FORMAT.store(FORMAT_UNKNOWN, Ordering::Relaxed);
    }

    // ==================================================================================
    //  Bitfield guard tests
    // ==================================================================================

    #[test]
    fn test_v2_bitfield_size_must_be_2_bytes() {
        assert_eq!(TxDeposit::bitflag_encoded_bytes(), 2, "V2 bitfield must be 2 bytes");
    }

    #[test]
    fn test_v1_bitfield_size_must_be_3_bytes() {
        assert_eq!(TxDepositV1::bitflag_encoded_bytes(), 3, "V1 bitfield must be 3 bytes");
    }

    // ==================================================================================
    //  Selector detection
    // ==================================================================================

    #[test]
    fn test_known_selectors() {
        assert!(has_known_l1_info_selector(&[0x01, 0x5d, 0x8e, 0xb9, 0x00]));
        assert!(has_known_l1_info_selector(&[0x49, 0xe7, 0x23, 0x83]));
        assert!(!has_known_l1_info_selector(&[0x00, 0x00, 0x00, 0x00]));
        assert!(!has_known_l1_info_selector(&[0x01, 0x5d, 0x8e]));
        assert!(!has_known_l1_info_selector(&[]));
    }

    // ==================================================================================
    //  V2 roundtrip (write V2, read V2 directly)
    // ==================================================================================

    #[test]
    fn test_v2_roundtrip_bedrock_260b() {
        let mut input_data = vec![0x01, 0x5d, 0x8e, 0xb9];
        for i in 0u8..8 {
            let mut arg = [0u8; 32];
            arg[31] = i + 1;
            input_data.extend_from_slice(&arg);
        }

        let original = AlloyTxDeposit {
            source_hash: B256::from(hex!(
                "520df4f6f1f883397e640e1f837e3d29b119241a4fb50ff483256d850562f903"
            )),
            from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            input: Bytes::from(input_data),
            eth_tx_value: None,
        };

        let mut buf = Vec::new();
        original.to_compact(&mut buf);
        let (restored, rem) = decode_v2(&buf, buf.len());
        assert!(rem.is_empty());
        assert_eq!(original, restored);
    }

    #[test]
    fn test_v2_roundtrip_nonzero_fields() {
        let original = AlloyTxDeposit {
            source_hash: B256::with_last_byte(42),
            from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: 1000,
            value: U256::from(5000),
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 200,
            input: Bytes::from_static(&[0x49, 0xe7, 0x23, 0x83, 0xaa, 0xbb]),
            eth_tx_value: Some(300),
        };

        let mut buf = Vec::new();
        original.to_compact(&mut buf);
        let (restored, rem) = decode_v2(&buf, buf.len());
        assert!(rem.is_empty());
        assert_eq!(original, restored);
    }

    #[test]
    fn test_v2_roundtrip_eth_tx_value_some_zero() {
        let original = AlloyTxDeposit {
            source_hash: B256::with_last_byte(1),
            from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            input: Bytes::from_static(&[0x01, 0x02, 0x03]),
            eth_tx_value: Some(0),
        };

        let mut buf = Vec::new();
        original.to_compact(&mut buf);
        let (restored, _) = decode_v2(&buf, buf.len());
        assert_eq!(original.eth_tx_value, restored.eth_tx_value);
        assert_eq!(original.input, restored.input);
    }

    // ==================================================================================
    //  Auto-detection: V1 encoded → detect_and_decode picks V1
    // ==================================================================================

    #[test]
    fn test_detect_v1_bedrock_selector() {
        reset_format_cache();
        let v1_tx = TxDepositV1 {
            source_hash: B256::from(hex!(
                "520df4f6f1f883397e640e1f837e3d29b119241a4fb50ff483256d850562f903"
            )),
            from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: None,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            eth_tx_value: None,
            input: Bytes::from_static(&[0x01, 0x5d, 0x8e, 0xb9, 0x00, 0x01]),
        };

        let mut v1_buf = Vec::new();
        v1_tx.to_compact(&mut v1_buf);

        let (restored, rem) = detect_and_decode(&v1_buf, v1_buf.len());
        assert!(rem.is_empty());
        assert_eq!(restored.source_hash, v1_tx.source_hash);
        assert_eq!(restored.from, v1_tx.from);
        assert_eq!(restored.gas_limit, v1_tx.gas_limit);
        assert_eq!(restored.eth_value, v1_tx.eth_value);
        assert_eq!(restored.input, v1_tx.input);
    }

    #[test]
    fn test_detect_v1_arsia_selector() {
        reset_format_cache();
        let v1_tx = TxDepositV1 {
            source_hash: B256::with_last_byte(99),
            from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: None,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            eth_tx_value: None,
            input: Bytes::from_static(&[0x49, 0xe7, 0x23, 0x83, 0x01, 0x02]),
        };

        let mut v1_buf = Vec::new();
        v1_tx.to_compact(&mut v1_buf);

        let (restored, rem) = detect_and_decode(&v1_buf, v1_buf.len());
        assert!(rem.is_empty());
        assert_eq!(restored.input, v1_tx.input);
        assert_eq!(restored.from, v1_tx.from);
    }

    #[test]
    fn test_detect_v1_nonzero_eth_value() {
        reset_format_cache();
        let v1_tx = TxDepositV1 {
            source_hash: B256::with_last_byte(42),
            from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: Some(1000),
            value: U256::from(5000),
            gas_limit: 2_000_000,
            is_system_transaction: false,
            eth_value: 200,
            eth_tx_value: Some(300),
            input: Bytes::from_static(&[0x01, 0x5d, 0x8e, 0xb9, 0xaa, 0xbb, 0xcc]),
        };

        let mut v1_buf = Vec::new();
        v1_tx.to_compact(&mut v1_buf);

        let (restored, rem) = detect_and_decode(&v1_buf, v1_buf.len());
        assert!(rem.is_empty());
        assert_eq!(restored.eth_value, 200);
        assert_eq!(restored.eth_tx_value, Some(300));
        assert_eq!(restored.mint, 1000);
        assert_eq!(restored.input, v1_tx.input);
    }

    #[test]
    fn test_detect_v1_bedrock_260b_input() {
        reset_format_cache();
        let mut input_data = vec![0x01, 0x5d, 0x8e, 0xb9];
        for i in 0u8..8 {
            let mut arg = [0u8; 32];
            arg[31] = i + 1;
            input_data.extend_from_slice(&arg);
        }
        assert_eq!(input_data.len(), 260);

        let v1_tx = TxDepositV1 {
            source_hash: B256::from(hex!(
                "f129853cf1f38fe1fbcf264f82d80e8fd4532bba9213ff0b0846890cbd2f1656"
            )),
            from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: None,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            eth_tx_value: None,
            input: Bytes::from(input_data.clone()),
        };

        let mut v1_buf = Vec::new();
        v1_tx.to_compact(&mut v1_buf);

        let (restored, rem) = detect_and_decode(&v1_buf, v1_buf.len());
        assert!(rem.is_empty());
        assert_eq!(restored.input.len(), 260, "Bedrock input must be 260 bytes");
        assert_eq!(restored.input, Bytes::from(input_data));
        assert_eq!(restored.source_hash, v1_tx.source_hash);
    }

    // ==================================================================================
    //  Auto-detection: V2 encoded → detect_and_decode picks V2
    // ==================================================================================

    #[test]
    fn test_detect_v2_bedrock_selector() {
        reset_format_cache();
        let original = AlloyTxDeposit {
            source_hash: B256::from(hex!(
                "520df4f6f1f883397e640e1f837e3d29b119241a4fb50ff483256d850562f903"
            )),
            from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            input: Bytes::from_static(&[0x01, 0x5d, 0x8e, 0xb9, 0x00, 0x01]),
            eth_tx_value: None,
        };

        let mut buf = Vec::new();
        original.to_compact(&mut buf);

        let (restored, rem) = detect_and_decode(&buf, buf.len());
        assert!(rem.is_empty());
        assert_eq!(restored, original);
    }

    // ==================================================================================
    //  Mixed-format test: both V1 and V2 data readable by detect_and_decode
    // ==================================================================================

    #[test]
    fn test_mixed_format_both_readable() {
        reset_format_cache();
        let tx = AlloyTxDeposit {
            source_hash: B256::with_last_byte(1),
            from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            input: Bytes::from_static(&[0x01, 0x5d, 0x8e, 0xb9, 0xaa, 0xbb]),
            eth_tx_value: None,
        };

        // V1 encoded
        let v1_inner = TxDepositV1 {
            source_hash: tx.source_hash,
            from: tx.from,
            to: tx.to,
            mint: None,
            value: tx.value,
            gas_limit: tx.gas_limit,
            is_system_transaction: tx.is_system_transaction,
            eth_value: tx.eth_value,
            eth_tx_value: tx.eth_tx_value,
            input: tx.input.clone(),
        };
        let mut v1_buf = Vec::new();
        v1_inner.to_compact(&mut v1_buf);

        // V2 encoded
        let mut v2_buf = Vec::new();
        tx.to_compact(&mut v2_buf);

        // Both readable via detect_and_decode
        let (from_v1, rem1) = detect_and_decode(&v1_buf, v1_buf.len());
        assert!(rem1.is_empty());
        assert_eq!(from_v1.source_hash, tx.source_hash);
        assert_eq!(from_v1.input, tx.input);

        let (from_v2, rem2) = detect_and_decode(&v2_buf, v2_buf.len());
        assert!(rem2.is_empty());
        assert_eq!(from_v2, tx);
    }

    // ==================================================================================
    //  Real mainnet tx RLP → Compact → RLP roundtrip
    // ==================================================================================

    #[test]
    fn test_real_mainnet_block_87910504() {
        use alloy_eips::eip2718::{Decodable2718, Encodable2718};

        let raw_bytes = hex!(
            "7ef9015aa0f129853cf1f38fe1fbcf264f82d80e8fd4532bba9213ff0b0846890cbd2f1656"
            "94deaddeaddeaddeaddeaddeaddeaddeaddead0001944200000000000000000000000000000000"
            "000015808083"
            "0f42408080b90104015d8eb900000000000000000000000000000000000000000000000000"
            "000000000f424000000000000000000000000000000000000000000000000000000000676f0775"
            "000000000000000000000000000000000000000000000000000000003b9aca0000000000000000"
            "0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000027100000000000000000000000003c44cdddb6a900fa2b585dd299e03d12fa4293bc00000000000000000000000000000000000000000000000000000000000000012710"
        );

        let tx = AlloyTxDeposit::decode_2718(&mut &raw_bytes[..]).unwrap();
        assert_eq!(tx.input.len(), 260);

        let mut compact_buf = Vec::new();
        tx.to_compact(&mut compact_buf);
        let (restored, _) = decode_v2(&compact_buf, compact_buf.len());
        assert_eq!(tx, restored);

        let rlp_original = tx.encoded_2718();
        let rlp_restored = restored.encoded_2718();
        assert_eq!(rlp_original, rlp_restored);
    }
}

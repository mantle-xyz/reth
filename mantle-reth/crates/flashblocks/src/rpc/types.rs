//! Subscription types for the `eth_` `PubSub` RPC extension.

use alloy_consensus::Eip658Value;
use alloy_primitives::{Address, Bloom};
use alloy_rpc_types_eth::{Log, pubsub::SubscriptionKind};
use derive_more::From;
use op_alloy_rpc_types::Transaction;
use serde::{Deserialize, Serialize};

/// A full transaction object with its associated logs and receipt-equivalent fields.
///
/// Returned by `newFlashblockTransactions` when `full = true` or a log filter is given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionWithLogs {
    /// The full transaction object.
    #[serde(flatten)]
    pub transaction: Transaction,
    /// Logs emitted by this transaction.
    pub logs: Vec<Log>,
    /// Gas consumed by this transaction's execution.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_used: u64,
    /// Status of the transaction, serialized the same way as `eth_getTransactionReceipt`.
    #[serde(flatten)]
    pub status: Eip658Value,
    /// Cumulative gas used in the block up to and including this transaction.
    #[serde(with = "alloy_serde::quantity")]
    pub cumulative_gas_used: u64,
    /// Contract address created, if this was a contract creation transaction.
    pub contract_address: Option<Address>,
    /// Bloom filter for all logs emitted by this transaction.
    pub logs_bloom: Bloom,
}

/// Subscription kind covering both standard Ethereum types and flashblocks types.
///
/// Encapsulating [`SubscriptionKind`] rather than redefining its variants inherits
/// upstream additions automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, From)]
#[serde(untagged)]
pub enum ExtendedSubscriptionKind {
    /// Standard Ethereum subscription types, proxied to reth's `EthPubSub`.
    #[from]
    Standard(SubscriptionKind),
    /// Flashblocks-specific subscription types.
    #[from]
    Flashblocks(FlashblocksSubscriptionKind),
}

/// Flashblocks-specific subscription types.
///
/// The variant names determine the wire values via `rename_all = "camelCase"` and
/// must not be changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FlashblocksSubscriptionKind {
    /// Fires each time a new flashblock is processed, carrying the pending block state.
    NewFlashblocks,
    /// Logs from pending flashblock state matching the filter.
    PendingLogs,
    /// Transactions from flashblocks as they are sequenced.
    ///
    /// Accepts `true` for full objects with logs, `false` (default) for hashes only,
    /// or a log filter object to select transactions with at least one matching log.
    NewFlashblockTransactions,
}

impl ExtendedSubscriptionKind {
    /// Returns the standard subscription kind if this is a standard subscription type.
    pub const fn as_standard(&self) -> Option<SubscriptionKind> {
        match self {
            Self::Standard(kind) => Some(*kind),
            Self::Flashblocks(_) => None,
        }
    }

    /// Returns true if this is a flashblocks-specific subscription.
    pub const fn is_flashblocks(&self) -> bool {
        matches!(self, Self::Flashblocks(_))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Signed, TxLegacy, transaction::Recovered};
    use alloy_primitives::{B256, Bytes, Log as PrimitiveLog, LogData, Signature, TxKind, U256};
    use op_alloy_consensus::OpTxEnvelope;

    use super::*;

    fn transaction_with_logs(contract_address: Option<Address>) -> TransactionWithLogs {
        let hash = B256::with_last_byte(0xAA);
        let envelope = OpTxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                chain_id: Some(5000),
                nonce: 7,
                gas_price: 1_000_000_000,
                gas_limit: 21_000,
                to: TxKind::Call(Address::with_last_byte(0xBB)),
                value: U256::from(1_000_000u64),
                input: Bytes::new(),
            },
            Signature::test_signature(),
            hash,
        ));

        TransactionWithLogs {
            transaction: Transaction {
                inner: alloy_rpc_types_eth::Transaction {
                    inner: Recovered::new_unchecked(envelope, Address::with_last_byte(0xCC)),
                    block_hash: None,
                    block_number: Some(42),
                    block_timestamp: Some(1_700_000_000),
                    transaction_index: Some(3),
                    effective_gas_price: Some(1_000_000_000),
                },
                deposit_nonce: None,
                deposit_receipt_version: None,
            },
            logs: vec![Log {
                inner: PrimitiveLog {
                    address: Address::with_last_byte(0xDD),
                    data: LogData::new_unchecked(
                        vec![B256::with_last_byte(0xEE)],
                        Bytes::from_static(&[0x01, 0x02]),
                    ),
                },
                block_hash: None,
                block_number: Some(42),
                block_timestamp: Some(1_700_000_000),
                transaction_hash: Some(hash),
                transaction_index: Some(3),
                log_index: Some(0),
                removed: false,
            }],
            gas_used: 21_000,
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 42_000,
            contract_address,
            logs_bloom: [0x11; 256].into(),
        }
    }

    /// The JSON shape is the `newFlashblockTransactions` wire contract: the
    /// transaction is flattened to the top level and carries the receipt-equivalent
    /// fields alongside it, all camelCase.
    #[test]
    fn the_transaction_is_flattened_next_to_the_receipt_fields() {
        let value =
            serde_json::to_value(transaction_with_logs(Some(Address::with_last_byte(0xEF))))
                .expect("serialisation");
        let object = value.as_object().expect("a JSON object");

        for key in ["hash", "from", "to", "nonce", "gas", "value", "input"] {
            assert!(object.contains_key(key), "the transaction must be flattened: missing `{key}`");
        }
        for key in ["logs", "gasUsed", "cumulativeGasUsed", "contractAddress", "logsBloom"] {
            assert!(object.contains_key(key), "missing `{key}`");
        }

        assert_eq!(object["gasUsed"], "0x5208", "quantity-encoded, not a JSON number");
        assert_eq!(object["cumulativeGasUsed"], "0xa410");
        assert_eq!(object["logs"].as_array().expect("logs").len(), 1);
    }

    /// `status` is flattened the same way `eth_getTransactionReceipt` renders it,
    /// so a client can read it without a second code path.
    #[test]
    fn the_status_is_flattened_as_a_receipt_renders_it() {
        let mut with_logs = transaction_with_logs(None);
        let succeeded = serde_json::to_value(&with_logs).expect("serialisation");
        assert_eq!(succeeded["status"], "0x1");

        with_logs.status = Eip658Value::Eip658(false);
        let reverted = serde_json::to_value(&with_logs).expect("serialisation");
        assert_eq!(reverted["status"], "0x0");
    }

    /// A non-creating transaction must still carry the key, as a receipt does.
    /// Omitting it would make clients treat the field as absent rather than null.
    #[test]
    fn a_contract_address_of_none_stays_present_as_null() {
        let value = serde_json::to_value(transaction_with_logs(None)).expect("serialisation");
        let object = value.as_object().expect("a JSON object");

        assert!(object.contains_key("contractAddress"), "the key must not be skipped");
        assert!(object["contractAddress"].is_null());
    }

    #[test]
    fn the_json_form_round_trips() {
        for contract_address in [None, Some(Address::with_last_byte(0xEF))] {
            let original = transaction_with_logs(contract_address);
            let encoded = serde_json::to_string(&original).expect("serialisation");
            let decoded: TransactionWithLogs =
                serde_json::from_str(&encoded).expect("deserialisation");
            assert_eq!(decoded, original);
        }
    }

    /// The variant names are the wire values for `eth_subscribe`; renaming one
    /// silently breaks every existing subscriber.
    #[rstest::rstest]
    #[case(FlashblocksSubscriptionKind::NewFlashblocks, "newFlashblocks")]
    #[case(FlashblocksSubscriptionKind::PendingLogs, "pendingLogs")]
    #[case(FlashblocksSubscriptionKind::NewFlashblockTransactions, "newFlashblockTransactions")]
    fn the_flashblocks_subscription_kinds_keep_their_wire_names(
        #[case] kind: FlashblocksSubscriptionKind,
        #[case] wire: &str,
    ) {
        assert_eq!(serde_json::to_value(kind).expect("serialisation"), wire);

        let decoded: ExtendedSubscriptionKind =
            serde_json::from_value(serde_json::Value::from(wire)).expect("deserialisation");
        assert_eq!(decoded, ExtendedSubscriptionKind::Flashblocks(kind));
        assert!(decoded.is_flashblocks());
        assert_eq!(decoded.as_standard(), None);
    }

    /// The untagged enum must keep resolving standard kinds, otherwise a delegated
    /// subscription would be misread as an unknown flashblocks kind.
    #[test]
    fn a_standard_subscription_kind_still_resolves() {
        let decoded: ExtendedSubscriptionKind =
            serde_json::from_value(serde_json::Value::from("newHeads")).expect("deserialisation");

        assert_eq!(decoded.as_standard(), Some(SubscriptionKind::NewHeads));
        assert!(!decoded.is_flashblocks());
    }
}

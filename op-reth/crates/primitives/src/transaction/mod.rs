//! Optimism transaction types

mod tx_type;

/// Kept for consistency tests
#[cfg(test)]
mod signed;

pub use op_alloy_consensus::{
    OpTransaction, OpTxEnvelope, OpTxType, OpTypedTransaction, POST_EXEC_TX_TYPE_ID,
    PostExecPayload, SDMGasEntry, TxPostExec, build_post_exec_tx,
};

/// Signed transaction.
pub type OpTransactionSigned = OpTxEnvelope;

/// Decodes a transaction only when its wire bytes are the canonical EIP-2718 encoding.
///
/// Exact decoding rejects trailing data, but can still accept a legacy transaction with an
/// extra type byte. Re-encoding must preserve the input so transaction hashes and trie roots
/// describe the same bytes.
pub fn decode_2718_canonical<T: alloy_eips::Decodable2718 + alloy_eips::Encodable2718>(
    bytes: &[u8],
) -> alloy_eips::eip2718::Eip2718Result<T> {
    let transaction = T::decode_2718_exact(bytes)?;
    if transaction.encoded_2718() != bytes {
        return Err(alloy_rlp::Error::Custom("non-canonical transaction encoding").into());
    }
    Ok(transaction)
}

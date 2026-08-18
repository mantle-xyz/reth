pub use alloy_op_evm::{
    spec as revm_spec, spec_by_timestamp_after_bedrock as revm_spec_by_timestamp_after_bedrock,
};
use op_alloy_rpc_types_engine::OpFlashblockPayloadBase;
use revm::primitives::{Address, B256, Bytes};

/// Context relevant for execution of a next block w.r.t OP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpNextBlockEnvAttributes {
    /// The timestamp of the next block.
    pub timestamp: u64,
    /// The suggested fee recipient for the next block.
    pub suggested_fee_recipient: Address,
    /// The randomness value for the next block.
    pub prev_randao: B256,
    /// Block gas limit.
    pub gas_limit: u64,
    /// The parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// Encoded EIP-1559 parameters to include into block's `extra_data` field.
    pub extra_data: Bytes,
}

#[cfg(feature = "rpc")]
impl<H: alloy_consensus::BlockHeader> reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv<H>
    for OpNextBlockEnvAttributes
{
    fn build_pending_env(parent: &crate::SealedHeader<H>) -> Self {
        Self {
            timestamp: parent.timestamp().saturating_add(12),
            suggested_fee_recipient: parent.beneficiary(),
            prev_randao: B256::random(),
            gas_limit: parent.gas_limit(),
            // Default the parent beacon block root to zero rather than inheriting the parent's
            // value. A pending/simulated block is not a real block, so it has no parent beacon
            // block root of its own; carrying the parent's value over reports a root that never
            // belonged to this block. Zeroing matches op-geth, which initializes the field to the
            // zero hash for `eth_simulateV1` and only fills it from an explicit `beaconRoot`
            // block override (`internal/ethapi/simulate.go`, `makeHeaders`).
            //
            // Upstream reth made the same change for its Ethereum `NextBlockEnvAttributes` in
            // paradigmxyz/reth#24652, citing go-ethereum as the reference. That PR did not touch
            // this OP-specific implementation, so we mirror it here. `.map()` preserves the
            // `Option` (i.e. "is this field present at all", gated on Cancun) and only replaces
            // the inner value.
            parent_beacon_block_root: parent.parent_beacon_block_root().map(|_| B256::ZERO),
            extra_data: parent.extra_data().clone(),
        }
    }
}

impl From<OpFlashblockPayloadBase> for OpNextBlockEnvAttributes {
    fn from(base: OpFlashblockPayloadBase) -> Self {
        Self {
            timestamp: base.timestamp,
            suggested_fee_recipient: base.fee_recipient,
            prev_randao: base.prev_randao,
            gas_limit: base.gas_limit,
            parent_beacon_block_root: Some(base.parent_beacon_block_root),
            extra_data: base.extra_data,
        }
    }
}

#[cfg(all(test, feature = "rpc"))]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use reth_primitives_traits::SealedHeader;
    use reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv;

    /// A pending/simulated block must not inherit the parent's beacon block root: it is not a real
    /// block, so it has no such root. Zeroing matches op-geth's `eth_simulateV1` and upstream
    /// paradigmxyz/reth#24652.
    #[test]
    fn pending_env_defaults_parent_beacon_root_to_zero() {
        let header = Header {
            parent_beacon_block_root: Some(B256::repeat_byte(0x42)),
            ..Default::default()
        };
        let sealed = SealedHeader::new(header, B256::ZERO);

        let attrs = OpNextBlockEnvAttributes::build_pending_env(&sealed);

        assert_eq!(attrs.parent_beacon_block_root, Some(B256::ZERO));
    }

    /// Zeroing must not turn `None` into `Some`: the `Option` encodes whether the field exists at
    /// all (gated on Cancun), which is independent of its value. A pre-Cancun parent stays `None`.
    #[test]
    fn pending_env_keeps_absent_parent_beacon_root_absent() {
        let header = Header { parent_beacon_block_root: None, ..Default::default() };
        let sealed = SealedHeader::new(header, B256::ZERO);

        let attrs = OpNextBlockEnvAttributes::build_pending_env(&sealed);

        assert_eq!(attrs.parent_beacon_block_root, None);
    }
}

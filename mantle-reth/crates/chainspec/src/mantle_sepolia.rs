//! Chain specification for the Mantle Sepolia network.

use crate::{
    OpChainSpec,
    mantle::{MantleGenesisInfo, configure_mantle_genesis},
};
use alloc::sync::Arc;
use alloy_op_hardforks::{
    MANTLE_SEPOLIA_ARSIA_TIMESTAMP, MANTLE_SEPOLIA_LIMB_TIMESTAMP, MANTLE_SEPOLIA_SKADI_TIMESTAMP,
};
use reth_primitives_traits::sync::LazyLock;

/// The Mantle Sepolia spec with hardcoded Mantle hardfork timestamps.
pub static MANTLE_SEPOLIA: LazyLock<Arc<OpChainSpec>> = LazyLock::new(|| {
    let genesis = create_mantle_sepolia_genesis();
    let spec = crate::from_mantle_genesis(genesis);
    Arc::new(spec)
});

fn create_mantle_sepolia_genesis() -> alloy_genesis::Genesis {
    let mut genesis: alloy_genesis::Genesis =
        serde_json::from_str(include_str!("../res/genesis/mantle_sepolia.json"))
            .expect("invalid Mantle Sepolia genesis JSON");
    configure_mantle_genesis(
        &mut genesis,
        MantleGenesisInfo {
            mantle_skadi_time: Some(MANTLE_SEPOLIA_SKADI_TIMESTAMP),
            mantle_limb_time: Some(MANTLE_SEPOLIA_LIMB_TIMESTAMP),
            mantle_arsia_time: Some(MANTLE_SEPOLIA_ARSIA_TIMESTAMP),
        },
    );
    genesis
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_op_hardforks::{MantleHardfork, OpHardforks};
    use reth_chainspec::EthereumHardforks;

    #[test]
    fn verify_mantle_sepolia_chain_id() {
        assert_eq!(MANTLE_SEPOLIA.chain.id(), 5003);
    }

    #[test]
    fn verify_mantle_sepolia_skadi_activates_l1_forks() {
        let spec = &*MANTLE_SEPOLIA;
        assert!(spec.is_shanghai_active_at_timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP));
        assert!(spec.is_cancun_active_at_timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP));
        assert!(spec.is_prague_active_at_timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP));
        assert!(!spec.is_shanghai_active_at_timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP - 1));
    }

    #[test]
    fn verify_mantle_sepolia_arsia_activates_op_forks() {
        let spec = &*MANTLE_SEPOLIA;
        assert!(spec.is_canyon_active_at_timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP));
        assert!(spec.is_fjord_active_at_timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP));
        assert!(spec.is_granite_active_at_timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP));
        assert!(spec.is_holocene_active_at_timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP));
        assert!(spec.is_jovian_active_at_timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP));
        assert!(!spec.is_canyon_active_at_timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP - 1));
    }

    #[test]
    fn verify_mantle_sepolia_mantle_hardforks() {
        let spec = &*MANTLE_SEPOLIA;
        assert!(
            spec.is_fork_active_at_timestamp(MantleHardfork::Skadi, MANTLE_SEPOLIA_SKADI_TIMESTAMP)
        );
        assert!(
            spec.is_fork_active_at_timestamp(MantleHardfork::Limb, MANTLE_SEPOLIA_LIMB_TIMESTAMP)
        );
        assert!(
            spec.is_fork_active_at_timestamp(MantleHardfork::Arsia, MANTLE_SEPOLIA_ARSIA_TIMESTAMP)
        );
    }
}

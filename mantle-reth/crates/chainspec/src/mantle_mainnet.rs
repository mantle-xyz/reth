//! Chain specification for the Mantle Mainnet network.

use crate::{
    OpChainSpec,
    mantle::{MantleGenesisInfo, configure_mantle_genesis},
};
use alloc::sync::Arc;
use alloy_op_hardforks::{
    MANTLE_MAINNET_ARSIA_TIMESTAMP, MANTLE_MAINNET_LIMB_TIMESTAMP, MANTLE_MAINNET_SKADI_TIMESTAMP,
};
use reth_primitives_traits::{SealedHeader, sync::LazyLock};

const MANTLE_MAINNET_GENESIS_HASH: alloy_primitives::B256 =
    alloy_primitives::b256!("0xcd3253817bbf6ae83c9839c362a0688a83d59d2fabeb9463b348cc98c4b056aa");

/// The Mantle Mainnet spec with hardcoded Mantle hardfork timestamps.
pub static MANTLE_MAINNET: LazyLock<Arc<OpChainSpec>> = LazyLock::new(|| {
    let genesis = create_mantle_mainnet_genesis();
    let mut spec = crate::from_mantle_genesis(genesis);
    spec.inner.prune_delete_limit = 10000;
    spec.inner.genesis_header =
        SealedHeader::new(spec.inner.genesis_header.clone_header(), MANTLE_MAINNET_GENESIS_HASH);
    Arc::new(spec)
});

fn create_mantle_mainnet_genesis() -> alloy_genesis::Genesis {
    let mut genesis: alloy_genesis::Genesis =
        serde_json::from_str(include_str!("../res/genesis/mantle.json"))
            .expect("invalid Mantle mainnet genesis JSON");
    configure_mantle_genesis(
        &mut genesis,
        MantleGenesisInfo {
            mantle_skadi_time: Some(MANTLE_MAINNET_SKADI_TIMESTAMP),
            mantle_limb_time: Some(MANTLE_MAINNET_LIMB_TIMESTAMP),
            mantle_arsia_time: Some(MANTLE_MAINNET_ARSIA_TIMESTAMP),
        },
    );
    genesis
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_hardforks::Hardfork;
    use alloy_op_hardforks::{MantleHardfork, OpHardforks};
    use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, EthereumHardforks};

    #[test]
    fn verify_mantle_mainnet_chain_id() {
        assert_eq!(MANTLE_MAINNET.chain.id(), 5000);
    }

    #[test]
    fn verify_mantle_mainnet_genesis_hash() {
        let header = MANTLE_MAINNET.genesis_header();
        assert_eq!(
            MANTLE_MAINNET.genesis_hash(),
            alloy_primitives::b256!(
                "0xcd3253817bbf6ae83c9839c362a0688a83d59d2fabeb9463b348cc98c4b056aa"
            )
        );
        let _ = header;
    }

    #[test]
    fn verify_mantle_mainnet_skadi_activates_l1_forks() {
        let spec = &*MANTLE_MAINNET;
        assert!(spec.is_shanghai_active_at_timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP));
        assert!(spec.is_cancun_active_at_timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP));
        assert!(spec.is_prague_active_at_timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP));
        assert!(!spec.is_shanghai_active_at_timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP - 1));
    }

    #[test]
    fn verify_mantle_mainnet_arsia_activates_op_forks() {
        let spec = &*MANTLE_MAINNET;
        assert!(spec.is_canyon_active_at_timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP));
        assert!(spec.is_fjord_active_at_timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP));
        assert!(spec.is_granite_active_at_timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP));
        assert!(spec.is_holocene_active_at_timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP));
        assert!(spec.is_jovian_active_at_timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP));
        assert!(!spec.is_canyon_active_at_timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP - 1));
        assert!(!spec.is_jovian_active_at_timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP - 1));
    }

    #[test]
    fn verify_mantle_mainnet_mantle_hardforks() {
        let spec = &*MANTLE_MAINNET;
        assert!(
            spec.is_fork_active_at_timestamp(MantleHardfork::Skadi, MANTLE_MAINNET_SKADI_TIMESTAMP)
        );
        assert!(
            spec.is_fork_active_at_timestamp(MantleHardfork::Limb, MANTLE_MAINNET_LIMB_TIMESTAMP)
        );
        assert!(
            spec.is_fork_active_at_timestamp(MantleHardfork::Arsia, MANTLE_MAINNET_ARSIA_TIMESTAMP)
        );
        assert!(!spec.is_fork_active_at_timestamp(
            MantleHardfork::Skadi,
            MANTLE_MAINNET_SKADI_TIMESTAMP - 1
        ));
    }

    #[test]
    fn verify_mantle_mainnet_base_fee_params() {
        assert_eq!(
            MANTLE_MAINNET.base_fee_params,
            BaseFeeParamsKind::Variable(
                vec![(MantleHardfork::Arsia.boxed(), BaseFeeParams::new(8, 2))].into()
            )
        );
    }
}

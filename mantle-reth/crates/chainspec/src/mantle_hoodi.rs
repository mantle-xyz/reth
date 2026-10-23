//! Chain specification for the Mantle Hoodi network.

use crate::OpChainSpec;
use alloc::{borrow::ToOwned, sync::Arc};
use alloy_op_hardforks::MANTLE_HOODI_BVM_UPGRADE_TIMESTAMP;
use reth_primitives_traits::sync::LazyLock;

/// The Mantle Hoodi spec with the hardcoded `HoodiBvmUpgrade` timestamp.
///
/// Unlike Mantle mainnet and Sepolia, its genesis is Mantle Hoodi's public block zero, shipped
/// verbatim from `mantle-xyz/networks` `hoodi/genesis.json`. Its hash must equal the rollup
/// config's `genesis.l2.hash`: changing the file makes existing MDBX databases fail the genesis
/// compatibility check at startup. The genesis already activates every mainline fork at zero, so
/// only the `HoodiBvmUpgrade` timestamp is added on top of it.
pub static MANTLE_HOODI: LazyLock<Arc<OpChainSpec>> = LazyLock::new(|| {
    let genesis = create_mantle_hoodi_genesis();
    let spec = crate::from_mantle_genesis(genesis);
    Arc::new(spec)
});

fn create_mantle_hoodi_genesis() -> alloy_genesis::Genesis {
    let mut genesis: alloy_genesis::Genesis =
        serde_json::from_str(include_str!("../res/genesis/mantle_hoodi.json"))
            .expect("invalid Mantle Hoodi genesis JSON");
    if let Some(upgrade_time) = MANTLE_HOODI_BVM_UPGRADE_TIMESTAMP {
        genesis
            .config
            .extra_fields
            .insert("mantleHoodiBvmUpgradeTime".to_owned(), serde_json::json!(upgrade_time));
    }
    genesis
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_op_hardforks::{MANTLE_HOODI_CHAIN_ID, MantleHardfork, OpHardforks};
    use reth_chainspec::EthereumHardforks;

    #[test]
    fn verify_mantle_hoodi_chain_id() {
        assert_eq!(MANTLE_HOODI.chain.id(), MANTLE_HOODI_CHAIN_ID);
    }

    /// The genesis hash is `genesis.l2.hash` of `mantle-xyz/networks` `hoodi/rollup.json`.
    #[test]
    fn verify_mantle_hoodi_genesis_hash() {
        let expected = alloy_primitives::b256!(
            "0x3cf04a58242c39e5faa0502736fa6aa8f99479ab3ab615d0ce8f326b7f1490d3"
        );
        let header = MANTLE_HOODI.genesis_header();

        assert_eq!(header.hash_slow(), expected);
        assert_eq!(MANTLE_HOODI.genesis_hash(), expected);
    }

    #[test]
    fn verify_mantle_hoodi_mainline_forks_active_at_genesis() {
        let spec = &*MANTLE_HOODI;
        assert!(spec.is_prague_active_at_timestamp(0));
        assert!(spec.is_osaka_active_at_timestamp(0));
        assert!(spec.is_isthmus_active_at_timestamp(0));
        assert!(spec.is_jovian_active_at_timestamp(0));
        assert!(spec.is_fork_active_at_timestamp(MantleHardfork::Skadi, 0));
        assert!(spec.is_fork_active_at_timestamp(MantleHardfork::Limb, 0));
        assert!(spec.is_fork_active_at_timestamp(MantleHardfork::Arsia, 0));
    }

    #[test]
    fn verify_mantle_hoodi_bvm_upgrade_follows_bundled_time() {
        let spec = &*MANTLE_HOODI;
        match MANTLE_HOODI_BVM_UPGRADE_TIMESTAMP {
            Some(time) => {
                assert!(spec.is_fork_active_at_timestamp(MantleHardfork::HoodiBvmUpgrade, time));
                assert!(
                    !spec.is_fork_active_at_timestamp(MantleHardfork::HoodiBvmUpgrade, time - 1)
                );
            }
            None => assert!(
                !spec.is_fork_active_at_timestamp(MantleHardfork::HoodiBvmUpgrade, u64::MAX)
            ),
        }
    }
}

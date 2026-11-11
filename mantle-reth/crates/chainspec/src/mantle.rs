use alloc::{borrow::ToOwned, vec};
use alloy_genesis::Genesis;
use alloy_hardforks::Hardfork;
use alloy_op_hardforks::MantleHardfork;
use alloy_primitives::U256;
use alloy_serde::OtherFields;
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind};

pub(crate) const MANTLE_BASE_FEE_DENOMINATOR: u64 = 8;
pub(crate) const MANTLE_BASE_FEE_ELASTICITY: u64 = 2;

#[derive(Debug, Default, Clone)]
pub(crate) struct MantleChainInfo {
    pub genesis_info: Option<MantleGenesisInfo>,
}

impl MantleChainInfo {
    pub(crate) fn extract_from(others: &OtherFields) -> Option<Self> {
        Self::try_from(others).ok()
    }
}

impl TryFrom<&OtherFields> for MantleChainInfo {
    type Error = serde_json::Error;

    fn try_from(others: &OtherFields) -> Result<Self, Self::Error> {
        let genesis_info = MantleGenesisInfo::try_from(others).ok();
        Ok(Self { genesis_info })
    }
}

#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub(crate) struct MantleGenesisInfo {
    pub mantle_skadi_time: Option<u64>,
    pub mantle_limb_time: Option<u64>,
    pub mantle_arsia_time: Option<u64>,
    /// Mantle Hoodi only: activation time of the `HoodiBvmUpgrade` irregular state transition.
    pub mantle_hoodi_bvm_upgrade_time: Option<u64>,
}

#[cfg(feature = "serde")]
impl TryFrom<&OtherFields> for MantleGenesisInfo {
    type Error = serde_json::Error;

    fn try_from(others: &OtherFields) -> Result<Self, Self::Error> {
        others.deserialize_as()
    }
}

#[cfg(not(feature = "serde"))]
impl TryFrom<&OtherFields> for MantleGenesisInfo {
    type Error = serde_json::Error;

    fn try_from(others: &OtherFields) -> Result<Self, Self::Error> {
        // A `null` time reads as unset, as with the `serde` feature and op-geth's `*uint64`.
        let time =
            |key| others.get_deserialized::<Option<u64>>(key).transpose().map(Option::flatten);
        let mantle_skadi_time = time("mantleSkadiTime")?;
        let mantle_limb_time = time("mantleLimbTime")?;
        let mantle_arsia_time = time("mantleArsiaTime")?;
        let mantle_hoodi_bvm_upgrade_time = time("mantleHoodiBvmUpgradeTime")?;
        Ok(Self {
            mantle_skadi_time,
            mantle_limb_time,
            mantle_arsia_time,
            mantle_hoodi_bvm_upgrade_time,
        })
    }
}

pub(crate) fn extract_mantle_base_fee_params(
    optimism_chain_info: &op_alloy_rpc_types::OpChainInfo,
) -> BaseFeeParamsKind {
    optimism_chain_info
        .base_fee_info
        .and_then(|info| {
            info.eip1559_denominator.zip(info.eip1559_elasticity).map(
                |(denominator, elasticity)| {
                    BaseFeeParamsKind::Variable(
                        vec![(
                            MantleHardfork::Arsia.boxed(),
                            BaseFeeParams::new(denominator as u128, elasticity as u128),
                        )]
                        .into(),
                    )
                },
            )
        })
        .unwrap_or_else(|| BaseFeeParamsKind::Constant(BaseFeeParams::ethereum()))
}

pub(crate) fn configure_mantle_genesis(
    genesis: &mut Genesis,
    mantle_genesis_info: MantleGenesisInfo,
) {
    // Keep London active at the synthetic block zero. When `baseFeePerGas` is absent,
    // `reth_chainspec::make_genesis_header` deliberately supplies the EIP-1559 initial base fee of
    // 1 gwei. The resulting `base_fee_per_gas` is part of the built-in chainspec identity; removing
    // it would require a new genesis hash and make existing MDBX databases fail the startup
    // compatibility check.
    genesis.config.london_block.get_or_insert(0);
    genesis.config.arrow_glacier_block.get_or_insert(0);
    genesis.config.gray_glacier_block.get_or_insert(0);
    genesis.config.merge_netsplit_block.get_or_insert(0);
    genesis.config.terminal_total_difficulty = Some(U256::ZERO);
    genesis.config.terminal_total_difficulty_passed = true;

    // These EIP-1559 params are used as defaults by the Arsia payload builder when
    // PayloadAttributes does not specify overrides. Pre-Arsia, basefee is inherited from
    // the parent block directly (see CalcBaseFee in op-geth eip1559.go:67-68).
    let mut extra_fields = serde_json::Map::from_iter([
        ("bedrockBlock".to_owned(), serde_json::json!(0)),
        ("regolithTime".to_owned(), serde_json::json!(0)),
        (
            "optimism".to_owned(),
            serde_json::json!({
                "eip1559Elasticity": MANTLE_BASE_FEE_ELASTICITY,
                "eip1559Denominator": MANTLE_BASE_FEE_DENOMINATOR,
            }),
        ),
    ]);
    if let Some(skadi_time) = mantle_genesis_info.mantle_skadi_time {
        extra_fields.insert("mantleSkadiTime".to_owned(), serde_json::json!(skadi_time));
    }
    if let Some(limb_time) = mantle_genesis_info.mantle_limb_time {
        extra_fields.insert("mantleLimbTime".to_owned(), serde_json::json!(limb_time));
    }
    if let Some(arsia_time) = mantle_genesis_info.mantle_arsia_time {
        extra_fields.insert("mantleArsiaTime".to_owned(), serde_json::json!(arsia_time));
    }
    if let Some(upgrade_time) = mantle_genesis_info.mantle_hoodi_bvm_upgrade_time {
        extra_fields
            .insert("mantleHoodiBvmUpgradeTime".to_owned(), serde_json::json!(upgrade_time));
    }

    genesis.config.extra_fields =
        serde_json::Value::Object(extra_fields).try_into().unwrap_or_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configure_mantle_genesis_uses_mantle_genesis_info() {
        let mut genesis = Genesis::default();
        let mantle_genesis_info = MantleGenesisInfo {
            mantle_skadi_time: Some(10),
            mantle_limb_time: Some(20),
            mantle_arsia_time: None,
            mantle_hoodi_bvm_upgrade_time: None,
        };

        configure_mantle_genesis(&mut genesis, mantle_genesis_info);

        assert_eq!(
            MantleGenesisInfo::try_from(&genesis.config.extra_fields).unwrap(),
            mantle_genesis_info
        );
        assert_eq!(genesis.config.extra_fields.get("bedrockBlock"), Some(&serde_json::json!(0)));
        assert_eq!(genesis.config.extra_fields.get("regolithTime"), Some(&serde_json::json!(0)));
        assert_eq!(genesis.config.extra_fields.get("mantleArsiaTime"), None);
    }

    /// A `null` fork time reads as unset and does not drop the other fork times, with and
    /// without the `serde` feature.
    #[test]
    fn null_fork_time_reads_as_unset() {
        let others: OtherFields = serde_json::from_value(serde_json::json!({
            "mantleSkadiTime": 10,
            "mantleLimbTime": 20,
            "mantleArsiaTime": 30,
            "mantleHoodiBvmUpgradeTime": null,
        }))
        .unwrap();

        assert_eq!(
            MantleGenesisInfo::try_from(&others).unwrap(),
            MantleGenesisInfo {
                mantle_skadi_time: Some(10),
                mantle_limb_time: Some(20),
                mantle_arsia_time: Some(30),
                mantle_hoodi_bvm_upgrade_time: None,
            }
        );
    }
}

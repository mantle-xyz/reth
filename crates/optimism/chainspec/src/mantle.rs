use alloc::borrow::ToOwned;
use alloy_genesis::Genesis;
use alloy_primitives::U256;
use alloy_serde::OtherFields;

/// Mantle base fee elasticity (same as Ethereum default)
const MANTLE_BASE_FEE_ELASTICITY: u64 = 2;

/// Mantle base fee denominator (same as Ethereum default)
const MANTLE_BASE_FEE_DENOMINATOR: u64 = 8;

/// Mantle 网络特定的链信息
#[derive(Debug, Default, Clone)]
pub(crate) struct MantleChainInfo {
    /// Genesis information
    pub genesis_info: Option<MantleGenesisInfo>,
}

impl MantleChainInfo {
    /// Extracts the Optimism specific fields from a genesis file. These fields are expected to be
    /// contained in the `genesis.config` under `extra_fields` property.
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

/// The Optimism-specific genesis block specification.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub(crate) struct MantleGenesisInfo {
    /// Mantle Skadi upgrade timestamp
    pub mantle_skadi_time: Option<u64>,
}

impl MantleGenesisInfo {
    /// Extract the Optimism-specific genesis info from a genesis file.
    pub(crate) fn _extract_from(others: &OtherFields) -> Option<Self> {
        Self::try_from(others).ok()
    }
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
        let mantle_skadi_time = others
            .get_deserialized("mantleSkadiTime")
            .transpose()?;

        Ok(Self { mantle_skadi_time })
    }
}

pub(crate) fn configure_mantle_genesis(genesis: &mut Genesis, mantle_genesis_info: MantleGenesisInfo) {
    genesis.config.london_block.get_or_insert(0);
    genesis.config.arrow_glacier_block.get_or_insert(0);
    genesis.config.gray_glacier_block.get_or_insert(0);
    genesis.config.merge_netsplit_block.get_or_insert(0);
    genesis.config.terminal_total_difficulty = Some(U256::ZERO);
    genesis.config.terminal_total_difficulty_passed = true;

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

    genesis.config.extra_fields =
        serde_json::Value::Object(extra_fields).try_into().unwrap_or_default();
}

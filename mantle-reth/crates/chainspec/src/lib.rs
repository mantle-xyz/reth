//! Mantle chain specifications.
//!
//! Provides `MANTLE_MAINNET`, `MANTLE_SEPOLIA` statics and the `from_mantle_genesis` function
//! for converting a [`Genesis`] JSON into a Mantle-configured [`OpChainSpec`].

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod mantle;
mod mantle_mainnet;
mod mantle_sepolia;

pub use alloy_op_hardforks::MantleHardfork;
pub use mantle_mainnet::MANTLE_MAINNET;
pub use mantle_sepolia::MANTLE_SEPOLIA;

use alloc::{vec, vec::Vec};
use alloy_genesis::Genesis;
use alloy_hardforks::Hardfork;
use alloy_op_hardforks::{
    MANTLE_MAINNET_ARSIA_TIMESTAMP, MANTLE_MAINNET_LIMB_TIMESTAMP, MANTLE_MAINNET_SKADI_TIMESTAMP,
    MANTLE_SEPOLIA_ARSIA_TIMESTAMP, MANTLE_SEPOLIA_LIMB_TIMESTAMP, MANTLE_SEPOLIA_SKADI_TIMESTAMP,
    OpHardfork,
};
use alloy_primitives::U256;
use reth_chainspec::ChainSpec;
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition};
use reth_optimism_chainspec::{OpChainSpec, make_op_genesis_header};
use reth_primitives_traits::{SealedHeader, sync::LazyLock};

/// Mantle mainnet list of hardforks.
///
/// Mantle's hardfork history differs from standard OP Stack:
/// - All pre-merge EVM forks at Block(0)
/// - Bedrock at Block(0), Regolith at Timestamp(0)
/// - Skadi activates Shanghai + Cancun + Prague + Ecotone + Isthmus simultaneously
/// - Arsia activates Canyon + Fjord + Granite + Holocene + Jovian simultaneously
static MANTLE_MAINNET_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(vec![
        (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::ArrowGlacier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::GrayGlacier.boxed(), ForkCondition::Block(0)),
        (
            EthereumHardfork::Paris.boxed(),
            ForkCondition::TTD {
                activation_block_number: 0,
                fork_block: Some(0),
                total_difficulty: U256::ZERO,
            },
        ),
        (OpHardfork::Bedrock.boxed(), ForkCondition::Block(0)),
        (OpHardfork::Regolith.boxed(), ForkCondition::Timestamp(0)),
        // Skadi activates L1 forks: Shanghai + Cancun + Prague
        (
            EthereumHardfork::Shanghai.boxed(),
            ForkCondition::Timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP),
        ),
        (OpHardfork::Ecotone.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP)),
        (
            EthereumHardfork::Cancun.boxed(),
            ForkCondition::Timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP),
        ),
        (
            EthereumHardfork::Prague.boxed(),
            ForkCondition::Timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP),
        ),
        (OpHardfork::Isthmus.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP)),
        (MantleHardfork::Skadi.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_SKADI_TIMESTAMP)),
        (MantleHardfork::Limb.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_LIMB_TIMESTAMP)),
        // Arsia activates remaining OP forks: Canyon + Fjord + Granite + Holocene + Jovian
        (OpHardfork::Canyon.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP)),
        (OpHardfork::Fjord.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP)),
        (OpHardfork::Granite.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP)),
        (OpHardfork::Holocene.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP)),
        (OpHardfork::Jovian.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP)),
        (MantleHardfork::Arsia.boxed(), ForkCondition::Timestamp(MANTLE_MAINNET_ARSIA_TIMESTAMP)),
    ])
});

/// Mantle Sepolia list of hardforks.
#[allow(dead_code)]
static MANTLE_SEPOLIA_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(vec![
        (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::ArrowGlacier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::GrayGlacier.boxed(), ForkCondition::Block(0)),
        (
            EthereumHardfork::Paris.boxed(),
            ForkCondition::TTD {
                activation_block_number: 0,
                fork_block: Some(0),
                total_difficulty: U256::ZERO,
            },
        ),
        (OpHardfork::Bedrock.boxed(), ForkCondition::Block(0)),
        (OpHardfork::Regolith.boxed(), ForkCondition::Timestamp(0)),
        // Skadi activates L1 forks: Shanghai + Cancun + Prague
        (
            EthereumHardfork::Shanghai.boxed(),
            ForkCondition::Timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP),
        ),
        (OpHardfork::Ecotone.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP)),
        (
            EthereumHardfork::Cancun.boxed(),
            ForkCondition::Timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP),
        ),
        (
            EthereumHardfork::Prague.boxed(),
            ForkCondition::Timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP),
        ),
        (OpHardfork::Isthmus.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP)),
        (MantleHardfork::Skadi.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_SKADI_TIMESTAMP)),
        (MantleHardfork::Limb.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_LIMB_TIMESTAMP)),
        // Arsia activates remaining OP forks: Canyon + Fjord + Granite + Holocene + Jovian
        (OpHardfork::Canyon.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP)),
        (OpHardfork::Fjord.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP)),
        (OpHardfork::Granite.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP)),
        (OpHardfork::Holocene.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP)),
        (OpHardfork::Jovian.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP)),
        (MantleHardfork::Arsia.boxed(), ForkCondition::Timestamp(MANTLE_SEPOLIA_ARSIA_TIMESTAMP)),
    ])
});

/// Converts a [`Genesis`] into a Mantle-configured [`OpChainSpec`].
///
/// This function applies Mantle's non-standard hardfork alignment:
/// - Skadi activates Shanghai + Cancun + Prague + Ecotone + Isthmus simultaneously.
/// - Arsia activates Canyon + Fjord + Granite + Holocene + Jovian simultaneously.
///
/// For Mantle mainnet/Sepolia the genesis JSON is pre-configured by
/// `configure_mantle_genesis`. For unknown Mantle-compatible chains, hardfork
/// timestamps are read from the genesis `extra_fields`.
pub fn from_mantle_genesis(genesis: Genesis) -> OpChainSpec {
    use mantle::MantleChainInfo;

    // Extract OP genesis info (for base_fee_params from the `optimism` field)
    let optimism_chain_info =
        op_alloy_rpc_types::OpChainInfo::extract_from(&genesis.config.extra_fields)
            .unwrap_or_default();
    let op_genesis_info = optimism_chain_info.genesis_info.unwrap_or_default();

    // Extract Mantle-specific genesis info
    let mantle_chain_info = MantleChainInfo::extract_from(&genesis.config.extra_fields);
    let mantle_genesis_info =
        mantle_chain_info.as_ref().and_then(|c| c.genesis_info).unwrap_or_default();

    let skadi_ts = mantle_genesis_info.mantle_skadi_time;
    let limb_ts = mantle_genesis_info.mantle_limb_time;
    let arsia_ts = mantle_genesis_info.mantle_arsia_time;

    // Block-based hardforks (same as standard OP)
    let hardfork_opts = [
        (EthereumHardfork::Frontier.boxed(), Some(0u64)),
        (EthereumHardfork::Homestead.boxed(), genesis.config.homestead_block),
        (EthereumHardfork::Tangerine.boxed(), genesis.config.eip150_block),
        (EthereumHardfork::SpuriousDragon.boxed(), genesis.config.eip155_block),
        (EthereumHardfork::Byzantium.boxed(), genesis.config.byzantium_block),
        (EthereumHardfork::Constantinople.boxed(), genesis.config.constantinople_block),
        (EthereumHardfork::Petersburg.boxed(), genesis.config.petersburg_block),
        (EthereumHardfork::Istanbul.boxed(), genesis.config.istanbul_block),
        (EthereumHardfork::MuirGlacier.boxed(), genesis.config.muir_glacier_block),
        (EthereumHardfork::Berlin.boxed(), genesis.config.berlin_block),
        (EthereumHardfork::London.boxed(), genesis.config.london_block),
        (EthereumHardfork::ArrowGlacier.boxed(), genesis.config.arrow_glacier_block),
        (EthereumHardfork::GrayGlacier.boxed(), genesis.config.gray_glacier_block),
        (OpHardfork::Bedrock.boxed(), op_genesis_info.bedrock_block),
    ];
    let mut block_hardforks = hardfork_opts
        .into_iter()
        .filter_map(|(hardfork, opt)| opt.map(|block| (hardfork, ForkCondition::Block(block))))
        .collect::<Vec<_>>();

    // Paris (TTD = 0 for all OP/Mantle chains)
    block_hardforks.push((
        EthereumHardfork::Paris.boxed(),
        ForkCondition::TTD {
            activation_block_number: 0,
            total_difficulty: U256::ZERO,
            fork_block: genesis.config.merge_netsplit_block,
        },
    ));

    // Mantle time-based hardfork layout:
    // - L1 forks (Shanghai, Cancun, Prague) + Ecotone + Isthmus → Skadi timestamp
    // - Limb → Limb timestamp
    // - OP forks (Canyon, Fjord, Granite, Holocene, Jovian) → Arsia timestamp
    let time_hardfork_opts: Vec<(alloc::boxed::Box<dyn Hardfork>, Option<u64>)> = vec![
        (OpHardfork::Regolith.boxed(), op_genesis_info.regolith_time),
        // Mantle Skadi
        (EthereumHardfork::Shanghai.boxed(), skadi_ts),
        (EthereumHardfork::Cancun.boxed(), skadi_ts),
        (EthereumHardfork::Prague.boxed(), skadi_ts),
        (OpHardfork::Ecotone.boxed(), skadi_ts),
        (OpHardfork::Isthmus.boxed(), skadi_ts),
        (MantleHardfork::Skadi.boxed(), skadi_ts),
        // Mantle Limb
        (MantleHardfork::Limb.boxed(), limb_ts),
        // Mantle Arsia
        (OpHardfork::Canyon.boxed(), arsia_ts),
        (OpHardfork::Fjord.boxed(), arsia_ts),
        (OpHardfork::Granite.boxed(), arsia_ts),
        (OpHardfork::Holocene.boxed(), arsia_ts),
        (OpHardfork::Jovian.boxed(), arsia_ts),
        (MantleHardfork::Arsia.boxed(), arsia_ts),
    ];

    let mut time_hardforks = time_hardfork_opts
        .into_iter()
        .filter_map(|(hardfork, opt)| opt.map(|time| (hardfork, ForkCondition::Timestamp(time))))
        .collect::<Vec<_>>();

    block_hardforks.append(&mut time_hardforks);

    // Use MANTLE_MAINNET_HARDFORKS as the canonical ordering template
    let ordering_template = MANTLE_MAINNET_HARDFORKS.clone();
    let mainnet_order = ordering_template.forks_iter();

    let mut ordered_hardforks = Vec::with_capacity(block_hardforks.len());
    for (hardfork, _) in mainnet_order {
        if let Some(pos) = block_hardforks.iter().position(|(e, _)| **e == *hardfork) {
            ordered_hardforks.push(block_hardforks.remove(pos));
        }
    }
    // Append remaining unknown hardforks
    ordered_hardforks.append(&mut block_hardforks);

    let hardforks = ChainHardforks::new(ordered_hardforks);
    let genesis_header = SealedHeader::seal_slow(make_op_genesis_header(&genesis, &hardforks));

    let base_fee_params = mantle::extract_mantle_base_fee_params(&optimism_chain_info);

    OpChainSpec {
        inner: ChainSpec {
            chain: genesis.config.chain_id.into(),
            genesis_header,
            genesis,
            hardforks,
            paris_block_and_final_difficulty: Some((0, U256::ZERO)),
            base_fee_params,
            ..Default::default()
        },
    }
}

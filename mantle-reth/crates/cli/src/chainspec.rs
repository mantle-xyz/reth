use mantle_reth_chainspec::{MANTLE_MAINNET, MANTLE_SEPOLIA, from_mantle_genesis};
use reth_cli::chainspec::{ChainSpecParser, parse_genesis};
use reth_optimism_chainspec::OpChainSpec;
use std::sync::Arc;

// `mantle` is the canonical mainnet name and stays first so it remains the default.
// `mantle-mainnet` is a backwards-compatible alias for the pre-elysium chain name.
const MANTLE_SUPPORTED_CHAINS: &[&str] = &["mantle", "mantle-mainnet", "mantle-sepolia"];

/// Mantle chain specification parser.
///
/// Supports `--chain mantle` (alias `mantle-mainnet`), `--chain mantle-sepolia`, and JSON
/// genesis files.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MantleChainSpecParser;

impl ChainSpecParser for MantleChainSpecParser {
    type ChainSpec = OpChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = MANTLE_SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>> {
        match s {
            "mantle" | "mantle-mainnet" => Ok(MANTLE_MAINNET.clone()),
            "mantle-sepolia" => Ok(MANTLE_SEPOLIA.clone()),
            _ => {
                let genesis = parse_genesis(s)?;
                Ok(Arc::new(from_mantle_genesis(genesis)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mantle_mainnet() {
        let spec = MantleChainSpecParser::parse("mantle").unwrap();
        assert_eq!(spec.chain.id(), 5000);
    }

    #[test]
    fn parse_mantle_mainnet_alias() {
        // `mantle-mainnet` is a backwards-compatible alias for `mantle` (pre-elysium name).
        let spec = MantleChainSpecParser::parse("mantle-mainnet").unwrap();
        assert_eq!(spec.chain.id(), 5000);
    }

    #[test]
    fn parse_mantle_sepolia() {
        let spec = MantleChainSpecParser::parse("mantle-sepolia").unwrap();
        assert_eq!(spec.chain.id(), 5003);
    }

    #[test]
    fn parse_known_chains() {
        for &chain in MantleChainSpecParser::SUPPORTED_CHAINS {
            assert!(
                <MantleChainSpecParser as ChainSpecParser>::parse(chain).is_ok(),
                "Failed to parse {chain}"
            );
        }
    }
}

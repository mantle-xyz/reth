//! Test [`OpEvmConfig`] bound to [`super::chainspec::test_chain_spec`].

use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::OpEvmConfig;

use super::chainspec::test_chain_spec;

/// Construct the test EVM config. Wraps `OpEvmConfig::optimism(...)`
/// with the shared test chainspec — equivalent to what the cli sets up
/// via `OpExecutorBuilder`, minus the SDM enable flag (off by default
/// for tests; individual tests can flip via `.with_sdm_enabled(true)`).
pub fn test_evm_config() -> OpEvmConfig<OpChainSpec> {
    OpEvmConfig::optimism(test_chain_spec())
}

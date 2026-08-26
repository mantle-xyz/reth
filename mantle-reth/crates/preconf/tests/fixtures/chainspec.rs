//! Test [`OpChainSpec`] for in-process preconf builder tests.
//!
//! Uses `OpChainSpecBuilder::optimism_mainnet()` as the base, but
//! activates **all** OP hardforks at genesis (timestamp 0) so tests
//! don't have to advance time to exercise post-Bedrock / -Regolith /
//! -Canyon / -Ecotone / -Holocene behaviour. Each test gets the same
//! deterministic chain spec via [`test_chain_spec`].

use std::sync::Arc;

use reth_optimism_chainspec::{OpChainSpec, OpChainSpecBuilder};

/// Build the shared test chainspec. Each call returns a fresh
/// `Arc<OpChainSpec>` (cheap to clone — internals are `Arc`-backed),
/// so tests can take ownership without contending on a global.
///
/// **Step 2 stub**: currently returns `optimism_mainnet()` unchanged.
/// Step 3+ will customise with `with_holocene_activated()` /
/// `with_isthmus_activated()` / etc. as the first real test exposes
/// what hardfork toggles are required by `apply_pre_execution_changes`
/// + L1-block contract preload.
pub fn test_chain_spec() -> Arc<OpChainSpec> {
    Arc::new(OpChainSpecBuilder::optimism_mainnet().build())
}

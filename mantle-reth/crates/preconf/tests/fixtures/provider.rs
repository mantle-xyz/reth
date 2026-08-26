//! Test [`MockEthProvider`] specialised for OP-stack primitives.
//!
//! reth's `MockEthProvider<T, ChainSpec>` impls all the traits the
//! preconf fork's generator + service builder require:
//! - `BlockReaderIdExt<Header = HeaderTy<OpPrimitives>>`
//! - `StateProviderFactory`
//! - `ChainSpecProvider<ChainSpec = OpChainSpec>`
//! - `CanonStateSubscriptions<Primitives = OpPrimitives>`
//!
//! So pointing it at `OpPrimitives + OpChainSpec` gives us a
//! drop-in client for `PreconfPayloadBuilder::new(...)` / Generator
//! `new_payload_job` without standing up a real database.

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, U256};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::OpPrimitives;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

use super::chainspec::test_chain_spec;

/// Concrete provider type used by every fork-builder test.
pub type TestProvider = MockEthProvider<OpPrimitives, OpChainSpec>;

/// Build a fresh test provider bound to the shared test chainspec.
///
/// Empty by default — call [`seed_with_genesis_parent`] to add the
/// parent block + base accounts the builder needs.
pub fn test_provider() -> TestProvider {
    MockEthProvider::<OpPrimitives>::new().with_chain_spec(test_chain_spec().as_ref().clone())
}

/// Block number used as the "parent" in every fork-builder test.
/// The fork's `build_payload` will produce block `PARENT_BLOCK_NUMBER + 1`.
pub const PARENT_BLOCK_NUMBER: u64 = 100;

/// Timestamp of the parent block. Tests build with `parent_timestamp + 2`
/// (matches OP's ~2s block time) so any post-Bedrock hardfork conditional
/// on timestamp activates uniformly.
pub const PARENT_TIMESTAMP: u64 = 1_700_000_000;

/// Returned by [`seed_with_genesis_parent`] — the test passes
/// `parent_hash` to `PayloadConfig::new` / `BuildArguments`.
#[derive(Debug, Clone, Copy)]
pub struct SeededParent {
    pub hash: B256,
    pub number: u64,
    pub timestamp: u64,
}

/// Seed the provider with the minimum parent state every fork-builder
/// test needs:
///
/// - A sealed parent header (`number = PARENT_BLOCK_NUMBER`) so
///   `client.sealed_header_by_hash(parent_hash)` returns `Some(header)`
/// - `funded_addresses` get `1000 ETH` balance each — covers gas payment for tx fixtures
///
/// **Known TODO** (uncovered until first real EVM test is exercised in
/// Steps 3-8 — these are placeholders the test will reveal whether we
/// need):
/// - L1 block contract code (`op_revm::constants::L1_BLOCK_CONTRACT`) may need to be pre-seeded so
///   `db.load_cache_account(L1_BLOCK_CONTRACT)` in `build_payload` doesn't panic
/// - Hardfork-conditional pre-execution helpers (e.g. `4788_BEACON_ROOTS`) may need to be
///   pre-seeded post-Cancun
pub fn seed_with_genesis_parent(
    provider: &TestProvider,
    funded_addresses: &[Address],
) -> SeededParent {
    let header = Header {
        number: PARENT_BLOCK_NUMBER,
        timestamp: PARENT_TIMESTAMP,
        gas_limit: 30_000_000,
        gas_used: 0,
        base_fee_per_gas: Some(1_000_000_000), // 1 gwei
        ..Default::default()
    };
    let hash = header.hash_slow();
    provider.add_header(hash, header);

    // Pre-fund test addresses. `ExtendedAccount::new` takes (nonce, balance).
    for &addr in funded_addresses {
        provider.add_account(
            addr,
            ExtendedAccount::new(0, U256::from(1_000) * U256::from(10).pow(U256::from(18))),
        );
    }

    SeededParent { hash, number: PARENT_BLOCK_NUMBER, timestamp: PARENT_TIMESTAMP }
}

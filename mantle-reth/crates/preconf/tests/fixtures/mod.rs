//! Shared in-process EVM test harness for `tests/fork_builder.rs`.
//!
//! Builds the minimum reth stack a preconf payload-builder test needs:
//!
//! - **`OpChainSpec`** ([`chainspec::test_chain_spec`]): mainnet-ish genesis with all OP hardforks
//!   active at timestamp 0 so tests don't have to advance time to enable
//!   Bedrock/Regolith/Canyon/Ecotone/...
//! - **`MockEthProvider<OpPrimitives, OpChainSpec>`** ([`provider::test_provider`]): reth's
//!   in-memory provider configured for OP primitives. Satisfies `BlockReaderIdExt +
//!   StateProviderFactory + ChainSpecProvider + CanonStateSubscriptions` — all bounds the preconf
//!   fork's generator needs.
//! - **`OpEvmConfig<OpChainSpec>`** ([`evm::test_evm_config`]): the default OP EVM config bound to
//!   the test chainspec.
//! - **`TestSigners`** ([`signer::test_signers`]): three pre-funded addresses (`Addr1` / `Addr2` /
//!   `Addr3`) backed by deterministic private keys — matches op-geth's preconf test fixtures so
//!   behaviour stays comparable across implementations.
//! - **`pool`** module: pool mock used by the `preconf_and_best_txs_share_state` test (Step 7).
//!   Other tests can use a `NoopTransactionPool` since they only exercise the preconf path.
//!
//! Each helper exposes a const-fn / pub-fn constructor so individual
//! tests can grab the bits they need without rebuilding the world. Tests
//! that need a fully-wired `PreconfPayloadBuilder` call
//! [`build_payload_builder()`] which assembles the above into one call.
//!
//! Step 2 (this commit): file structure + minimal constructors. Step 3
//! lands the first real test and fills in any missing helpers; Step 7
//! adds the pool mock for the share-state test.

#![allow(dead_code)] // helpers are pulled in by individual test files

pub mod chainspec;
pub mod evm;
pub mod provider;
pub mod signer;

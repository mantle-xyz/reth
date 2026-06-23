//! `PreconfPayloadBuilder` — mantle's preconf-aware OP payload builder.
//!
//! Forked from `reth_optimism_payload_builder::OpPayloadBuilder` and
//! `op-rbuilder`'s `StandardOpPayloadBuilder` (pinned via workspace
//! Cargo.toml `reth-optimism-payload-builder`). See
//! `docs/design/preconf-dev-plan.md` §P5f for the rationale (wrapper
//! mode cannot satisfy "RPC receipt == sealed receipt byte-for-byte"
//! because the applier has no channel into the inner OP builder's
//! `State<DB>`; only the fork shares state).
//!
//! Reuses the upstream [`OpPayloadBuilderCtx`] verbatim — mantle does
//! **not** add fields to ctx. The preconf-specific state
//! (`PreconfConfig`, `PreconfTxSet`, the preconf-tx mpsc receiver)
//! lives on this struct and is threaded into the build loop body.
//!
//! Stage A.1 lands across multiple commits:
//!
//! - **Step 2 (commit `08a4722c0`)**: struct skeleton + accessors
//! - **Step 3a (this commit)**: `build_payload` async signature with the
//!   upstream generic bounds; body is `unimplemented!()`
//! - **Step 3b**: fork `OpBuilder::build` body (deposits + sequencer txs
//!   + best txs + finalize) without preconf
//! - **Step 4**: `dispatch.rs` adds the preconf `select!` arm
//! - **Steps 5-9**: job / generator / cleanup / cli wiring / tests
//!
//! [`OpPayloadBuilderCtx`]: reth_optimism_payload_builder::builder::OpPayloadBuilderCtx

use std::sync::Arc;

use reth_basic_payload_builder::BuildArguments;
use reth_optimism_evm::ConfigurePostExecEvm;
use reth_optimism_node::OpBuiltPayload;
use reth_optimism_payload_builder::{
    OpAttributes, OpPayloadPrimitives, config::OpBuilderConfig,
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::BuildNextEnv;
use reth_primitives_traits::{HeaderTy, TxTy};
use reth_revm::cancelled::CancelOnDrop;

use crate::{PreconfConfig, PreconfTxSet};

/// Mantle's preconf-aware OP payload builder.
///
/// Construction is via [`PreconfPayloadBuilder::new`]. The driving
/// loop lives in `build_payload` (lands in subsequent steps), invoked
/// once per payload job by the matching
/// `PreconfPayloadJobGenerator`.
///
/// Type parameters:
/// - `Pool` — reth transaction pool (yields the best-txs iterator)
/// - `Client` — state provider factory + chain-spec provider
/// - `Evm` — [`reth_evm::ConfigureEvm`] impl (production: `OpEvmConfig`)
#[derive(Debug, Clone)]
pub struct PreconfPayloadBuilder<Pool, Client, Evm> {
    pool: Pool,
    client: Client,
    evm_config: Evm,
    /// Forwarded to [`OpPayloadBuilderCtx::builder_config`] on every
    /// [`Self::build_payload`] call. Carries OP-specific DA / gas-limit
    /// / SDM-enable settings.
    builder_config: OpBuilderConfig,
    cfg: Arc<PreconfConfig>,
    fifo: Arc<PreconfTxSet>,
}

impl<Pool, Client, Evm> PreconfPayloadBuilder<Pool, Client, Evm> {
    /// Wrap a pool / client / EVM config with shared preconf handles
    /// and OP builder config.
    ///
    /// Cloning the resulting builder is cheap — `cfg` / `fifo` are
    /// `Arc`s, pool / client / `evm_config` are typically `Arc`-backed
    /// too, and [`OpBuilderConfig`] is a small `Clone` struct.
    pub const fn new(
        pool: Pool,
        client: Client,
        evm_config: Evm,
        builder_config: OpBuilderConfig,
        cfg: Arc<PreconfConfig>,
        fifo: Arc<PreconfTxSet>,
    ) -> Self {
        Self { pool, client, evm_config, builder_config, cfg, fifo }
    }

    /// Borrow the underlying transaction pool.
    pub const fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Borrow the underlying state-provider client.
    pub const fn client(&self) -> &Client {
        &self.client
    }

    /// Borrow the EVM configuration.
    pub const fn evm_config(&self) -> &Evm {
        &self.evm_config
    }

    /// Borrow the OP builder config.
    pub const fn builder_config(&self) -> &OpBuilderConfig {
        &self.builder_config
    }

    /// Borrow the shared preconf config handle.
    pub const fn cfg(&self) -> &Arc<PreconfConfig> {
        &self.cfg
    }

    /// Borrow the shared preconf fifo handle.
    pub const fn fifo(&self) -> &Arc<PreconfTxSet> {
        &self.fifo
    }
}

// ─── build_payload (async) ──────────────────────────────────────────────────

// Forked from `reth_optimism_payload_builder::builder::OpBuilder::build`
// (path workspace dep, see Cargo.toml). The sync upstream is converted
// into `async fn` so future steps can interleave a preconf-tx select!
// arm without restructuring the signature. Generic bounds copied
// verbatim from upstream.
impl<Pool, Client, Evm> PreconfPayloadBuilder<Pool, Client, Evm> {
    /// Drive a single payload job to completion against the in-flight
    /// state, applying preconf-tx commitments mid-block via the inner
    /// `select!` loop (added in Step 4).
    ///
    /// Returns the final [`OpBuiltPayload<N>`] on success. Cancellation
    /// (via `cancel`) cuts the build short and seals whatever has been
    /// applied so far (Step 4 will wire the cancel-aware finalize).
    ///
    /// Step 3a (this commit): signature with full generic bounds in
    /// place; body is `unimplemented!()`. Step 3b lands the real flow.
    ///
    /// Generic parameters `N` (payload primitives) and `Attrs` (payload
    /// attributes) are bound on the method rather than the `impl`
    /// because each call may target different primitives (e.g.
    /// `OpPrimitives` vs. `MantlePrimitives`) without instantiating a
    /// new builder.
    #[allow(clippy::unused_async)]
    pub async fn build_payload<N, Attrs>(
        self,
        args: BuildArguments<Attrs, OpBuiltPayload<N>>,
        cancel: CancelOnDrop,
    ) -> Result<OpBuiltPayload<N>, PayloadBuilderError>
    where
        Pool: reth_transaction_pool::TransactionPool<
                Transaction: reth_optimism_txpool::OpPooledTx<Consensus = N::SignedTx>,
            >,
        Client: reth_storage_api::StateProviderFactory
            + reth_chainspec::ChainSpecProvider<ChainSpec: reth_optimism_forks::OpHardforks>,
        <Client as reth_chainspec::ChainSpecProvider>::ChainSpec:
            reth_chainspec::EthChainSpec + reth_optimism_forks::OpHardforks,
        N: OpPayloadPrimitives,
        N::SignedTx: From<alloy_primitives::Sealed<op_alloy_consensus::TxPostExec>>,
        Evm: ConfigurePostExecEvm<
                Primitives = N,
                NextBlockEnvCtx: BuildNextEnv<
                    Attrs,
                    HeaderTy<N>,
                    <Client as reth_chainspec::ChainSpecProvider>::ChainSpec,
                >,
            >,
        Attrs: OpAttributes<Transaction = TxTy<N>>,
    {
        let _ = (args, cancel, &self.evm_config, &self.builder_config);
        unimplemented!("Step 3b lands the fork of OpBuilder::build body")
    }
}

// Compile-time witness that the upstream OP payload-builder types we
// will need in subsequent steps are reachable from outside their
// home crate with the bounds we'll be using. Optimised out of release
// builds. Keeps `cargo check` clean of `unused_crate_dependencies`
// warnings now while the real wiring is in flight.
#[allow(dead_code)]
fn _upstream_witness() {
    use reth_basic_payload_builder::BuildArguments;
    use reth_optimism_evm::OpEvmConfig;
    use reth_optimism_node::OpBuiltPayload;
    use reth_optimism_payload_builder::{
        OpPayloadBuilderAttributes,
        builder::{ExecutionInfo, OpPayloadBuilderCtx},
    };
    use reth_optimism_primitives::OpTransactionSigned;

    fn _ctx<E: reth_evm::ConfigureEvm, ChainSpec, Attrs>(
        _: &OpPayloadBuilderCtx<E, ChainSpec, Attrs>,
    ) {
    }
    fn _info(_: &ExecutionInfo) {}
    fn _attrs(_: &OpPayloadBuilderAttributes<OpTransactionSigned>) {}
    fn _payload(_: &OpBuiltPayload) {}
    fn _evm(_: &OpEvmConfig) {}
    fn _build_args<Attr, Built: reth_payload_primitives::BuiltPayload>(
        _: &BuildArguments<Attr, Built>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PreconfStatus;

    #[derive(Clone, Debug)]
    struct DummyPool;
    #[derive(Clone, Debug)]
    struct DummyClient;
    #[derive(Clone, Debug)]
    struct DummyEvm;

    #[test]
    fn constructor_threads_shared_handles() {
        let cfg = Arc::new(PreconfConfig::default());
        let fifo = Arc::new(PreconfTxSet::new(8));
        let builder_config = OpBuilderConfig::default();
        let builder = PreconfPayloadBuilder::new(
            DummyPool,
            DummyClient,
            DummyEvm,
            builder_config,
            cfg.clone(),
            fifo.clone(),
        );
        assert!(Arc::ptr_eq(builder.cfg(), &cfg));
        assert!(Arc::ptr_eq(builder.fifo(), &fifo));
        // Arc counts: outer + inside builder = 2 each.
        assert_eq!(Arc::strong_count(&cfg), 2);
        assert_eq!(Arc::strong_count(&fifo), 2);
        // Smoke: PreconfStatus is reachable from this module via crate root
        // re-exports, no need to also test accessor traversals here.
        let _ = PreconfStatus::Waiting;
    }
}

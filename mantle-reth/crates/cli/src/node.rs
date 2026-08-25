//! Mantle node type configuration.
//!
//! [`MantleNode`] is a thin wrapper over [`OpNode`] that substitutes the standard
//! [`OpPoolBuilder`] with [`MantlePoolBuilder`], adding Mantle-specific transaction
//! validation on top of the OP stack checks.

use crate::txpool::MantleTransactionValidator;
use mantle_reth_preconf::{
    MantlePreconfServiceBuilder, PreconfAwareValidator, PreconfClassifier, PreconfConfig,
    PreconfPoolListener, PreconfServiceBuilder, PreconfTxSet, bootstrap_whitelist,
    run_whitelist_watcher,
};
use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
use op_alloy_consensus::OpTxEnvelope;
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeComponents, PrimitivesTy, TxTy};
use reth_node_builder::{
    BuilderContext, Node, NodeAdapter, NodeComponentsBuilder,
    components::{ComponentsBuilder, PoolBuilder, PoolBuilderConfigOverrides, TxPoolBuilder},
    node::{FullNodeTypes, NodeTypes},
    rpc::BasicEngineValidatorBuilder,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::{
    OpAddOns, OpConsensusBuilder, OpExecutorBuilder, OpFullNodeTypes, OpNetworkBuilder,
    OpNodeTypes, args::RollupArgs, engine::OpEngineTypes, rpc::OpEthApiBuilder,
};
use reth_optimism_payload_builder::config::{OpBuilderConfig, OpDAConfig, OpGasLimitConfig};
use reth_optimism_primitives::OpPrimitives;
use reth_optimism_storage::OpStorage;
use reth_optimism_txpool::{OpPool, OpPooledTransaction, OpPooledTx};
use reth_provider::CanonStateSubscriptions;
use reth_transaction_pool::{
    CoinbaseTipOrdering, EthPoolTransaction, Pool, TransactionValidationTaskExecutor,
    blobstore::DiskFileBlobStore,
};
use tracing::info;

use std::sync::Arc;

use reth_optimism_node::{OpEngineApiBuilder, OpEngineValidatorBuilder};

/// Type alias for the Mantle transaction pool.
///
/// Same structure as `OpTransactionPool` but the inner validator chain is:
///
/// ```text
/// PreconfAwareValidator<MantleTransactionValidator<OpTransactionValidator<...>>>
/// ```
///
/// The outermost [`PreconfAwareValidator`] is always present in the type;
/// when preconf is not wired up via [`MantlePoolBuilder::with_preconf`], it
/// holds a default-disabled `PreconfConfig` and an empty `PreconfTxSet` —
/// effectively a no-op layer (both replacement guard and gas-ceiling check
/// short-circuit on the empty / disabled state).
pub type MantleTransactionPool<Client, S, Evm, T = OpPooledTransaction> = OpPool<
    Pool<
        TransactionValidationTaskExecutor<
            PreconfAwareValidator<
                MantleTransactionValidator<
                    reth_optimism_txpool::OpTransactionValidator<Client, T, Evm>,
                >,
            >,
        >,
        CoinbaseTipOrdering<T>,
        S,
    >,
>;

/// Mantle pool builder.
///
/// Wraps [`OpPoolBuilder`] but adds [`MantleTransactionValidator`] to reject:
/// - EIP-155 unprotected transactions (legacy type 0 without `chain_id`)
/// - Legacy `MetaTx` transactions (disabled since `MantleEverest`)
///
/// Mantle does not use OP Stack interop, so supervisor/interop logic is omitted.
#[derive(Debug, Clone)]
pub struct MantlePoolBuilder<T = OpPooledTransaction> {
    pool_config_overrides: PoolBuilderConfigOverrides,
    enable_tx_conditional: bool,
    /// Optional preconfirmation wiring. `None` ⇒ preconf disabled; the
    /// validator chain still includes a `PreconfAwareValidator`, but it
    /// receives a default-disabled config and an empty fifo so its checks
    /// short-circuit. `Some(_)` ⇒ both validator and pool listener are
    /// driven by the provided `cfg` / `fifo`.
    preconf: Option<PreconfWiring>,
    _pd: core::marker::PhantomData<T>,
}

/// Preconf wiring bundle held by [`MantlePoolBuilder`]. Constructed and
/// owned by the preconf service builder; the pool builder only needs the
/// shared handles.
#[derive(Debug, Clone)]
pub struct PreconfWiring {
    /// Runtime preconf configuration; cloned into the validator.
    pub cfg: Arc<PreconfConfig>,
    /// Owns the allowlists; the single decider of preconf eligibility. Shared
    /// with the RPC handler and the payload builder — see `PreconfServiceBuilder`
    /// for why there is exactly one instance.
    pub classifier: Arc<PreconfClassifier>,
    /// Commitment fifo shared between validator, RPC handler, and builder.
    pub fifo: Arc<PreconfTxSet>,
    /// Handle to the application-level service builder — used by
    /// [`MantlePoolBuilder::build_pool`] to run [`PreconfServiceBuilder::start`]
    /// immediately after the pool is up, replaying any journaled commitments
    /// into the fifo before any pool listener / canon handler / payload builder
    /// task is spawned. `None` when preconf is disabled on the node (the default
    /// pass-through path).
    pub svc: Option<Arc<PreconfServiceBuilder>>,
}

impl<T> Default for MantlePoolBuilder<T> {
    fn default() -> Self {
        Self {
            pool_config_overrides: Default::default(),
            enable_tx_conditional: false,
            preconf: None,
            _pd: core::marker::PhantomData,
        }
    }
}

impl<T> MantlePoolBuilder<T> {
    /// Sets the `enable_tx_conditional` flag.
    pub fn with_enable_tx_conditional(mut self, enable_tx_conditional: bool) -> Self {
        self.enable_tx_conditional = enable_tx_conditional;
        self
    }

    /// Sets the [`PoolBuilderConfigOverrides`].
    pub fn with_pool_config_overrides(
        mut self,
        pool_config_overrides: PoolBuilderConfigOverrides,
    ) -> Self {
        self.pool_config_overrides = pool_config_overrides;
        self
    }

    /// Enable preconfirmation: thread `cfg`, `fifo`, and the service builder
    /// handle into the validator decoration chain + startup restore + pool
    /// listener spawn. The journal is mandatory and carried by `svc` (read
    /// via [`PreconfServiceBuilder::journal`] where the listener needs it).
    pub fn with_preconf(
        mut self,
        cfg: Arc<PreconfConfig>,
        classifier: Arc<PreconfClassifier>,
        fifo: Arc<PreconfTxSet>,
        svc: Arc<PreconfServiceBuilder>,
    ) -> Self {
        self.preconf = Some(PreconfWiring { cfg, classifier, fifo, svc: Some(svc) });
        self
    }
}

impl<N, T, Evm> PoolBuilder<N, Evm> for MantlePoolBuilder<T>
where
    N: FullNodeTypes<Types: NodeTypes<ChainSpec: OpHardforks>>,
    T: EthPoolTransaction<Consensus = TxTy<N::Types>> + OpPooledTx,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<N::Types>> + Clone + 'static,
    // PreconfPoolListener bridges the pool's consensus tx into `OpTxEnvelope`
    // (and drops Deposit / PostExec). This bound makes the conversion path
    // discoverable to the compiler; for any OP-stack `NodePrimitives` it is
    // satisfied by `impl From<OpTransactionSigned> for OpTxEnvelope` in
    // `op-reth/crates/primitives/src/transaction/signed.rs`.
    OpTxEnvelope: From<TxTy<N::Types>>,
{
    type Pool = MantleTransactionPool<N::Provider, DiskFileBlobStore, Evm, T>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<N>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let blob_store = reth_node_builder::components::create_blob_store(ctx)?;

        // Resolve preconf wiring once for both the validator decoration
        // and the optional listener spawn below. When no caller has wired
        // up preconf (`with_preconf` not invoked), supply default-disabled
        // handles so the `PreconfAwareValidator` layer becomes a cheap
        // pass-through (empty fifo + disabled cfg).
        // No journal handle is pulled out here: nothing in this scope needs one —
        // rotation reaches it through `svc.journal()` below.
        let (preconf_cfg, preconf_classifier, preconf_fifo, preconf_svc) =
            match self.preconf.clone() {
                Some(p) => (p.cfg, p.classifier, p.fifo, p.svc),
                None => {
                    let cfg = PreconfConfig::default();
                    let classifier = Arc::new(PreconfClassifier::from_config(&cfg));
                    let fifo = Arc::new(PreconfTxSet::new(cfg.broadcast_cap));
                    (Arc::new(cfg), classifier, fifo, None)
                }
            };
        // Clones moved into the validator-build closure. The listener
        // (spawned later) takes a separate clone of the same Arcs.
        let validator_cfg = preconf_cfg.clone();
        let validator_classifier = preconf_classifier.clone();
        let validator_fifo = preconf_fifo.clone();

        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .no_eip4844()
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .kzg_settings(ctx.kzg_settings()?)
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(
                    self.pool_config_overrides
                        .additional_validation_tasks
                        .unwrap_or_else(|| ctx.config().txpool.additional_validation_tasks),
                )
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone())
                .map(move |validator| {
                    let op_validator = reth_optimism_txpool::OpTransactionValidator::new(validator)
                        .require_l1_data_gas_fee(!ctx.config().dev.dev);
                    // Enforce `--rpc.txfeecap` for all RPC-submitted txs (op-geth parity);
                    // see MantleTransactionValidator docs for why this can't live in the
                    // inner (upstream) validator. Same config source as `set_tx_fee_cap` above.
                    let mantle_validator = MantleTransactionValidator::new(
                        op_validator,
                        ctx.config().rpc.rpc_tx_fee_cap,
                    );
                    // Wrap with the preconf replacement/gas guard.
                    // `.map` takes `FnMut`; clone the Arcs on every call.
                    PreconfAwareValidator::new(
                        mantle_validator,
                        validator_cfg.clone(),
                        validator_classifier.clone(),
                        validator_fifo.clone(),
                    )
                });

        let final_pool_config = self.pool_config_overrides.apply(ctx.pool_config());

        let inner_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build(blob_store, final_pool_config.clone());

        // Mantle does not use OP interop — filter is always disabled
        let transaction_pool = OpPool::new(inner_pool, false);

        // Load the on-chain allowlists BEFORE anything can classify a
        // transaction, and start tracking updates.
        //
        // Ordering is load-bearing twice over. This cannot live in the binary's
        // `on_node_started` hook, which runs only once the RPC server, payload
        // builder and consensus engine are up: a verdict is frozen at admission,
        // so anything admitted against empty allowlists would stay `NotEligible`
        // for the rest of that tx's life. It must also precede journal restore,
        // which pushes promised envelopes through the validator.
        //
        // Fatal on a code-less address — `build_pool` returns `eyre::Result`, so
        // the launch aborts. An *empty* allowlist is not an error; it is a
        // legitimate governance state (see the whitelist module's "whose decision
        // is it").
        //
        // Gated on `enabled` because preconf is sequencer-only. Both entry points
        // also self-gate via `wants_whitelist` (which is what covers
        // `--preconf.all`); the check here keeps a verifier from spawning a
        // critical task that would only return immediately.
        if preconf_cfg.enabled {
            bootstrap_whitelist(ctx.provider(), &preconf_cfg, &preconf_classifier)
                .map_err(|e| eyre::eyre!("preconf whitelist bootstrap: {e}"))?;

            let watcher_cfg = preconf_cfg.clone();
            let watcher_classifier = preconf_classifier.clone();
            let watcher_provider = ctx.provider().clone();
            ctx.task_executor().spawn_critical_task(
                "mantle-preconf-whitelist-watcher",
                run_whitelist_watcher(watcher_provider, watcher_cfg, watcher_classifier),
            );
        }

        // Run journal restore before any background pool task starts consuming
        // events. Two ordering constraints:
        // - Must run before `spawn_maintenance_tasks` (which spawns reth's local-tx backup loader)
        //   so the loader and the restore path don't race on the pool mutex.
        // - Must run before the pool listener is spawned so the restore helper's fifo pushes are
        //   attributed to the restart path, not to a fresh RPC submission.
        if let Some(svc) = preconf_svc.as_ref() {
            use mantle_reth_preconf::{ProviderChainView, RestorePoolAdapter};
            let adapter = RestorePoolAdapter::<_, T, TxTy<N::Types>>::new(transaction_pool.clone());
            // The provider answers restore's chain question — whether a
            // commitment whose nonce is gone is the transaction that consumed it.
            // The pool cannot: its only chain-derived signal is the account
            // nonce. See `CommitmentChainView`.
            let chain_view = ProviderChainView::new(ctx.provider().clone());
            svc.start(&adapter, &chain_view)
                .await
                .map_err(|e| eyre::eyre!("preconf service start: {e:?}"))?;
            info!(target: "reth::cli", "Mantle preconf service builder started (restore + wire)");
        }

        reth_node_builder::components::spawn_maintenance_tasks(
            ctx,
            transaction_pool.clone(),
            &final_pool_config,
        )?;

        // Spawn the pool listener only when preconf is actually enabled — when
        // `with_preconf` was not called, `preconf_cfg.enabled` is false and the
        // listener would sit idle skipping every tx for want of a verdict.
        // Skipping the spawn saves a task.
        if preconf_cfg.enabled {
            // No journal handle: the listener asks the classifier instead — see
            // `PreconfPoolListener::run`.
            let listener = PreconfPoolListener::new(
                transaction_pool.clone(),
                preconf_cfg.clone(),
                preconf_classifier.clone(),
                preconf_fifo.clone(),
            );
            ctx.task_executor().spawn_critical_task("mantle-preconf-pool-listener", listener.run());
            info!(target: "reth::cli", "Mantle preconf pool listener spawned");
        }

        if self.enable_tx_conditional {
            let chain_events = ctx.provider().canonical_state_stream();
            ctx.task_executor().spawn_critical_task(
                "Mantle txpool conditional maintenance task",
                reth_optimism_txpool::maintain::maintain_transaction_pool_conditional_future(
                    transaction_pool.clone(),
                    chain_events,
                ),
            );
        }

        info!(target: "reth::cli", "Mantle transaction pool initialized");

        Ok(transaction_pool)
    }
}

/// Type alias for the Mantle node component builder.
///
/// The outer type is always [`MantlePreconfServiceBuilder`] so the
/// `ComponentsBuilder` type stays stable across enable/disable modes.
/// At spawn time (`spawn_payload_builder_service`) the builder
/// inspects `cfg.enabled`:
///
/// - `true` (preconf enabled) — spawns the fork's [`PreconfPayloadBuilder`] with the shared `(cfg,
///   fifo)` pair, wire publisher, select! loop, sweep-ticker quota, etc.
/// - `false` (preconf disabled) — delegates to reth's upstream
///   [`BasicPayloadServiceBuilder`]`<`[`OpPayloadBuilder`]`>`, giving byte-identical behavior to
///   vanilla op-reth. Provides a rollback path if a fatal preconf-fork bug is discovered in
///   production — omit `--preconf.enable` and the fork is bypassed at runtime.
///
/// [`PreconfPayloadBuilder`]: mantle_reth_preconf::builder::payload_builder::PreconfPayloadBuilder
/// [`BasicPayloadServiceBuilder`]: reth_node_builder::components::BasicPayloadServiceBuilder
/// [`OpPayloadBuilder`]: reth_optimism_node::node::OpPayloadBuilder
pub type MantleNodeComponentBuilder<N> = ComponentsBuilder<
    N,
    MantlePoolBuilder,
    MantlePreconfServiceBuilder<OpPrimitives>,
    OpNetworkBuilder,
    OpExecutorBuilder,
    OpConsensusBuilder,
>;

/// Mantle node type configuration.
///
/// A newtype wrapper over [`OpNode`](reth_optimism_node::OpNode) that replaces
/// [`OpPoolBuilder`] with [`MantlePoolBuilder`] to enforce Mantle-specific
/// transaction-pool validation rules.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MantleNode {
    /// Underlying OP node configuration.
    pub op_node: reth_optimism_node::OpNode,
    /// Optional preconfirmation subsystem handle. `None` ⇒ preconf
    /// disabled (default); the node behaves exactly like the
    /// underlying OP node. `Some` ⇒ the validator chain, pool listener,
    /// canonical-state cleaner, and RPC handler all get wired up to
    /// the shared `cfg` / `fifo` held by the builder.
    pub preconf: Option<Arc<PreconfServiceBuilder>>,
}

impl MantleNode {
    /// Creates a new [`MantleNode`] with the given rollup arguments.
    pub fn new(args: RollupArgs) -> Self {
        Self { op_node: reth_optimism_node::OpNode::new(args), preconf: None }
    }

    /// Configure the data availability configuration for the Mantle builder.
    pub fn with_da_config(mut self, da_config: OpDAConfig) -> Self {
        self.op_node = self.op_node.with_da_config(da_config);
        self
    }

    /// Configure the gas limit configuration for the Mantle builder.
    pub fn with_gas_limit_config(mut self, gas_limit_config: OpGasLimitConfig) -> Self {
        self.op_node = self.op_node.with_gas_limit_config(gas_limit_config);
        self
    }

    /// Enable the preconfirmation subsystem on this node. The provided
    /// builder owns the shared `cfg` / `fifo` handles; the same handles
    /// thread into the validator chain, the pool listener (spawned by
    /// [`MantlePoolBuilder::build_pool`]), the canonical-state handler
    /// (spawned in [`MantleNode::add_ons`]), and the RPC handler
    /// (injected into [`MantleRpcExt`]).
    pub fn with_preconf(mut self, builder: PreconfServiceBuilder) -> Self {
        self.preconf = Some(Arc::new(builder));
        self
    }

    /// Returns the component builder for this Mantle node.
    pub fn components<N>(&self) -> MantleNodeComponentBuilder<N>
    where
        N: FullNodeTypes<Types: OpNodeTypes>,
    {
        let args = &self.op_node.args;

        // Activate pool-side preconf wiring (validator decoration + spawned
        // pool listener inside `build_pool`) if the node was configured
        // for preconf. Otherwise the pool builder stays in its default
        // pass-through state.
        let mut pool_builder =
            MantlePoolBuilder::default().with_enable_tx_conditional(args.enable_tx_conditional);
        if let Some(p) = &self.preconf {
            pool_builder = pool_builder.with_preconf(
                p.cfg().clone(),
                p.classifier().clone(),
                p.fifo().clone(),
                p.clone(),
            );
        }

        // Construct the preconf-aware payload service. When
        // `self.preconf == None` we still supply a default-empty
        // `(cfg, fifo)` — no whitelist, no broadcast events. The fork's
        // select! loop then sits idle waiting for cancel, and the
        // block seals at CL `getPayload` time.
        let builder_config = OpBuilderConfig::new_with_sdm(
            self.op_node.da_config.clone(),
            self.op_node.gas_limit_config.clone(),
            args.sdm_enabled,
        );
        let (cfg, classifier, fifo) = if let Some(p) = &self.preconf {
            (p.cfg().clone(), p.classifier().clone(), p.fifo().clone())
        } else {
            // Disabled path: default-empty cfg / classifier / fifo (no allowlists,
            // no events). Built by hand rather than via `PreconfServiceBuilder`,
            // which would open the journal file and needs a resolved
            // `journal_path` the default config deliberately leaves unset. Same
            // shape as the disabled path in `build_pool` above.
            let cfg = PreconfConfig::default();
            let classifier = Arc::new(PreconfClassifier::from_config(&cfg));
            let fifo = Arc::new(PreconfTxSet::new(cfg.broadcast_cap));
            (Arc::new(cfg), classifier, fifo)
        };
        let payload_service =
            MantlePreconfServiceBuilder::<OpPrimitives>::new(cfg, classifier, fifo, builder_config);

        ComponentsBuilder::default()
            .node_types::<N>()
            .executor(OpExecutorBuilder::default().with_sdm_enabled(args.sdm_enabled))
            .pool(pool_builder)
            .payload(payload_service)
            .network(OpNetworkBuilder::new(args.disable_txpool_gossip, !args.discovery_v4))
            .consensus(OpConsensusBuilder::default())
    }
}

impl NodeTypes for MantleNode {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;
    type Storage = OpStorage;
    type Payload = OpEngineTypes;
}

impl<N> Node<N> for MantleNode
where
    N: FullNodeTypes<Types: OpFullNodeTypes + OpNodeTypes>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        MantlePoolBuilder,
        MantlePreconfServiceBuilder<OpPrimitives>,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >;

    type AddOns = OpAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        OpEthApiBuilder,
        OpEngineValidatorBuilder,
        OpEngineApiBuilder<OpEngineValidatorBuilder>,
        BasicEngineValidatorBuilder<OpEngineValidatorBuilder>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.components()
    }

    fn add_ons(&self) -> Self::AddOns {
        let sequencer_url = self.op_node.args.sequencer.clone();
        let preconf = self.preconf.clone();
        let mut add_ons: Self::AddOns = self.op_node.add_ons_builder().build();
        add_ons = add_ons.extend_rpc_modules(move |ctx| {
            // Build SequencerClient if a sequencer URL is configured.
            // SequencerClient::new is async; use block_in_place since we're inside tokio.
            let sequencer_client = sequencer_url
                .map(|url| {
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(reth_optimism_rpc::SequencerClient::new(url))
                    })
                })
                .transpose()
                .map_err(|e| eyre::eyre!("failed to create SequencerClient: {e}"))?;

            // If preconf is enabled, spin up the canonical-state handler and
            // build the RPC handler. The pool listener is already spawned
            // by `MantlePoolBuilder::build_pool` when its `with_preconf`
            // was called during `components()`.
            let preconf_handler: Option<Arc<dyn mantle_reth_rpc_ext::DynPreconfHandler>> =
                preconf.as_ref().map(|svc| {
                    let canon =
                        svc.canon_handler(ctx.node().provider().clone(), ctx.node().pool().clone());
                    ctx.node()
                        .task_executor()
                        .spawn_critical_task("mantle-preconf-canon-handler", canon.run());
                    info!(target: "reth::cli", "Mantle preconf canonical-state handler spawned");

                    // If persistence is on, drive the periodic rotation
                    // loop under the reth `TaskManager`'s graceful-
                    // shutdown protocol. Holding the `GracefulShutdownGuard`
                    // across `run_rejournal_loop`'s final rotate makes the
                    // `TaskManager` wait for the last on-disk write to
                    // finish before returning from process shutdown, so
                    // sealed hashes reported by the canon handler on the
                    // way down are dropped from the journal file before
                    // the node exits.
                    // Journal is mandatory (stage2), so no `Option` dance here.
                    let journal = svc.journal().clone();
                    // Rotation asks the classifier which commitments are still
                    // tracked — it owns that answer.
                    let rotate_classifier = svc.classifier().clone();
                    let interval = svc.cfg().rejournal_interval;
                    ctx.node().task_executor().spawn_critical_with_graceful_shutdown_signal(
                        "mantle-preconf-rejournal-loop",
                        move |signal| async move {
                            // `signal` (`GracefulShutdown`) resolves to
                            // a `GracefulShutdownGuard` when the reth
                            // `TaskManager` begins shutdown. Passing it
                            // as the shutdown future to
                            // `run_rejournal_loop` — whose `T` type
                            // parameter carries the guard through the
                            // final-rotate step — keeps the guard alive
                            // until the last on-disk write finishes,
                            // so the process only exits after the
                            // journal file has been closed cleanly.
                            let guard = mantle_reth_preconf::run_rejournal_loop(
                                journal,
                                rotate_classifier,
                                interval,
                                signal,
                            )
                            .await;
                            // Explicit drop for clarity; the guard is
                            // released here, letting `TaskManager`'s
                            // outstanding-tasks counter reach zero.
                            drop(guard);
                        },
                    );
                    info!(target: "reth::cli", "Mantle preconf journal rotation loop spawned");

                    let handler =
                        svc.rpc_handler(ctx.node().pool().clone(), ctx.node().provider().clone());
                    Arc::new(handler) as Arc<dyn mantle_reth_rpc_ext::DynPreconfHandler>
                });

            let mantle_ext = MantleRpcExt::new(
                ctx.node().provider().clone(),
                Arc::new(ctx.registry.eth_api().clone()),
                sequencer_client,
                preconf_handler,
            );
            // `replace_configured`, not `merge_configured`: this module now also serves
            // `eth_simulateV1`, which the standard `eth_` namespace already registers. Merging a
            // duplicate method name fails, so remove-then-add to override it. The other methods in
            // this module are new names and are unaffected by the choice.
            ctx.modules.replace_configured(mantle_ext.into_rpc())?;
            info!(target: "reth::cli", "Mantle RPC extensions registered");

            Ok(())
        });
        add_ons
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantle_reth_preconf::PreconfConfig;
    use reth_optimism_node::args::RollupArgs;

    fn default_args() -> RollupArgs {
        RollupArgs::default()
    }

    #[test]
    fn new_starts_with_preconf_disabled() {
        // Default constructor: preconf opt-in, must be None at entry. Regression
        // guard for the "MantleNode behaves exactly like OpNode when preconf is
        // not configured" contract.
        let node = MantleNode::new(default_args());
        assert!(node.preconf.is_none());
    }

    /// Build a service builder with a fresh journal under a temp dir.
    /// The journal is mandatory, so construction opens a real file.
    async fn test_svc(dir: &tempfile::TempDir) -> PreconfServiceBuilder {
        let cfg = PreconfConfig {
            journal_path: Some(dir.path().join("preconf.jsonl")),
            ..PreconfConfig::default()
        };
        PreconfServiceBuilder::from_config(cfg).await.expect("default validates + journal opens")
    }

    #[tokio::test]
    async fn with_preconf_attaches_service_builder() {
        // `with_preconf` must store the service builder reachable through
        // `self.preconf` so that `components()` / `add_ons()` can thread the
        // same Arc<cfg, fifo, journal> handles into all consumers.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_svc(&dir).await;
        let cfg_ptr = svc.cfg().clone();
        let fifo_ptr = svc.fifo().clone();
        let classifier_ptr = svc.classifier().clone();

        let node = MantleNode::new(default_args()).with_preconf(svc);
        let stored = node.preconf.as_ref().expect("with_preconf must attach handle");
        // Pointer-equal: no clone-and-rebuild — same Arc instance.
        assert!(Arc::ptr_eq(stored.cfg(), &cfg_ptr));
        assert!(Arc::ptr_eq(stored.fifo(), &fifo_ptr));
        assert!(Arc::ptr_eq(stored.classifier(), &classifier_ptr));
    }

    #[tokio::test]
    async fn with_preconf_replaces_previous_builder() {
        // Last-wins semantics. Documents the builder contract — important
        // because the call site in main.rs writes `node = node.with_preconf(...)`
        // unconditionally within the `Some(cfg)` branch.
        let dir1 = tempfile::TempDir::new().unwrap();
        let dir2 = tempfile::TempDir::new().unwrap();
        let svc1 = test_svc(&dir1).await;
        let svc2 = test_svc(&dir2).await;
        let fifo2 = svc2.fifo().clone();

        let node = MantleNode::new(default_args()).with_preconf(svc1).with_preconf(svc2);
        let stored = node.preconf.as_ref().expect("attached");
        // svc1's handles dropped; stored points at svc2's.
        assert!(Arc::ptr_eq(stored.fifo(), &fifo2));
    }
}

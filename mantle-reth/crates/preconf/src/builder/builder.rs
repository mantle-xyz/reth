// The file shares its name with the containing module by design — the
// loop is the canonical "builder" inside the `builder/` submodule. The
// alternative would be an awkward `runner.rs` / `core.rs` rename for
// the central module.
#![allow(clippy::module_inception)]

//! The preconf payload-builder's inner dispatch loop.
//!
//! [`BuilderLoop`] owns a `tokio::select!` over four async sources:
//!
//! - the fifo's broadcast notifier (one new preconf tx ready to apply)
//! - the broadcast's `Lagged` error (drop signal — reconcile via
//!   [`PreconfTxSet::snapshot`])
//! - a sweep ticker (gives the future pool-sweep arm a chance to land
//!   normal-path work; for now we use the ticker only to enforce the
//!   has-pending drain barrier described below)
//! - a cancel signal ([`JobCancel`])
//!
//! The loop is **decoupled from the concrete EVM stack** via the
//! [`PreconfTxApplier`] trait. Production code wires the trait to a reth
//! `BlockBuilder` (calling [`apply_preconf_tx`]); tests pass a stub so
//! the dispatch / fifo state-machine logic can be exercised without
//! standing up a chain spec, EVM, and state DB.
//!
//! Invariants enforced here:
//!
//! - **Dedup**: a hash that already appeared in
//!   [`BuilderTxTracker::committed`] or `excluded` is short-circuited
//!   before any fifo / EVM work.
//! - **Status gate**: only `Waiting` entries are applied. `Timeout` /
//!   `Success` / `Failed` are recorded as excluded and skipped.
//! - **Pre-apply deadline**: if `entry.inserted_at.elapsed() +
//!   safety_margin >= preconf_timeout`, the tx is *not* applied — the
//!   loop flips the fifo entry to `Timeout` and cancels any responder
//!   itself. Without this, a slow start could land the tx in the
//!   block but leave the client believing it timed out.
//! - **Has-pending Dekker barrier**: every sweep tick clears the
//!   fifo's `has_pending` flag, then drains the broadcast channel with
//!   `try_recv` to absorb any event published between flag-clear and
//!   sweep handoff. Otherwise a sweep would briefly believe "no
//!   preconf work" while a freshly-pushed entry is in flight.
//! - **Responder ownership**: on every terminal path (success, failure,
//!   deadline skip) the loop calls `take_responder` or
//!   `cancel_responder` exactly once. The fifo guarantees take-once
//!   semantics, so the RPC handler can never see a stale send.
//!
//! [`apply_preconf_tx`]: crate::apply::apply_preconf_tx

use std::sync::Arc;
use std::time::Duration;

use alloy_consensus::TxEnvelope;
use alloy_primitives::TxHash;
use tokio::sync::broadcast;
use tracing::{debug, trace, warn};

use crate::{
    PreconfConfig, PreconfTxSet,
    builder::{cancel::JobCancel, event::BuilderEvent, tx_tracker::BuilderTxTracker},
    types::{PreconfError, PreconfReceipt, PreconfStatus},
};

// ─── Applier abstraction ────────────────────────────────────────────────────

/// Minimal interface the builder loop needs from "something that can
/// execute one preconf transaction and produce a receipt".
///
/// Production impls of this trait fall into two families:
///
/// - **Promise-only** ([`PromiseApplier`]): fabricates a synthetic
///   success receipt without running the EVM. Cheap; lets clients
///   observe the commitment immediately. The real execution outcome
///   surfaces later via `eth_getTransactionReceipt` against the
///   canonical chain.
/// - **EVM-backed** (future work): runs the tx against the latest
///   committed state (or, ideally, the in-flight block builder's
///   state) and produces a receipt that includes real revert
///   reasons, gas usage, and logs. Outline:
///   1. Hold `evm_config: E where E: reth_evm::ConfigureEvm` and a
///      [`reth_storage_api::StateProviderFactory`].
///   2. On each `apply` call: fetch latest state, construct a
///      `State<CacheDB<StateProviderBox>>`, call
///      `evm_config.builder_for_next_block(db, parent, attrs)?` to
///      get a [`reth_evm::execute::BlockBuilder`].
///   3. Pass the resulting builder + tx + `block_height` to
///      [`apply_preconf_tx`](crate::apply::apply_preconf_tx).
///   4. Discard the builder (we don't seal it — the inner OP payload
///      builder owns block assembly).
///
/// The trait is sync to match the underlying `apply_preconf_tx`
/// function — the EVM call itself is CPU-bound and does not yield. The
/// loop awaits I/O (fifo mutex, responder channel) around the apply
/// itself, not within it.
pub trait PreconfTxApplier: Send {
    /// Execute `tx` against the in-flight block, reporting its receipt
    /// or the EVM rejection reason.
    fn apply(
        &mut self,
        tx: Arc<TxEnvelope>,
        block_height: u64,
    ) -> Result<PreconfReceipt, PreconfError>;
}

// Blanket impl so a boxed dyn applier still satisfies the trait. This
// is what lets [`PreconfApplierFactory`] (alias around `dyn Fn ->
// Box<dyn PreconfTxApplier + Send>`) hand the result straight into
// [`BuilderLoop::new`] without an extra wrapping layer.
impl<T: PreconfTxApplier + ?Sized> PreconfTxApplier for Box<T> {
    fn apply(
        &mut self,
        tx: Arc<TxEnvelope>,
        block_height: u64,
    ) -> Result<PreconfReceipt, PreconfError> {
        (**self).apply(tx, block_height)
    }
}

/// Type-erased applier suitable for boxing into a factory.
pub type BoxedPreconfTxApplier = Box<dyn PreconfTxApplier + Send + 'static>;

/// Factory that produces one fresh [`PreconfTxApplier`] per payload
/// job. Each preconf payload job spawns its own [`BuilderLoop`] task
/// holding one applier instance; the factory is invoked once per
/// `PreconfPayloadJob::new` call.
///
/// Cloning the `Arc` is cheap; the closure itself must be `Send + Sync`
/// because the generator clones it into every produced job.
pub type PreconfApplierFactory = Arc<dyn Fn() -> BoxedPreconfTxApplier + Send + Sync>;

/// Default applier factory — produces a [`PromiseApplier`] per slot.
/// Returned by [`default_applier_factory`] so callers don't need to
/// know the type-erased boxing syntax.
pub fn default_applier_factory() -> PreconfApplierFactory {
    Arc::new(|| Box::new(PromiseApplier))
}

// ─── Promise Applier ────────────────────────────────────────────────────────

/// A [`PreconfTxApplier`] that fabricates always-success receipts without
/// running the EVM.
///
/// Semantics: the receipt is a **commitment promise** — "this transaction
/// will be included in the in-flight block" — not the result of executing
/// the transaction. The client SDK is expected to later poll
/// `eth_getTransactionReceipt` for the authoritative execution outcome.
///
/// Fields the synthetic receipt carries:
///
/// - `tx_hash`: the actual hash (from the [`TxEnvelope`] handed to
///   `apply`).
/// - `block_height`: the value supplied to [`apply`](Self::apply), which
///   is the predicted height the [`BuilderLoop`] was constructed with.
/// - `status`: `true`. The pool validator already accepted this tx, so
///   "promise of inclusion" is best expressed as success. Real
///   reverts surface via the canonical receipt path.
/// - `logs`: empty. We did not execute; we have no logs to report.
/// - `gas_used`: the tx's `gas_limit`. This is a worst-case stand-in
///   that lets clients budget downstream gas accounting without
///   waiting for execution.
/// - `reason` / `revert_data`: empty.
///
/// This is the integration intermediate: it completes the loop's
/// structural wiring (so [`PreconfPayloadJob`] can spawn a working
/// loop and clients stop seeing 200ms timeouts), without requiring the
/// deep refactor that an EVM-backed applier would.
///
/// [`PreconfPayloadJob`]: crate::builder::job::PreconfPayloadJob
#[derive(Debug, Clone, Copy, Default)]
pub struct PromiseApplier;

impl PreconfTxApplier for PromiseApplier {
    fn apply(
        &mut self,
        tx: Arc<TxEnvelope>,
        block_height: u64,
    ) -> Result<PreconfReceipt, PreconfError> {
        use alloy_consensus::Transaction as _;
        Ok(PreconfReceipt {
            tx_hash: *tx.tx_hash(),
            block_height,
            status: true,
            logs: Vec::new(),
            gas_used: tx.gas_limit(),
            reason: String::new(),
            revert_data: alloy_primitives::Bytes::new(),
        })
    }
}

// ─── Loop ───────────────────────────────────────────────────────────────────

/// Per-payload-job dispatch loop.
///
/// One [`BuilderLoop`] is constructed per `PayloadJob`. It owns its
/// applier and runs until either the cancel signal flips or the fifo's
/// broadcast sender is dropped (node shutdown).
pub struct BuilderLoop<A> {
    applier: A,
    fifo: Arc<PreconfTxSet>,
    cfg: Arc<PreconfConfig>,
    cancel: JobCancel,
    /// Predicted L2 block height for the in-flight block. Stamped on
    /// every produced [`PreconfReceipt`] so RPC clients see the slot
    /// number the commitment is good for.
    block_height: u64,
    /// Cross-iteration dedup. Owned (not shared) — one tracker per
    /// payload job. Reset by dropping the loop on job teardown.
    tx_tracker: BuilderTxTracker,
}

impl<A> std::fmt::Debug for BuilderLoop<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuilderLoop")
            .field("cfg", &self.cfg)
            .field("block_height", &self.block_height)
            .field("committed_len", &self.tx_tracker.committed_len())
            .field("excluded_len", &self.tx_tracker.excluded_len())
            .finish_non_exhaustive()
    }
}

impl<A: PreconfTxApplier> BuilderLoop<A> {
    /// Construct a fresh loop. `block_height` is the predicted L2 block
    /// number for the in-flight payload (used only for stamping
    /// receipts).
    pub fn new(
        applier: A,
        fifo: Arc<PreconfTxSet>,
        cfg: Arc<PreconfConfig>,
        cancel: JobCancel,
        block_height: u64,
    ) -> Self {
        Self { applier, fifo, cfg, cancel, block_height, tx_tracker: BuilderTxTracker::new() }
    }

    /// Drive the dispatch loop to completion. Returns when [`JobCancel`]
    /// flips or the fifo's broadcast sender drops.
    pub async fn run(mut self) {
        let mut fifo_rx = self.fifo.subscribe();
        let mut sweep = tokio::time::interval(self.cfg.sweep_interval);
        // Sweep is a barrier hint, not a scheduled task; if we fall
        // behind we want to skip catch-up ticks instead of bursting.
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            let event = tokio::select! {
                // `biased` — drain cancel first so a torn-down job
                // doesn't perform one more apply between cancel and
                // the next yield point.
                biased;
                () = self.cancel.wait() => BuilderEvent::Cancel,
                recv = fifo_rx.recv() => match recv {
                    Ok(hash) => BuilderEvent::Preconf(hash),
                    Err(broadcast::error::RecvError::Lagged(_)) => BuilderEvent::BroadcastLagged,
                    Err(broadcast::error::RecvError::Closed) => {
                        debug!(target: "mantle::preconf::builder", "fifo broadcast closed; exiting loop");
                        return;
                    }
                },
                _ = sweep.tick() => BuilderEvent::SweepTick,
            };

            trace!(target: "mantle::preconf::builder", event = event.label(), "loop iteration");

            match event {
                BuilderEvent::Cancel => return,
                BuilderEvent::Preconf(hash) => self.handle_preconf(hash).await,
                BuilderEvent::BroadcastLagged => self.reconcile_lagged().await,
                BuilderEvent::SweepTick => self.handle_sweep_tick(&mut fifo_rx).await,
            }
        }
    }

    /// Apply a single preconf-eligible tx end-to-end: dedup check,
    /// status gate, pre-apply deadline check, EVM apply, fifo state
    /// transition, responder send.
    async fn handle_preconf(&mut self, hash: TxHash) {
        if self.tx_tracker.contains(&hash) {
            trace!(target: "mantle::preconf::builder", ?hash, "dedup hit; skipping");
            return;
        }

        let Some(entry) = self.fifo.find_by_hash(&hash).await else {
            trace!(target: "mantle::preconf::builder", ?hash, "no fifo entry; skipping");
            return;
        };

        if entry.status != PreconfStatus::Waiting {
            // Already terminal — either a prior iteration finished it
            // or the RPC timeout flipped it. Record so we don't try
            // again on the next broadcast event.
            self.tx_tracker.record_excluded(hash);
            return;
        }

        if self.is_past_deadline(&entry.inserted_at.elapsed()) {
            debug!(
                target: "mantle::preconf::builder",
                ?hash,
                elapsed_ms = entry.inserted_at.elapsed().as_millis() as u64,
                "pre-apply deadline passed; aborting"
            );
            let _ = self.fifo.mark_timeout(&hash).await;
            self.fifo
                .cancel_responder(
                    &hash,
                    PreconfError::Timeout {
                        timeout_ms: self.cfg.preconf_timeout.as_millis() as u64,
                    },
                )
                .await;
            self.tx_tracker.record_excluded(hash);
            return;
        }

        let Some(tx) = self.fifo.get_tx(&hash).await else {
            warn!(target: "mantle::preconf::builder", ?hash, "fifo had entry but no tx; skipping");
            return;
        };

        match self.applier.apply(tx, self.block_height) {
            Ok(receipt) => {
                self.tx_tracker.record_committed(hash);
                let mark_result = if receipt.status {
                    self.fifo.mark_succeeded(&hash).await
                } else {
                    self.fifo.mark_failed(&hash).await
                };
                if let Err(e) = mark_result {
                    // Lost a race with clean_timeout / cancel — entry
                    // already gone or in a non-Waiting state. Log and
                    // continue; the responder still gets the receipt.
                    trace!(target: "mantle::preconf::builder", ?hash, ?e, "mark transition lost race");
                }
                if let Some(resp) = self.fifo.take_responder(&hash).await {
                    let _ = resp.send(Ok(receipt));
                }
            }
            Err(err) => {
                warn!(target: "mantle::preconf::builder", ?hash, ?err, "EVM rejected preconf tx");
                self.tx_tracker.record_excluded(hash);
                let _ = self.fifo.mark_failed(&hash).await;
                if let Some(resp) = self.fifo.take_responder(&hash).await {
                    let _ = resp.send(Err(err));
                }
            }
        }
    }

    /// Drain every still-`Waiting` hash from the fifo snapshot. Called
    /// when the broadcast channel signals lag, which means at least one
    /// `Preconf(hash)` notification was dropped and the loop can no
    /// longer trust the event stream alone.
    async fn reconcile_lagged(&mut self) {
        warn!(target: "mantle::preconf::builder", "broadcast lagged; reconciling via fifo snapshot");
        for hash in self.fifo.snapshot().await {
            if self.tx_tracker.contains(&hash) {
                continue;
            }
            self.handle_preconf(hash).await;
        }
    }

    /// Sweep-tick handler. Acts as the safety net for the broadcast
    /// channel: if the fifo's `has_pending` flag is set we know there
    /// is at least one new entry whose `Preconf(hash)` notification we
    /// might have missed (broadcast subscribers created after the
    /// send, slow drain, channel under pressure). We:
    ///
    /// 1. Clear the flag.
    /// 2. Drain whatever the broadcast channel still has via `try_recv`
    ///    (Dekker barrier — picks up events published between the flag
    ///    clear above and this drain).
    /// 3. Walk `snapshot()` to catch entries we definitely missed.
    ///
    /// The actual pool-sweep arm (pulling `best_transactions` into the
    /// in-flight block) is wired into the production applier in the
    /// next phase. The sweep tick is conceptually two interleaved
    /// duties — preconf safety net + normal-tx sweep — and only the
    /// first is wired today.
    async fn handle_sweep_tick(&mut self, fifo_rx: &mut broadcast::Receiver<TxHash>) {
        if !self.fifo.has_pending_unprocessed() {
            return;
        }
        self.fifo.clear_pending_flag();
        // Drain broadcast first — cheap and avoids re-walking the
        // snapshot for entries we've already gotten notifications for.
        loop {
            match fifo_rx.try_recv() {
                Ok(hash) => self.handle_preconf(hash).await,
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    self.reconcile_lagged().await;
                    return;
                }
                Err(broadcast::error::TryRecvError::Closed) => return,
            }
        }
        // Catch anything the broadcast missed (e.g. subscription that
        // started after the event was sent).
        for hash in self.fifo.snapshot().await {
            if !self.tx_tracker.contains(&hash) {
                self.handle_preconf(hash).await;
            }
        }
    }

    /// `true` when `elapsed + safety_margin >= preconf_timeout` — at
    /// that point applying the tx would leave the client believing it
    /// timed out while the receipt nonetheless lands on chain.
    ///
    /// Safety margin is `preconf_timeout / 5` (e.g. 40ms for the
    /// 200ms default) — enough to bracket typical EVM + DB latency
    /// without being so generous that we drop healthy work.
    fn is_past_deadline(&self, elapsed: &Duration) -> bool {
        let margin = self.cfg.preconf_timeout / 5;
        *elapsed + margin >= self.cfg.preconf_timeout
    }
}

#[cfg(test)]
mod tests;

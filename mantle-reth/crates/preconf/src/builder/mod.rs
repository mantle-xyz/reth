//! Mantle preconf-aware OP payload builder — forked from upstream's
//! `OpPayloadBuilder`. The fork is required so preconf-tx apply and the
//! ordinary block-build path can share the same in-flight `State<DB>`;
//! a wrapper cannot deliver "RPC receipt bytes == sealed receipt bytes"
//! because the applier would have no state channel to the inner OP
//! builder.
//!
//! Module layout:
//!
//! - [`payload_builder`] — the fork itself: struct + `async build_payload` (deposits → sequencer
//!   txs → preconf select! loop → SDM post-exec → finalize).
//! - `dispatch` — the select! loop's per-event fifo state machine (`apply_one_preconf`) plus the
//!   same-sender cascade state (`LoopState`), separated so the state-machine invariants are
//!   unit-testable without standing up the full EVM stack. Block-capacity admission
//!   (`preconf_admission` / `admit_and_dispatch`) lives in [`payload_builder`] where the block
//!   state (`info` / DA limits) is owned.
//! - [`payload_job`] — `PreconfPayloadJob` implementing reth's
//!   [`PayloadJob`](reth_payload_builder::PayloadJob) trait.
//! - [`payload_job_generator`] — `PreconfPayloadJobGenerator` implementing reth's
//!   [`PayloadJobGenerator`](reth_payload_builder::PayloadJobGenerator) trait.
//! - [`cancel`] — `JobCancel`, the async-aware cancel signal shared between the job and the spawned
//!   build task.

pub mod cancel;
pub(crate) mod dispatch;
pub mod payload_builder;
pub mod payload_job;
pub mod payload_job_generator;

pub use cancel::JobCancel;
pub use payload_builder::PreconfPayloadBuilder;
pub use payload_job::{PreconfPayloadJob, ResolvePayloadFuture};
pub use payload_job_generator::PreconfPayloadJobGenerator;

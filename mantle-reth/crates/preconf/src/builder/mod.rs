//! Mantle preconf-aware OP payload builder — forked from upstream's
//! `OpPayloadBuilder` (see `docs/design/preconf-dev-plan.md` §P5f for
//! the rationale).
//!
//! Module layout:
//!
//! - [`payload_builder`] — the fork itself: struct + `async
//!   build_payload` (deposits → sequencer txs → preconf select! loop
//!   → SDM post-exec → finalize).
//! - [`dispatch`] — the select! loop's per-event helpers
//!   (`apply_one_preconf` + `reconcile_lagged`), separated so the
//!   state-machine invariants are unit-testable without standing up
//!   the full EVM stack.
//! - [`payload_job`] — `PreconfPayloadJob` implementing reth's
//!   [`PayloadJob`](reth_payload_builder::PayloadJob) trait.
//! - [`payload_job_generator`] — `PreconfPayloadJobGenerator`
//!   implementing reth's [`PayloadJobGenerator`](reth_payload_builder::PayloadJobGenerator) trait.
//! - [`cancel`] — `JobCancel`, the async-aware cancel signal shared
//!   between the job and the spawned build task.

pub mod cancel;
pub(crate) mod dispatch;
pub mod payload_builder;
pub mod payload_job;
pub mod payload_job_generator;

pub use cancel::JobCancel;
pub use payload_builder::PreconfPayloadBuilder;
pub use payload_job::{PreconfPayloadJob, ResolvePayloadFuture};
pub use payload_job_generator::PreconfPayloadJobGenerator;

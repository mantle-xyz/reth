//! Payload builder helpers — primitives used by the preconf-aware payload
//! job's main loop.
//!
//! This module is intentionally narrow: it houses small, side-effect-free
//! data structures (`BuilderTxTracker`, future state-carry helpers, ...) so
//! they can be unit-tested in isolation without spinning up a full reth
//! payload-building stack. The `PreconfPayloadJob` itself lives elsewhere
//! and consumes these primitives.

pub mod builder;
pub mod cancel;
pub mod event;
pub mod generator;
pub mod job;
pub mod state_carry;
pub mod tx_tracker;

pub use builder::{BuilderLoop, PreconfTxApplier, PromiseApplier};
pub use cancel::JobCancel;
pub use event::BuilderEvent;
pub use generator::PreconfPayloadJobGenerator;
pub use job::PreconfPayloadJob;
pub use state_carry::CarriedState;
pub use tx_tracker::BuilderTxTracker;

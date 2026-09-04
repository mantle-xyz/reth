//! The producer side, assembled.
//!
//! Three things have to exist together for a node to publish slices: the
//! endpoint, the loop that accepts subscribers on it, and the handle the build
//! loop publishes through. This module stands all three up in one step and
//! hands them out, so the layers that hold them — the preconf service builder
//! and the payload service builder — only have to hold them, not know how to
//! build them.

use std::{future::Future, io, pin::Pin, sync::Arc};

use parking_lot::Mutex;

use crate::flashblocks::{
    config::FlashblockProducerConfig,
    publisher::{MantleFlashblocksPublisher, PublisherHandle},
};

/// What the build loop needs in order to publish slices.
///
/// Cheap to clone: the config is an `Arc` and the handle is a broadcast sender
/// plus a shared archive.
#[derive(Debug, Clone)]
pub struct FlashblocksProducer {
    /// Slice cadence and budget settings.
    pub cfg: Arc<FlashblockProducerConfig>,
    /// Where finished slices go.
    pub publisher: PublisherHandle,
}

/// Everything a node needs to keep the flashblocks **producer** endpoint
/// alive and to hand slice publishing to the payload builder.
///
/// Named for the side it serves because the consumer lives in its own crate
/// and will be wired into the same node: at a use site the crate path is gone
/// and a bare `Flashblocks*` says nothing about which half it belongs to.
///
/// The endpoint is bound at construction, so a port clash fails the node's
/// startup rather than a task nobody is watching. Dropping this closes the
/// endpoint and every open subscription, which is why it is held for the
/// node's lifetime rather than passed around by value.
pub struct FlashblocksProducerHandles {
    /// Cloned into the payload builder: the cadence settings and a handle
    /// to publish through.
    producer: FlashblocksProducer,
    /// Owns the listener.
    publisher: MantleFlashblocksPublisher,
    /// The accept loop, waiting for a layer that has somewhere to run it.
    /// Taken once — a second caller gets `None` rather than a second loop
    /// racing the first for connections.
    accepting: Mutex<Option<Pin<Box<dyn Future<Output = ()> + Send>>>>,
}

impl std::fmt::Debug for FlashblocksProducerHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlashblocksProducerHandles")
            .field("producer", &self.producer)
            .field("publisher", &self.publisher)
            .field("accept_loop_taken", &self.accepting.lock().is_none())
            .finish()
    }
}

impl FlashblocksProducerHandles {
    /// Bind the endpoint and assemble the producer side.
    ///
    /// Synchronous on purpose: binding here rather than inside the accept loop
    /// means a port already in use stops the node with a clear message,
    /// instead of leaving it running with an endpoint nobody is serving.
    pub fn bind(cfg: FlashblockProducerConfig) -> io::Result<Self> {
        let (publisher, accepting) = MantleFlashblocksPublisher::bind(&cfg)?;
        let producer = FlashblocksProducer { cfg: Arc::new(cfg), publisher: publisher.handle() };

        Ok(Self { producer, publisher, accepting: Mutex::new(Some(Box::pin(accepting))) })
    }

    /// The handle the payload builder publishes slices through.
    pub const fn producer(&self) -> &FlashblocksProducer {
        &self.producer
    }

    /// The bound endpoint. Useful for the address actually assigned when the
    /// port was left to the OS.
    pub const fn publisher(&self) -> &MantleFlashblocksPublisher {
        &self.publisher
    }

    /// Take the accept loop, for the caller to spawn.
    ///
    /// Deliberately not spawned here: how it runs, and what happens when it
    /// stops, is a decision for whoever owns the node's tasks. `None` on
    /// every call after the first.
    pub fn take_accept_loop(&self) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        self.accepting.lock().take()
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    /// Bind on an OS-assigned port so tests never collide.
    fn cfg() -> FlashblockProducerConfig {
        FlashblockProducerConfig { addr: Ipv4Addr::LOCALHOST.into(), port: 0, ..Default::default() }
    }

    /// The accept loop is handed out once. A second caller getting its own
    /// copy would mean two loops racing for the same connections.
    #[tokio::test]
    async fn the_accept_loop_is_handed_out_once() {
        let handles = FlashblocksProducerHandles::bind(cfg()).expect("binds");

        assert!(handles.take_accept_loop().is_some());
        assert!(handles.take_accept_loop().is_none(), "a second caller must not get a loop");
    }

    /// Binding reports the address actually assigned, which is the only way a
    /// caller learns the port when it left the choice to the OS.
    #[tokio::test]
    async fn binding_reports_the_address_it_got() {
        let handles = FlashblocksProducerHandles::bind(cfg()).expect("binds");

        assert_ne!(handles.publisher().local_addr().port(), 0, "the OS assigned a real port");
    }

    /// The build loop's handle publishes into the archive the bound endpoint
    /// replays from — one endpoint, not two.
    #[tokio::test]
    async fn the_producer_handle_shares_the_bound_endpoints_archive() {
        let handles = FlashblocksProducerHandles::bind(cfg()).expect("binds");
        let mut payload = mantle_reth_flashblocks_types::MantleFlashblockPayload {
            index: 4,
            ..Default::default()
        };
        payload.metadata.block_number = 11;

        handles.producer().publisher.publish(&payload).expect("publishes");

        assert_eq!(
            handles.publisher().handle().latest_position(),
            Some(crate::flashblocks::FlashblockPosition { block_number: 11, flashblock_index: 4 }),
        );
    }
}

//! Metrics for the flashblock consumer.
//!
//! Names are dot-separated under `flashblocks.consumer`, matching
//! `flashblocks.proxy` in `mantle-reth-ws-proxy`. The segment is load-bearing:
//! a sequencer runs the producer (which publishes flat `flashblocks.*` names
//! from `mantle-reth-preconf`) and this consumer in one process, and without
//! it there is no way to tell from a series name which half emitted it.
//!
//! Counters end in `_total`, as the producer's already do and as Prometheus
//! expects.
//!
//! [`Metrics::init`] must be called once, after the Prometheus recorder is
//! installed — see its documentation for what breaks otherwise.

use metrics::{Counter, Gauge, Histogram};

/// Accessors for every flashblock consumer metric series.
#[derive(Debug, Clone, Copy)]
pub struct Metrics;

macro_rules! flashblock_metrics {
    ($( $kind:ident $name:ident => $series:literal , $desc:literal ; )*) => {
        impl Metrics {
            $(
                #[doc = concat!("The `", $series, "` series: ", $desc, ".")]
                pub fn $name() -> flashblock_metrics!(@ty $kind) {
                    metrics::$kind!($series)
                }
            )*

            /// Registers descriptions and publishes a zero for every counter
            /// and gauge.
            ///
            /// Must run once, **after** the Prometheus recorder is installed —
            /// calls made before that go to a no-op recorder and are lost.
            ///
            /// Zeroing is not cosmetic. A `metrics` series comes into
            /// existence on its first observation, so without this a counter
            /// that has never fired is **absent** from `/metrics` rather than
            /// reported as 0. Prometheus then answers queries against it with
            /// no data, which reads as "not scraped" rather than "nothing has
            /// happened" — and an alert cannot be written against a series
            /// that may or may not exist.
            ///
            /// The series that matter most are exactly the ones that stay at
            /// zero in a healthy process: `foreign_build_slice_total` is the
            /// only signal for a build this node did not follow,
            /// `block_processing_error_total` and `unexpected_block_order_total`
            /// are the two that say the stream went wrong.
            ///
            /// Histograms are left alone: they have no zero observation to
            /// publish.
            pub fn init() {
                $( flashblock_metrics!(@describe $kind $series, $desc); )*
                $( flashblock_metrics!(@zero $kind $series); )*
            }
        }
    };

    (@ty counter) => { Counter };
    (@ty gauge) => { Gauge };
    (@ty histogram) => { Histogram };

    (@describe counter $series:literal, $desc:literal) => {
        metrics::describe_counter!($series, $desc)
    };
    (@describe gauge $series:literal, $desc:literal) => {
        metrics::describe_gauge!($series, $desc)
    };
    (@describe histogram $series:literal, $desc:literal) => {
        metrics::describe_histogram!($series, $desc)
    };

    (@zero counter $series:literal) => { metrics::counter!($series).absolute(0) };
    (@zero gauge $series:literal) => { metrics::gauge!($series).set(0.0) };
    (@zero histogram $series:literal) => {};
}

flashblock_metrics! {
    counter   upstream_errors                           => "flashblocks.consumer.upstream_errors_total",
        "Websocket errors on the upstream slice connection";
    counter   upstream_decode_errors                    => "flashblocks.consumer.upstream_decode_errors_total",
        "Upstream text or binary messages that could not be decoded as a slice";
    counter   upstream_messages                         => "flashblocks.consumer.upstream_messages_total",
        "Slices received from upstream";
    histogram block_processing_duration                 => "flashblocks.consumer.block_processing_duration",
        "Time to fold one slice into the overlay";
    histogram sender_recovery_duration                  => "flashblocks.consumer.sender_recovery_duration",
        "Time spent recovering senders for one slice";
    counter   unexpected_block_order                    => "flashblocks.consumer.unexpected_block_order_total",
        "Slices refused for arriving out of order";
    counter   rebuilt_block                             => "flashblocks.consumer.rebuilt_block_total",
        "Blocks whose overlay was replaced by a new build of the same height";
    counter   foreign_build_slice                       => "flashblocks.consumer.foreign_build_slice_total",
        "Continuation slices from a build this node never saw the base of";
    counter   stale_predecessor                         => "flashblocks.consumer.stale_predecessor_total",
        "Base slices that did not link to the overlay's parent block, restarted from the canonical parent";
    counter   parent_hash_mismatch                      => "flashblocks.consumer.parent_hash_mismatch_total",
        "Base payloads whose parent hash differed from the canonical parent, dropping the overlay";
    counter   slice_content_mismatch                    => "flashblocks.consumer.slice_content_mismatch_total",
        "Slices that did not follow the overlay's latest slice by content, dropping the overlay";
    counter   base_fee_mismatch                         => "flashblocks.consumer.base_fee_mismatch_total",
        "Base slices whose basefee differed from the one derived from the parent header, using the slice's";
    histogram flashblocks_in_block                      => "flashblocks.consumer.slices_per_block",
        "Slices the consumer saw for one block (the producer's view is `flashblocks.slices_per_block`)";
    counter   block_processing_error                    => "flashblocks.consumer.block_processing_error_total",
        "Slices that failed to apply";
    counter   pending_clear_catchup                     => "flashblocks.consumer.pending_clear_catchup_total",
        "Overlays cleared because the canonical chain caught up with them";
    counter   pending_clear_reorg                       => "flashblocks.consumer.pending_clear_reorg_total",
        "Overlays cleared by a reorg";
    counter   flashblock_superseded                     => "flashblocks.consumer.superseded_slices_total",
        "Slices superseded before they were applied (the producer's counterpart is `flashblocks.superseded_slices_total`)";
    counter   pending_drop_not_ahead                    => "flashblocks.consumer.pending_drop_not_ahead_total",
        "Overlays dropped for not leading the canonical tip";
    counter   pending_drop_stranded                     => "flashblocks.consumer.pending_drop_stranded_total",
        "Overlays dropped for being stranded off the canonical chain";
    counter   pending_drop_base_mismatch                => "flashblocks.consumer.pending_drop_base_mismatch_total",
        "Overlays dropped because their base no longer matches the chain";
    gauge     pending_snapshot_fb_index                 => "flashblocks.consumer.pending_snapshot_fb_index",
        "Slice index of the overlay currently served";
    gauge     pending_snapshot_height                   => "flashblocks.consumer.pending_snapshot_height",
        "Block height of the overlay currently served";
    counter   leading_depth_exceeded                    => "flashblocks.consumer.leading_depth_exceeded_total",
        "Slices refused for running too far ahead of the canonical tip";
    gauge     queue_depth                               => "flashblocks.consumer.queue_depth",
        "Slices waiting in the state-update queue";
    counter   reconnect_attempts                        => "flashblocks.consumer.reconnect_attempts_total",
        "Reconnections attempted against the upstream";
    counter   subscription_lagged                       => "flashblocks.consumer.subscription_lagged_total",
        "Overlay updates eth_subscribe flashblocks subscribers fell too far behind to receive";
    gauge     subscriptions_active                      => "flashblocks.consumer.subscriptions_active",
        "Open eth_subscribe flashblocks subscriptions";
    counter   rpc_get_transaction_count                 => "flashblocks.consumer.rpc.get_transaction_count_total",
        "`eth_getTransactionCount` calls served from the overlay";
    counter   rpc_get_transaction_receipt               => "flashblocks.consumer.rpc.get_transaction_receipt_total",
        "`eth_getTransactionReceipt` calls served from the overlay";
    counter   rpc_get_transaction_by_hash               => "flashblocks.consumer.rpc.get_transaction_by_hash_total",
        "`eth_getTransactionByHash` calls served from the overlay";
    counter   rpc_get_balance                           => "flashblocks.consumer.rpc.get_balance_total",
        "`eth_getBalance` calls served from the overlay";
    counter   rpc_get_code                              => "flashblocks.consumer.rpc.get_code_total",
        "`eth_getCode` calls served from the overlay";
    counter   rpc_get_storage_at                        => "flashblocks.consumer.rpc.get_storage_at_total",
        "`eth_getStorageAt` calls served from the overlay";
    counter   rpc_get_block_by_number                   => "flashblocks.consumer.rpc.get_block_by_number_total",
        "`eth_getBlockByNumber` calls served from the overlay";
    counter   rpc_call                                  => "flashblocks.consumer.rpc.call_total",
        "`eth_call` calls served from the overlay";
    counter   rpc_estimate_gas                          => "flashblocks.consumer.rpc.estimate_gas_total",
        "`eth_estimateGas` calls served from the overlay";
    // Emitted from `mantle-reth-rpc-ext`, not from this crate — `eth_simulateV1`
    // is owned by `MantleEthApiExt`, which injects the pending overrides.
    counter   rpc_simulate_v1                           => "flashblocks.consumer.rpc.simulate_v1_total",
        "`eth_simulateV1` calls served with pending overrides";
    counter   rpc_get_logs                              => "flashblocks.consumer.rpc.get_logs_total",
        "`eth_getLogs` calls served from the overlay";
    counter   rpc_get_block_transaction_count_by_number => "flashblocks.consumer.rpc.get_block_transaction_count_by_number_total",
        "`eth_getBlockTransactionCountByNumber` calls served from the overlay";
    histogram bundle_state_clone_duration               => "flashblocks.consumer.bundle_state_clone_duration",
        "Time spent cloning the bundle state for one overlay";
    histogram bundle_state_clone_size                   => "flashblocks.consumer.bundle_state_clone_size",
        "Accounts cloned with the bundle state for one overlay";
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::Metrics;

    /// Without `init`, a counter that has never fired is **absent** from the
    /// scrape rather than reported as 0 — which Prometheus answers as "no
    /// data", indistinguishable from "not scraped". An alert cannot be
    /// written against a series that may or may not exist.
    ///
    /// `foreign_build_slice_total` is the one asserted on because it is the
    /// only signal a build this node did not follow produces, and it is
    /// supposed to stay at zero.
    #[test]
    fn init_publishes_a_zero_for_a_counter_that_has_never_fired() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let before = handle.render();
        assert!(
            !before.contains("flashblocks_consumer_foreign_build_slice_total"),
            "premise: nothing has observed it yet",
        );

        metrics::with_local_recorder(&recorder, Metrics::init);

        let after = handle.render();
        assert!(
            after.contains("flashblocks_consumer_upstream_decode_errors_total 0"),
            "decode errors must be observable at zero on a healthy node:\n{after}",
        );
        assert!(
            after.contains("flashblocks_consumer_foreign_build_slice_total 0"),
            "a healthy node must still publish this series at zero:\n{after}",
        );
        assert!(
            after.contains("# HELP flashblocks_consumer_foreign_build_slice_total "),
            "and with the description `init` registers:\n{after}",
        );
    }

    /// Histograms have no zero observation to publish, so `init` describes
    /// them and leaves them alone. Asserted so that a future change which
    /// starts recording a synthetic 0 is noticed.
    #[test]
    fn init_leaves_histograms_unobserved() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, Metrics::init);

        assert!(
            !handle.render().contains("flashblocks_consumer_block_processing_duration"),
            "a histogram must not be given an observation it never had",
        );
    }
}

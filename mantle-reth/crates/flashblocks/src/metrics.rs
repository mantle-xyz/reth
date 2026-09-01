//! Metrics for the flashblock consumer.
//!
//! Names are dot-separated to match the convention in `preconf::metrics_seed`.

use metrics::{Counter, Gauge, Histogram};

/// Accessors for every flashblock consumer metric series.
#[derive(Debug, Clone, Copy)]
pub struct Metrics;

macro_rules! flashblock_metrics {
    ($( $kind:ident $name:ident => $series:literal ; )*) => {
        impl Metrics {
            $(
                #[doc = concat!("The `", $series, "` series.")]
                pub fn $name() -> flashblock_metrics!(@ty $kind) {
                    metrics::$kind!($series)
                }
            )*
        }
    };
    (@ty counter) => { Counter };
    (@ty gauge) => { Gauge };
    (@ty histogram) => { Histogram };
}

flashblock_metrics! {
    counter   upstream_errors                           => "flashblocks.upstream_errors";
    counter   upstream_messages                         => "flashblocks.upstream_messages";
    histogram block_processing_duration                 => "flashblocks.block_processing_duration";
    histogram sender_recovery_duration                  => "flashblocks.sender_recovery_duration";
    counter   unexpected_block_order                    => "flashblocks.unexpected_block_order";
    histogram flashblocks_in_block                      => "flashblocks.flashblocks_in_block";
    counter   block_processing_error                    => "flashblocks.block_processing_error";
    counter   pending_clear_catchup                     => "flashblocks.pending_clear_catchup";
    counter   pending_clear_reorg                       => "flashblocks.pending_clear_reorg";
    gauge     pending_snapshot_fb_index                 => "flashblocks.pending_snapshot_fb_index";
    gauge     pending_snapshot_height                   => "flashblocks.pending_snapshot_height";
    counter   leading_depth_exceeded                    => "flashblocks.leading_depth_exceeded";
    counter   reconnect_attempts                        => "flashblocks.reconnect_attempts";
    counter   rpc_get_transaction_count                 => "flashblocks.rpc.get_transaction_count";
    counter   rpc_get_transaction_receipt               => "flashblocks.rpc.get_transaction_receipt";
    counter   rpc_get_transaction_by_hash               => "flashblocks.rpc.get_transaction_by_hash";
    counter   rpc_get_balance                           => "flashblocks.rpc.get_balance";
    counter   rpc_get_block_by_number                   => "flashblocks.rpc.get_block_by_number";
    counter   rpc_call                                  => "flashblocks.rpc.call";
    counter   rpc_estimate_gas                          => "flashblocks.rpc.estimate_gas";
    counter   rpc_simulate_v1                           => "flashblocks.rpc.simulate_v1";
    counter   rpc_get_logs                              => "flashblocks.rpc.get_logs";
    counter   rpc_get_block_transaction_count_by_number => "flashblocks.rpc.get_block_transaction_count_by_number";
    histogram bundle_state_clone_duration               => "flashblocks.bundle_state_clone_duration";
    histogram bundle_state_clone_size                   => "flashblocks.bundle_state_clone_size";
}

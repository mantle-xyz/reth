//! Startup pre-registration ("seeding") of this crate's metric series —
//! `preconf.*`, `flashblocks.*` and the shared `mantle.*` alike.
//!
//! The `metrics` facade creates a series lazily on the first emit, so a
//! rare-event counter (`preconf.api.timeout_total`,
//! `preconf.build.panic_total`, `preconf.whitelist.reload_failed`, ...) has no
//! Prometheus until it first fires — exactly when an alert most needs a
//! stable `0` baseline, and when dashboards would otherwise show "no data"
//! (indistinguishable from a broken pipeline). [`seed_preconf_metrics`]
//! registers every metric emitted under either prefix up front, so every
//! counter and gauge carries a `0` from node start. Histograms are registered
//! but deliberately not observed, so their `_count` / `_bucket` series still
//! appear only with the first real observation — see [`HISTOGRAMS`].
//!
//! Names MUST match the inline emit sites verbatim (dot-separated) — a mismatch
//! registers a shadow series that collides on export. The
//! `seed_lists_cover_every_emitted_metric` test guards against drift.

/// Counter series. Registered at their true starting value (`0`) —
/// `increment(0)` never clobbers.
const COUNTERS: &[&str] = &[
    "flashblocks.slice_allowance_exhausted_total",
    "flashblocks.oversized_slices_total",
    "preconf.build.failed_total",
    "preconf.build.resolved_payload_superseded_total",
    "preconf.build.pool_tx_on_committed_nonce_total",
    "preconf.build.pool_tx_already_applied_total",
    "preconf.build.tx_nonce_already_used_total",
    "preconf.build.tx_over_block_limits_total",
    // The two halves of the block's RLP budget verdict; why they are counted
    // apart is at the `RawSizeAdmission` branch in
    // `builder::payload_builder::apply_one_best_tx`. Distinct from
    // `tx_over_block_limits_total` above, which is the block's gas/DA ceilings
    // rather than its RLP size.
    "preconf.build.tx_exceeds_block_rlp_total",
    "preconf.build.block_rlp_full_skipped_total",
    "preconf.build.tx_rejected_by_evm_total",
    "preconf.rpc.nonce_gap_rejected_total",
    "preconf.admit.rejected_entries_total",
    "preconf.admit.rejected_bytes_total",
    "preconf.admit.rejected_gas_total",
    "preconf.admit.rejected_account_slots_total",
    "preconf.admit.rejected_delegated_total",
    // The three policy rejections, as opposed to the capacity ones above.
    // `rejected_not_eligible_total` is the one that matters: an allowlist
    // misconfiguration is indistinguishable, from outside, from a node that
    // is ignoring preconf requests.
    "preconf.admit.rejected_tx_type_total",
    "preconf.admit.rejected_not_eligible_total",
    "preconf.admit.rejected_tx_gas_total",
    // Restore-path losses: the whole file failing to load (startup then
    // aborts), and single lines skipped.
    "preconf.journal.restore_load_failed_total",
    "preconf.journal.restore_bad_lines_total",
    "flashblocks.deadline_already_passed_total",
    "flashblocks.dropped_after_abandon_total",
    // The two slice losses that used to leave only a WARN behind. Neither
    // breaks the index chain — a subscriber sees fewer slices, not a gap —
    // which is exactly why they need counting.
    "flashblocks.slice_assembly_failed_total",
    "flashblocks.slice_serialize_failed_total",
    "flashblocks.pool_maintenance_balance_unreadable_total",
    // Slices published for a block whose seal then failed. Zero on a healthy
    // chain; non-zero means subscribers held a prefix of a block that does
    // not exist, which the rebuild replaces but nothing else reports.
    "flashblocks.slices_outliving_their_block_total",
    "flashblocks.unlanded_sweep_slow_total",
    "flashblocks.unlanded_nonce_unreadable_total",
    // Same failure on the queue's forward sweep at build start.
    "preconf.build.forward_nonce_unreadable_total",
    "flashblocks.slices_missed_by_slow_subscribers_total",
    "flashblocks.slow_subscriber_dropped_total",
    "flashblocks.resume_before_archive_total",
    "flashblocks.superseded_slices_total",
    // `Timeout` answers sent to clients, from the RPC deadline and from
    // dispatch's pre-apply gate alike.
    "preconf.api.timeout_total",
    // The completion protocol was broken: an entry finished without a result
    // reaching its client. Should stay flat at zero.
    "preconf.api.no_result_total",
    "preconf.tx.success_total",
    "preconf.tx.failure_total",
    "preconf.tx.fatal_total",
    "preconf.build.panic_total",
    // A job abandoned while it waited for the build lock, so it built nothing.
    // Routine when jobs are superseded; not counted as a failure.
    "preconf.build.abandoned_before_start_total",
    // A build found the shared allowlist behind its parent (`listsHash` differs)
    // and read the parent's lists itself. Occasional; a steady rate means the
    // whitelist watcher lags block production.
    "preconf.whitelist.build_reread_total",
    // That check or read failed; the build used the shared allowlist.
    "preconf.whitelist.build_reread_failed_total",
    // A build that found its parent no longer on the canonical chain when
    // about to replay a commitment, and abandoned itself. Steady state is 0; it
    // moves with reorgs.
    "preconf.build.stale_parent_abandoned_total",
    // The canonical chain could not be read at that check, so the replayed
    // commitment waited for a later build.
    "preconf.build.canonical_chain_unreadable_total",
    "preconf.build.watchdog_cancel_total",
    "preconf.fifo.da_rejected_total",
    "preconf.fifo.block_gas_rejected_total",
    "preconf.fifo.block_rlp_rejected_total",
    "preconf.fifo.block_uncompressed_rejected_total",
    "preconf.fifo.staged_evicted_total",
    // An entry left the queue with a client still waiting on it — the
    // completion protocol was bypassed. Should stay flat at zero.
    "preconf.fifo.responder_dropped_total",
    "preconf.fifo.replay_deferred_total",
    "preconf.dispatch.deadline_skipped_total",
    "preconf.dispatch.gas_budget_skipped_total",
    // What the EVM refused, under the reason the client is told — the
    // builder-stage counterpart of the `preconf.admit.rejected_*` counters.
    "preconf.execute.rejected_funds_total",
    "preconf.execute.rejected_base_fee_total",
    "preconf.execute.rejected_other_total",
    // Commitment retention + on-chain allowlist governance.
    // Shared with `mantle-reth-flashblocks`, which emits the same series under
    // its own `subscriber` label and cannot seed it — see `PREFIXES`.
    "mantle.canon.notifications_lagged_total",
    "preconf.canon.reinject_conflict_total",
    "preconf.canon.reorg_drift_total",
    "preconf.tx.announced_dropped_total",
    "preconf.tx.commitment_broken_total",
    "preconf.tx.replay_round_total",
    "preconf.journal.dropped_entries_total",
    "preconf.journal.restore_nonce_taken",
    "preconf.journal.restore_promise_nonce_taken",
    "preconf.journal.restore_undecodable",
    "preconf.journal.restore_unknown",
    "preconf.whitelist.zero_entry_skipped",
    "preconf.whitelist.revoked_total",
    // The pair that says whether the in-memory allowlist is still tracking the
    // contract. Seeding matters more than usual here: a refresh only ever runs on
    // a governance update, or a reorg or gap that moved `listsHash`, so on a quiet
    // chain both series would stay unregistered for hours and "never refreshed"
    // would be indistinguishable from "refreshing fine".
    "preconf.whitelist.reload_ok",
    "preconf.whitelist.reload_failed",
    // The watcher skipped a full re-read: `listsHash` at the head was unchanged.
    "preconf.whitelist.reload_skipped_total",
];

/// Gauge series. Registered **non-destructively** via `increment(0.0)`: the
/// journal already publishes a real value for `size_bytes` at `open()`, so
/// `set(0.0)` here could clobber it back to `0` depending on call ordering; a
/// zero increment only registers the series, leaving any existing value intact.
const GAUGES: &[&str] = &[
    "flashblocks.endpoint_serving",
    "flashblocks.subscribers",
    "preconf.fifo.pending",
    "preconf.fifo.entries",
    "preconf.fifo.bytes_used",
    "preconf.fifo.gas_used",
    "preconf.journal.buffered_entries",
    "preconf.journal.pending_entries",
    "preconf.journal.size_bytes",
    "preconf.classifier.records",
    "preconf.classifier.slots",
    "preconf.classifier.over_capacity",
    "preconf.classifier.persisted_height",
    "preconf.whitelist.pair_count",
    "preconf.whitelist.from_wildcard_count",
    "preconf.whitelist.to_wildcard_count",
    "preconf.whitelist.warn_threshold",
    "preconf.whitelist.consecutive_reload_failures",
];

/// Histogram series. Registered only — never `record(0.0)`, which would inject
/// a fake observation and skew the distribution. `_count`/`_bucket` still
/// materialise on the first real observation, but duration metrics see traffic
/// almost immediately so the gap is negligible; the list exists mainly so the
/// drift test covers histograms too.
const HISTOGRAMS: &[&str] = &[
    "flashblocks.byte_size",
    // Isolates sealing from the wall-clock `publish_interval_ms`, which also
    // contains publishing and idle waiting.
    "flashblocks.slice_seal_duration_ms",
    "flashblocks.first_slice_offset_ms",
    "flashblocks.journal_write_duration_ms",
    "flashblocks.publish_interval_ms",
    "flashblocks.slices_per_block",
    // The slice-boundary arm as a whole, and its two pool steps.
    "flashblocks.slice_boundary_duration_ms",
    "flashblocks.pool_maintenance_duration_ms",
    "flashblocks.pool_maintenance_accounts",
    "flashblocks.best_txs_refresh_duration_ms",
    "mantle.canon.notification_duration",
    "preconf.api.handle_duration_ms",
    "preconf.build.lock_wait_ms",
    "preconf.execute.duration_ms",
    "preconf.dispatch.elapsed_at_gate_ms",
    "preconf.journal.rotate_duration_ms",
    "preconf.journal.rotate_locked_ms",
];

/// Pre-register every metric this crate emits — `preconf.*` and
/// `flashblocks.*` — so counter and gauge series exist from node start with a
/// `0` baseline instead of appearing only on the first emit. Histograms are
/// registered without an observation, so theirs still appear on first use.
///
/// Call once at startup, **only when preconf is enabled**, so other nodes
/// don't expose phantom series. Requires the metrics recorder to be
/// installed already (true by the `on_node_started` hook). Idempotent.
pub fn seed_preconf_metrics() {
    metrics::describe_counter!(
        "flashblocks.resume_before_archive_total",
        "Replay attempts whose requested position precedes the oldest retained slice, including attempts that later fail; not a count of missing slices"
    );
    for &name in COUNTERS {
        metrics::counter!(name).increment(0);
    }
    for &name in GAUGES {
        metrics::gauge!(name).increment(0.0);
    }
    for &name in HISTOGRAMS {
        // Register the handle without observing (see `HISTOGRAMS` doc).
        let _ = metrics::histogram!(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, fs, path::Path};

    /// Recursively collect `*.rs` file contents under `dir`.
    fn read_rs_sources(dir: &Path, out: &mut String) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                read_rs_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push_str(&fs::read_to_string(&path).expect("read_to_string"));
                out.push('\n');
            }
        }
    }

    /// Namespaces this crate emits under. All three are scanned, so a metric
    /// added under any of them is held to the same seeding rule.
    ///
    /// `mantle.` is the shared namespace for a series more than one crate
    /// emits under one name, so a single alert covers every emitter.
    ///
    /// `flashblocks.` is shared differently: the producer metrics here and the
    /// consumer metrics in `mantle-reth-flashblocks` sit in one namespace, but
    /// only the producer half is seeded. This test reads **this** crate's
    /// sources, and that crate has no seeding of its own — so a consumer
    /// metric can be added without anything noticing.
    const PREFIXES: &[&str] = &["preconf.", "flashblocks.", "mantle."];

    /// Extract every name passed to `<macro>!("<prefix>…"` in the source (i.e.
    /// the inline emit sites; the seed lists use a variable, not a literal, so
    /// they don't match).
    fn emitted_names(src: &str, macro_name: &str) -> HashSet<String> {
        let mut names = HashSet::new();
        let open = format!("{macro_name}!(");
        let mut rest = src;
        while let Some(pos) = rest.find(&open) {
            let after = &rest[pos + open.len()..];
            rest = after;

            // The name is not always adjacent to the paren: labels push the
            // call past the line width and rustfmt then puts the name on its
            // own line. Requiring adjacency used to let every labelled emit
            // through unchecked.
            let Some(body) = after.trim_start().strip_prefix('"') else { continue };
            let Some(end) = body.find('"') else { continue };
            let name = &body[..end];
            if PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
                names.insert(name.to_string());
            }
        }
        names
    }

    /// Every metric emitted anywhere in the crate must appear in the matching
    /// seed list — otherwise it regresses to lazy registration.
    ///
    /// Names are all this compares, because names are all it can read out of
    /// the source: passing says a series is registered and spelled right, not
    /// that it reports anything true. Computing a value wrong
    /// (`increment(superseded - 1)`), counting a gauge the wrong way
    /// (`decrement` when a subscriber arrives), and deleting the line that
    /// constructs a `Drop`-guard all leave the whole suite green — the last
    /// because the macro call stays in the source for this test to find while
    /// the metric never fires again, and is caught by `-D warnings` on the
    /// unused binding instead.
    ///
    /// Deliberately so: a wrong number bends a chart rather than changing what
    /// the chain does, and the recorder is process-global, so reading values
    /// back inside a shared test binary would collide with whatever else is
    /// running. Names are held here; values by review and by watching a real
    /// network.
    #[test]
    fn seed_lists_cover_every_emitted_metric() {
        let mut src = String::new();
        read_rs_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(), &mut src);

        let mut total_found = 0usize;
        for (macro_name, seeded) in
            [("counter", COUNTERS), ("gauge", GAUGES), ("histogram", HISTOGRAMS)]
        {
            let seeded: HashSet<&str> = seeded.iter().copied().collect();
            let emitted = emitted_names(&src, macro_name);
            total_found += emitted.len();
            for name in &emitted {
                assert!(
                    seeded.contains(name.as_str()),
                    "{macro_name} metric {name:?} is emitted but missing from the seed list \
                     in metrics_seed.rs — add it so its series is pre-registered",
                );
            }
            // And the reverse: a deleted emit site would otherwise leave its
            // series pre-registered and flat at zero — indistinguishable from
            // "this never happened", the very failure mode seeding exists to
            // prevent.
            for name in &seeded {
                assert!(
                    emitted.contains(*name),
                    "{macro_name} metric {name:?} is pre-registered but nothing emits it — \
                     drop it from the seed list, or restore the emit site",
                );
            }
        }
        // Guard against a parser regression silently passing the test vacuously.
        assert!(total_found >= 15, "expected to scan the emit sites, found only {total_found}");
    }
}

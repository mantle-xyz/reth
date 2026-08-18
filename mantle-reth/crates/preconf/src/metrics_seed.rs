//! Startup pre-registration ("seeding") of preconf metric series.
//!
//! The `metrics` facade creates a series lazily on the first emit, so a
//! rare-event counter (timeouts, failures, replacement rejections, ...) has no
//! Prometheus series until it first fires — exactly when an alert most needs a
//! stable `0` baseline, and when dashboards would otherwise show "no data"
//! (indistinguishable from a broken pipeline). [`seed_preconf_metrics`]
//! registers every preconf metric up front so scrapes carry a `0` from node
//! start.
//!
//! Names MUST match the inline emit sites verbatim (dot-separated) — a mismatch
//! registers a shadow series that collides on export. The
//! `seed_lists_cover_every_emitted_metric` test guards against drift.

/// Counter series. Registered at their true starting value (`0`) —
/// `increment(0)` never clobbers.
const COUNTERS: &[&str] = &[
    "preconf.api.timeout_total",
    "preconf.tx.success_total",
    "preconf.tx.failure_total",
    "preconf.tx.fatal_total",
    "preconf.build.panic_total",
    "preconf.build.watchdog_cancel_total",
    "preconf.fifo.da_rejected_total",
    "preconf.fifo.replay_deferred_total",
    "preconf.dispatch.deadline_skipped_total",
    "preconf.dispatch.gas_budget_skipped_total",
    "preconf.listener.replacement_rejected_total",
    "preconf.journal.abandoned_total",
    "preconf.pending_responders.expired_total",
    // Commitment retention + on-chain allowlist governance.
    "preconf.canon.reorg_drift_total",
    "preconf.tx.commitment_broken_total",
    "preconf.tx.replay_retry_total",
    "preconf.tx.replay_round_total",
    "preconf.journal.restore_nonce_taken",
    "preconf.journal.restore_undecodable",
    "preconf.journal.restore_unknown",
    "preconf.whitelist.zero_entry_skipped",
    "preconf.whitelist.revoked_total",
];

/// Gauge series. Registered **non-destructively** via `increment(0.0)`: the
/// journal already publishes a real value for `size_bytes` at `open()`, so
/// `set(0.0)` here could clobber it back to `0` depending on call ordering; a
/// zero increment only registers the series, leaving any existing value intact.
///
/// `journal.sealed_len` / `journal.promised_len` are gone: the journal no longer
/// keeps either set — the classifier owns commitment tracking, and
/// `classifier.verdicts` / `classifier.slots` are the replacement signals.
const GAUGES: &[&str] = &[
    "preconf.fifo.pending",
    "preconf.journal.size_bytes",
    "preconf.classifier.verdicts",
    "preconf.classifier.slots",
    "preconf.classifier.over_capacity",
    "preconf.classifier.persisted_height",
    "preconf.whitelist.pair_count",
    "preconf.whitelist.from_wildcard_count",
    "preconf.whitelist.to_wildcard_count",
    "preconf.whitelist.warn_threshold",
];

/// Histogram series. Registered only — never `record(0.0)`, which would inject
/// a fake observation and skew the distribution. `_count`/`_bucket` still
/// materialise on the first real observation, but duration metrics see traffic
/// almost immediately so the gap is negligible; the list exists mainly so the
/// drift test covers histograms too.
const HISTOGRAMS: &[&str] = &[
    "preconf.api.handle_duration_ms",
    "preconf.execute.duration_ms",
    "preconf.validate.duration_ms",
    "preconf.dispatch.elapsed_at_gate_ms",
    "preconf.journal.rotate_duration_ms",
];

/// Pre-register every preconf metric so its Prometheus series exists from node
/// start with a `0` baseline instead of appearing only on the first emit.
///
/// Call once at startup, **only when preconf is enabled**, so non-preconf nodes
/// don't expose phantom preconf series. Requires the metrics recorder to be
/// installed already (true by the `on_node_started` hook). Idempotent.
pub fn seed_preconf_metrics() {
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

    /// Extract every `"preconf.…"` name passed to `<macro>!("preconf.…"` in the
    /// source (i.e. the inline emit sites; the seed lists use a variable, not a
    /// literal, so they don't match).
    fn emitted_names(src: &str, macro_name: &str) -> HashSet<String> {
        let needle = format!("{macro_name}!(\"preconf.");
        let mut names = HashSet::new();
        let mut rest = src;
        while let Some(pos) = rest.find(&needle) {
            let after = &rest[pos + needle.len() - "preconf.".len()..];
            if let Some(end) = after.find('"') {
                names.insert(after[..end].to_string());
            }
            rest = &rest[pos + needle.len()..];
        }
        names
    }

    /// Every `preconf.*` metric emitted anywhere in the crate must appear in the
    /// matching seed list — otherwise it regresses to lazy registration.
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
            // And the reverse. Without it a deleted emit site is invisible: the
            // series keeps being pre-registered, so it still shows up in a
            // scrape — flat at zero, indistinguishable from "this never
            // happened". That is exactly the failure mode seeding exists to
            // prevent, reintroduced from the other end. Deleting a metric is
            // fine; deleting it from *both* lists is what this asks for.
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

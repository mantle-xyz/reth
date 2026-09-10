//! Metrics for the flashblock websocket proxy.
//!
//! Names are dot-separated under `flashblocks.proxy`, matching the convention
//! in `mantle-reth-flashblocks` and `preconf::metrics_seed`.
//!
//! [`Metrics::init`] must be called once, after the Prometheus recorder is
//! installed — see its documentation for what breaks otherwise.

use metrics::{Counter, Gauge, Histogram};

/// Accessors for every proxy metric series.
#[derive(Debug, Clone, Copy)]
pub struct Metrics;

macro_rules! proxy_metrics {
    ($( $kind:ident $name:ident => $series:literal , $desc:literal ; )*) => {
        impl Metrics {
            $(
                #[doc = concat!("The `", $series, "` series: ", $desc, ".")]
                pub fn $name() -> proxy_metrics!(@ty $kind) {
                    metrics::$kind!($series)
                }
            )*

            /// Registers descriptions and publishes a zero for every counter
            /// and gauge.
            ///
            /// Must run once, **after** the Prometheus recorder is installed —
            /// calls made before that are recorded against a no-op recorder
            /// and silently lost.
            ///
            /// Zeroing is not cosmetic. A `metrics` series comes into
            /// existence on its first observation, so without this a counter
            /// that has never fired is absent from `/metrics` rather than
            /// reported as 0. Prometheus then answers queries against it with
            /// no data, which reads as "not scraped" instead of "nothing has
            /// happened" — and the series that matter most here are exactly
            /// the ones that stay at zero in a healthy process:
            /// `unauthorized_requests`, the two rate-limit counters,
            /// `upstream_errors`, `failed_messages`, `lagged_connections`,
            /// `client_pong_disconnects` and `ping_failures`.
            ///
            /// Histograms are left alone: they have no zero observation to
            /// publish. The two labelled series are described but not zeroed,
            /// since their label values are only known once a client connects
            /// or an upstream is configured.
            pub fn init() {
                $( proxy_metrics!(@describe $kind $series, $desc); )*
                $( proxy_metrics!(@zero $kind $series); )*

                metrics::describe_counter!(
                    CONNECTIONS_BY_APP,
                    "Downstream connections opened, by authenticated application"
                );
                metrics::describe_counter!(
                    UPSTREAM_MESSAGES,
                    "Messages received from upstream, by upstream URI"
                );
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

/// The `flashblocks.proxy.connections_by_app` series name.
const CONNECTIONS_BY_APP: &str = "flashblocks.proxy.connections_by_app";
/// The `flashblocks.proxy.upstream_messages` series name.
const UPSTREAM_MESSAGES: &str = "flashblocks.proxy.upstream_messages";

proxy_metrics! {
    counter   sent_messages                 => "flashblocks.proxy.sent_messages",
        "Messages sent to clients";
    counter   failed_messages               => "flashblocks.proxy.failed_messages",
        "Messages that could not be sent to a client";
    histogram message_send_duration         => "flashblocks.proxy.message_send_duration",
        "Duration of a single send to one client";
    gauge     broadcast_queue_size          => "flashblocks.proxy.broadcast_queue_size",
        "Current depth of the broadcast queue";
    counter   new_connections               => "flashblocks.proxy.new_connections",
        "Downstream connections opened";
    counter   closed_connections            => "flashblocks.proxy.closed_connections",
        "Downstream connections closed";
    counter   lagged_connections            => "flashblocks.proxy.lagged_connections",
        "Downstream connections dropped for lagging past the broadcast buffer";
    gauge     active_connections            => "flashblocks.proxy.active_connections",
        "Downstream connections currently open";
    counter   per_ip_rate_limited_requests  => "flashblocks.proxy.per_ip_rate_limited_requests",
        "Connection attempts rejected by the per-IP limit";
    counter   global_rate_limited_requests  => "flashblocks.proxy.global_rate_limited_requests",
        "Connection attempts rejected by the instance limit";
    counter   unauthorized_requests         => "flashblocks.proxy.unauthorized_requests",
        "Connection attempts rejected for an unknown API key";
    counter   upstream_errors               => "flashblocks.proxy.upstream_errors",
        "Upstream connections that closed or errored";
    gauge     upstream_connections          => "flashblocks.proxy.upstream_connections",
        "Upstream connections currently established";
    counter   upstream_connection_attempts  => "flashblocks.proxy.upstream_connection_attempts",
        "Upstream connection attempts";
    counter   upstream_connection_successes => "flashblocks.proxy.upstream_connection_successes",
        "Upstream connection attempts that succeeded";
    counter   upstream_connection_failures  => "flashblocks.proxy.upstream_connection_failures",
        "Upstream connection attempts that failed";
    counter   bytes_broadcasted             => "flashblocks.proxy.bytes_broadcasted",
        "Payload bytes sent to clients, summed over every client";
    counter   bytes_compressed              => "flashblocks.proxy.bytes_compressed",
        "Brotli-compressed bytes placed on the broadcast channel, counted once per message";
    counter   client_pong_disconnects       => "flashblocks.proxy.client_pong_disconnects",
        "Clients disconnected for missing a pong deadline";
    counter   ping_attempts                 => "flashblocks.proxy.ping_attempts",
        "Pings the upstream subscriber attempted to send";
    counter   ping_failures                 => "flashblocks.proxy.ping_failures",
        "Upstream pings that failed to send";
    counter   ping_sent                     => "flashblocks.proxy.ping_sent",
        "Pings sent to the upstream";
}

impl Metrics {
    /// The `flashblocks.proxy.connections_by_app` series, labelled by app name.
    pub fn connections_by_app(app: String) -> Counter {
        metrics::counter!(CONNECTIONS_BY_APP, "app" => app)
    }

    /// The `flashblocks.proxy.upstream_messages` series, labelled by upstream URL.
    pub fn upstream_messages(upstream: String) -> Counter {
        metrics::counter!(UPSTREAM_MESSAGES, "upstream" => upstream)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    /// Renders a scrape after running [`Metrics::init`] against a recorder of
    /// this test's own, so the assertions do not depend on a global one.
    fn scrape_after_init() -> String {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, Metrics::init);
        handle.render()
    }

    /// Without the zeroing in [`Metrics::init`] a counter that has never fired
    /// is absent from the scrape, and Prometheus answers queries against it
    /// with no data rather than 0. The counters listed here are precisely the
    /// ones that stay at zero in a healthy process, so they are the ones whose
    /// absence would be mistaken for a scrape failure.
    #[test]
    fn init_publishes_a_zero_for_counters_that_never_fire() {
        let scrape = scrape_after_init();

        for series in [
            "flashblocks_proxy_unauthorized_requests",
            "flashblocks_proxy_per_ip_rate_limited_requests",
            "flashblocks_proxy_global_rate_limited_requests",
            "flashblocks_proxy_upstream_errors",
            "flashblocks_proxy_failed_messages",
            "flashblocks_proxy_lagged_connections",
            "flashblocks_proxy_client_pong_disconnects",
            "flashblocks_proxy_ping_failures",
        ] {
            assert!(
                scrape.contains(&format!("{series} 0")),
                "`{series}` should be scraped as 0, not absent:\n{scrape}"
            );
        }
    }

    /// Gauges are zeroed too; histograms have no zero observation to publish.
    #[test]
    fn init_zeroes_gauges_but_not_histograms() {
        let scrape = scrape_after_init();

        for gauge in [
            "flashblocks_proxy_active_connections",
            "flashblocks_proxy_broadcast_queue_size",
            "flashblocks_proxy_upstream_connections",
        ] {
            assert!(scrape.contains(&format!("{gauge} 0")), "`{gauge}` should be scraped as 0");
        }
    }

    /// Every zeroed series carries a HELP line.
    ///
    /// Only zeroed series can be asserted on here: the exporter renders
    /// nothing at all for a series with no observation, so a description
    /// registered for one is held until its first sample. That is why the
    /// histogram and the two labelled series are absent from this list — see
    /// [`described_series_appear_only_once_observed`].
    #[test]
    fn init_registers_a_description_for_every_zeroed_series() {
        let scrape = scrape_after_init();

        for series in [
            "flashblocks_proxy_sent_messages",
            "flashblocks_proxy_failed_messages",
            "flashblocks_proxy_active_connections",
            "flashblocks_proxy_broadcast_queue_size",
            "flashblocks_proxy_bytes_broadcasted",
        ] {
            assert!(
                scrape.contains(&format!("# HELP {series} ")),
                "`{series}` should carry a HELP line:\n{scrape}"
            );
        }
    }

    /// A described-but-unobserved series is absent from the scrape entirely,
    /// and appears with its description once something records against it.
    ///
    /// This is the exporter's behaviour, not a gap in [`Metrics::init`]: the
    /// histogram has no zero to publish, and the labelled series have no known
    /// label values at start-up. It does mean the three of them cannot be used
    /// to check that the proxy is being scraped at all.
    #[test]
    fn described_series_appear_only_once_observed() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            Metrics::init();

            let before = handle.render();
            assert!(
                !before.contains("flashblocks_proxy_connections_by_app"),
                "a labelled series must not be published with invented label values",
            );
            assert!(
                !before.contains("flashblocks_proxy_message_send_duration"),
                "a histogram must not be given a synthetic observation",
            );

            Metrics::connections_by_app("app1".to_owned()).increment(1);
            Metrics::message_send_duration().record(Duration::from_millis(5));
        });

        let after = handle.render();
        assert!(
            after.contains("# HELP flashblocks_proxy_connections_by_app "),
            "the description registered by `init` should surface on first observation:\n{after}"
        );
        assert!(after.contains(r#"flashblocks_proxy_connections_by_app{app="app1"}"#));
        assert!(after.contains("# HELP flashblocks_proxy_message_send_duration "));
    }
}

//! Entry point for the flashblock websocket fan-out proxy.
//!
//! Glue only: parse arguments, stand up the upstream subscribers, the broadcast
//! registry and the downstream server, then wait for any of them to finish or
//! for a signal. All behaviour lives in the library.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use clap::Parser;
use ipnet::IpNet;
use mantle_reth_ws_proxy::{
    Authentication, InMemoryRateLimit, Message, Metrics, RateLimit, Registry, Server,
    SubscriberOptions, TrustedProxyConfig, Uri, WebsocketSubscriber, upstream_listener,
};
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::broadcast,
    time::interval,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};
use tracing_subscriber::EnvFilter;

/// Command-line arguments for the proxy.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Address and port to listen on for downstream connections.
    #[arg(long, env, default_value = "0.0.0.0:8545")]
    listen_addr: SocketAddr,

    /// Websocket URI of the upstream flashblock source — the sequencer.
    ///
    /// One, not a list. The plural form this used to accept promised a
    /// topology the proxy does not implement: each subscriber kept its own
    /// resume position and fed the same broadcast channel, with no
    /// de-duplication anywhere, so two upstreams carrying one stream
    /// delivered every slice twice and interleaved. Redundancy belongs in
    /// front of the proxy, not inside it.
    #[arg(long, env)]
    upstream_ws: Uri,

    /// Messages to buffer for lagging clients.
    ///
    /// Has to clear the producer's archive, not merely approach it. A
    /// reconnecting subscriber is replayed up to `ring_capacity` slices
    /// (32 by default in `mantle-reth-preconf`) with no throttling, and
    /// `broadcast::channel` rounds its capacity up to a power of two — so the
    /// old default of 20 was really 32, exactly the replay burst, leaving no
    /// room for the live slices arriving alongside it. One overflow drops
    /// every downstream client, and they have no resume to recover the gap
    /// with. 128 costs a few hundred KiB.
    #[arg(long, env, default_value = "128")]
    message_buffer_size: usize,

    /// Maximum concurrently connected clients for this instance.
    #[arg(long, env, default_value = "100")]
    instance_connection_limit: usize,

    /// Maximum concurrently connected clients per client IP.
    #[arg(long, env, default_value = "10")]
    per_ip_connection_limit: usize,

    /// Brotli-compress messages sent to downstream clients.
    #[arg(long, env, default_value = "false")]
    enable_compression: bool,

    /// Header consulted to resolve the client's origin IP.
    #[arg(long, env, default_value = "X-Forwarded-For")]
    ip_addr_http_header: String,

    /// Proxy CIDRs trusted when resolving client IPs. Include every forwarding hop.
    #[arg(long, env, value_delimiter = ',', value_parser = TrustedProxyConfig::parse_cidr)]
    trusted_proxy_cidrs: Vec<IpNet>,

    /// API keys as `<app>:<key>` pairs. Required unless `--public-access-enabled`
    /// says the network in front of the proxy is the access control.
    #[arg(long, env, value_delimiter = ',')]
    api_keys: Vec<String>,

    /// Serve unauthenticated clients even when `--api-keys` is set.
    #[arg(long, env, default_value = "false")]
    public_access_enabled: bool,

    /// Honour the address and topic filters on `/ws/{api_key}/filter`.
    ///
    /// Off by default, because both read `metadata.receipts` and
    /// `metadata.new_account_balances`, which the Mantle producer leaves empty
    /// — a filter would silently match nothing rather than filter. With this
    /// off the route still serves unfiltered subscriptions; one carrying
    /// filter parameters is refused with `501` and an explanation. Turn it on
    /// only for an upstream that populates those maps.
    #[arg(long, env, default_value = "false")]
    enable_filters: bool,

    /// Serve a Prometheus endpoint.
    #[arg(long, env, default_value = "true")]
    metrics: bool,

    /// Address for the Prometheus endpoint.
    #[arg(long, env, default_value = "0.0.0.0:9000")]
    metrics_addr: SocketAddr,

    /// Labels added to every metric, as `label1=value1,label2=value2`.
    #[arg(long, env, default_value = "")]
    metrics_global_labels: String,

    /// Upper bound on upstream reconnect backoff, in milliseconds. Must be
    /// greater than zero.
    #[arg(long, env, default_value = "20000")]
    subscriber_max_interval_ms: u64,

    /// Interval between pings sent upstream, in milliseconds.
    #[arg(long, env, default_value = "2000")]
    subscriber_ping_interval_ms: u64,

    /// How long to wait for an upstream pong before declaring the connection
    /// dead. Must exceed `--subscriber-ping-interval-ms`.
    #[arg(long, env, default_value = "4000")]
    subscriber_pong_timeout_ms: u64,

    /// Send ping frames to downstream clients as a health check.
    #[arg(long, env, default_value = "false")]
    client_ping_enabled: bool,

    /// Interval between pings sent to clients, in milliseconds.
    #[arg(long, env, default_value = "15000")]
    client_ping_interval_ms: u64,

    /// How long to wait for a client pong before dropping it, in milliseconds.
    /// With `--client-ping-enabled`, must exceed `--client-ping-interval-ms`.
    #[arg(long, env, default_value = "30000")]
    client_pong_timeout_ms: u64,

    /// Timeout for a single send to a client, in milliseconds.
    #[arg(long, env, default_value = "1000")]
    client_send_timeout_ms: u64,
}

/// Everything a bad flag combination should be refused for, in one place and
/// before the runtime exists — so the message reaches whoever started the
/// process instead of a task nobody is watching.
///
/// Returns the non-empty API keys, since the caller needs them next and
/// filtering them is part of deciding.
fn refuse_unusable_args(args: &Args) -> eyre::Result<Vec<String>> {
    // Refused here, where the message reaches whoever started the process.
    // Three of these fail loudly on their own — the channel's own assertion,
    // or a task whose death cancels the token — but
    // `--client-pong-timeout-ms` below 4 does not: `pong_timeout_ms / 4`
    // rounds to zero, `tokio::time::interval` panics in the per-connection
    // read task, and the proxy goes on accepting connections and answering
    // `/healthz` while every client is disconnected the moment it arrives.
    if args.message_buffer_size == 0 {
        eyre::bail!("--message-buffer-size must be greater than zero");
    }
    if args.client_ping_interval_ms == 0 {
        eyre::bail!("--client-ping-interval-ms must be greater than zero");
    }
    if args.client_pong_timeout_ms < 4 {
        eyre::bail!(
            "--client-pong-timeout-ms must be at least 4: the read task ticks at a quarter of \
             it, and a zero interval is not a valid tokio timer"
        );
    }
    if args.subscriber_ping_interval_ms == 0 {
        eyre::bail!("--subscriber-ping-interval-ms must be greater than zero");
    }
    // The three below start, then cycle a connection forever.
    if args.subscriber_pong_timeout_ms <= args.subscriber_ping_interval_ms {
        eyre::bail!(
            "--subscriber-pong-timeout-ms ({}) must be greater than \
             --subscriber-ping-interval-ms ({}): a pong cannot arrive before the ping it answers, \
             so the upstream connection would be declared dead and reopened forever",
            args.subscriber_pong_timeout_ms,
            args.subscriber_ping_interval_ms,
        );
    }
    if args.subscriber_max_interval_ms == 0 {
        eyre::bail!(
            "--subscriber-max-interval-ms must be greater than zero: it caps the reconnect \
             backoff, so zero reconnects without pause"
        );
    }
    if args.client_ping_enabled && args.client_pong_timeout_ms <= args.client_ping_interval_ms {
        eyre::bail!(
            "--client-pong-timeout-ms ({}) must be greater than --client-ping-interval-ms ({}) \
             when --client-ping-enabled is set: every client would be dropped before its pong \
             is due",
            args.client_pong_timeout_ms,
            args.client_ping_interval_ms,
        );
    }

    // Fail closed. An unauthenticated endpoint is a supported configuration —
    // this proxy runs inside trusted networks — but it is not one a process
    // should fall into by omission. What it serves is the preconfirmation
    // stream: transactions that have been executed and not yet put on chain.
    //
    // `--public-access-enabled` already means "serve unauthenticated clients"
    // when keys are set, so it is the opt-in here too, rather than a second
    // flag saying the same thing.
    let api_keys: Vec<String> = args.api_keys.iter().filter(|s| !s.is_empty()).cloned().collect();
    if api_keys.is_empty() && !args.public_access_enabled {
        eyre::bail!(
            "refusing to start an unauthenticated endpoint: pass --api-keys, or \
             --public-access-enabled to say the network in front of this proxy is the \
             access control"
        );
    }

    Ok(api_keys)
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let api_keys = refuse_unusable_args(&args)?;

    let authentication = if api_keys.is_empty() {
        None
    } else {
        Some(
            Authentication::try_from(api_keys)
                .map_err(|e| eyre::eyre!("failed to parse --api-keys: {e}"))?,
        )
    };

    // Said once, at startup, where an operator reads it. Nothing in this
    // binary speaks TLS — `tokio-tungstenite` is pulled in with default
    // features only, and so is axum's `ws` — so the upstream connection and
    // the downstream listener are both plaintext, and an API key travels in
    // the URL path in the clear. The network has to be the trust boundary;
    // there is no configuration that makes it otherwise.
    warn!(
        target: "ws-proxy",
        "this proxy speaks no TLS: both the upstream connection and the downstream \
         listener are plaintext ws://, so it must run inside a trusted network"
    );

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args, authentication))
}

async fn run(args: Args, authentication: Option<Authentication>) -> eyre::Result<()> {
    if args.metrics {
        info!(target: "ws-proxy", address = %args.metrics_addr, "starting metrics server");

        let mut builder = PrometheusBuilder::new().with_http_listener(args.metrics_addr);
        for (key, value) in parse_global_metrics(&args.metrics_global_labels) {
            builder = builder.add_global_label(key, value);
        }
        builder.install().map_err(|e| eyre::eyre!("failed to install Prometheus endpoint: {e}"))?;

        // Only meaningful once the recorder above is installed: descriptions
        // and zeroes registered earlier would go to a no-op recorder.
        Metrics::init();
    }

    info!(target: "ws-proxy", uri = %args.upstream_ws, "using upstream URI");

    // `_rec` keeps the channel alive through any moment with no clients, and is
    // discounted from the connection gauge by `upstream_listener`.
    let (sender, _rec) = broadcast::channel(args.message_buffer_size);
    let listener = upstream_listener(sender.clone(), args.enable_compression);

    let token = CancellationToken::new();

    let options = SubscriberOptions::default()
        .with_max_backoff_interval(Duration::from_millis(args.subscriber_max_interval_ms))
        .with_ping_interval(Duration::from_millis(args.subscriber_ping_interval_ms))
        .with_pong_timeout(Duration::from_millis(args.subscriber_pong_timeout_ms))
        .with_backoff_initial_interval(Duration::from_millis(500))
        .with_initial_grace_period(Duration::from_secs(5));

    let subscriber_task = {
        let uri = args.upstream_ws.clone();
        let mut subscriber = WebsocketSubscriber::new(uri.clone(), listener, options);
        let token = token.clone();
        tokio::spawn(async move {
            info!(target: "ws-proxy", uri = %uri, "starting subscriber");
            subscriber.run(token).await;
        })
    };

    let ping_task = if args.client_ping_enabled {
        let ping_sender = sender.clone();
        let ping_token = token.clone();
        let ping_interval_ms = args.client_ping_interval_ms;

        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_millis(ping_interval_ms));
            info!(target: "ws-proxy", interval_ms = ping_interval_ms, "starting client ping sender");

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        match ping_sender.send(Message::Ping(vec![].into())) {
                            Ok(_) => {
                                trace!(target: "ws-proxy", "sent ping to all clients");
                                Metrics::broadcast_queue_size().set(ping_sender.len() as f64);
                            }
                            Err(error) => {
                                error!(target: "ws-proxy", %error, "failed to ping clients");
                            }
                        }
                    }
                    _ = ping_token.cancelled() => break,
                }
            }
        })
    } else {
        tokio::spawn(std::future::pending())
    };

    let registry = Registry::new(
        sender,
        args.enable_compression,
        args.client_ping_enabled,
        args.client_pong_timeout_ms,
        Duration::from_millis(args.client_send_timeout_ms),
    );

    let rate_limiter: Arc<dyn RateLimit> = Arc::new(InMemoryRateLimit::new(
        args.instance_connection_limit,
        args.per_ip_connection_limit,
    ));

    let server = Server::new(
        args.listen_addr,
        registry,
        rate_limiter,
        authentication,
        TrustedProxyConfig::new(args.ip_addr_http_header, args.trusted_proxy_cidrs),
        args.public_access_enabled,
        args.enable_filters,
    );
    let server_task = server.listen(token.clone());

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = subscriber_task => {
            info!(target: "ws-proxy", "all subscriber tasks terminated");
        }
        _ = server_task => info!(target: "ws-proxy", "server task terminated"),
        _ = ping_task => info!(target: "ws-proxy", "ping task terminated"),
        _ = interrupt.recv() => info!(target: "ws-proxy", "interrupted, shutting down"),
        _ = terminate.recv() => info!(target: "ws-proxy", "terminated, shutting down"),
    }

    token.cancel();
    Ok(())
}

/// Parses `label1=value1,label2=value2`, skipping malformed entries with a warning.
fn parse_global_metrics(labels: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();

    for entry in labels.split(',') {
        if entry.is_empty() {
            continue;
        }

        let Some((label, value)) = entry.split_once('=') else {
            warn!(target: "ws-proxy", entry, "malformed global metric label: no `=`");
            continue;
        };

        if label.is_empty() || value.is_empty() {
            warn!(target: "ws-proxy", entry, "malformed global metric label: empty side");
            continue;
        }

        result.push((label.to_owned(), value.to_owned()));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_metric_labels_skip_malformed_entries() {
        assert_eq!(parse_global_metrics(""), Vec::<(String, String)>::new());
        assert_eq!(parse_global_metrics("key=value"), vec![("key".into(), "value".into())]);
        assert_eq!(
            parse_global_metrics("key=value,key2=value2"),
            vec![("key".into(), "value".into()), ("key2".into(), "value2".into())]
        );
        assert_eq!(parse_global_metrics("gibberish"), Vec::new());
        assert_eq!(parse_global_metrics("key=value,key2=,"), vec![("key".into(), "value".into())]);
    }

    #[test]
    fn trusted_proxy_cidrs_are_validated() {
        let args = Args::try_parse_from([
            "mantle-ws-proxy",
            "--upstream-ws",
            "ws://sequencer:1111",
            "--trusted-proxy-cidrs",
            "10.0.0.0/8,192.168.0.0/16",
        ])
        .expect("valid CIDRs parse");
        assert_eq!(args.trusted_proxy_cidrs.len(), 2);

        assert!(
            Args::try_parse_from([
                "mantle-ws-proxy",
                "--upstream-ws",
                "ws://sequencer:1111",
                "--trusted-proxy-cidrs",
                "not-a-cidr",
            ])
            .is_err()
        );
    }

    /// `--upstream-ws` takes exactly one URI.
    ///
    /// The comma-separated form used to parse into several, which read as
    /// support for a redundant-source topology the proxy never implemented —
    /// no de-duplication across subscribers, so one stream behind two
    /// upstreams was delivered twice. Refusing it at the flag is the honest
    /// version.
    #[test]
    fn upstream_ws_takes_one_uri_and_refuses_a_list() {
        let one = Args::try_parse_from(["mantle-ws-proxy", "--upstream-ws", "ws://a:1111"])
            .expect("a single URI parses");
        assert_eq!(one.upstream_ws.to_string(), "ws://a:1111/");

        assert!(
            Args::try_parse_from(["mantle-ws-proxy", "--upstream-ws", "ws://a:1111,ws://b:1111"])
                .is_err(),
            "a comma-separated list must not be read as several upstreams",
        );
    }

    /// And the flag is required: a proxy with no upstream has nothing to do.
    #[test]
    fn upstream_ws_is_required() {
        assert!(Args::try_parse_from(["mantle-ws-proxy"]).is_err());
    }

    fn args_with(extra: &[&str]) -> Args {
        let mut argv = vec!["mantle-ws-proxy", "--upstream-ws", "ws://sequencer:1111"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).expect("the base arguments parse")
    }

    /// An endpoint with no keys and no acknowledgement does not start.
    ///
    /// Unauthenticated is a supported configuration — this proxy runs inside
    /// trusted networks — but it has to be asked for. What it serves is the
    /// preconfirmation stream: executed, not-yet-on-chain transactions.
    #[test]
    fn an_unauthenticated_endpoint_has_to_be_asked_for() {
        let err = refuse_unusable_args(&args_with(&[]))
            .expect_err("no keys and no opt-in must not start");
        assert!(
            err.to_string().contains("unauthenticated"),
            "the message has to say what was refused: {err}",
        );

        refuse_unusable_args(&args_with(&["--api-keys", "app:key"]))
            .expect("keys are one way to satisfy it");
        refuse_unusable_args(&args_with(&["--public-access-enabled"]))
            .expect("and saying so explicitly is the other");
    }

    /// `--client-pong-timeout-ms` below 4 is the one of these that does not
    /// fail loudly on its own: `pong_timeout_ms / 4` rounds to zero, the
    /// per-connection read task panics on the zero interval, and the proxy
    /// keeps accepting connections and answering `/healthz` while every
    /// client is disconnected on arrival.
    #[test]
    fn a_pong_timeout_that_rounds_to_a_zero_interval_is_refused() {
        for value in ["0", "1", "3"] {
            assert!(
                refuse_unusable_args(&args_with(&[
                    "--public-access-enabled",
                    "--client-pong-timeout-ms",
                    value,
                ]))
                .is_err(),
                "--client-pong-timeout-ms {value} ticks at zero",
            );
        }
        refuse_unusable_args(&args_with(&[
            "--public-access-enabled",
            "--client-pong-timeout-ms",
            "4",
        ]))
        .expect("4 is the smallest that still ticks");
    }

    /// The three that would fail loudly anyway are refused here too, so the
    /// message names the flag instead of surfacing as a panic.
    #[test]
    fn the_zero_valued_timing_flags_are_refused_by_name() {
        for flag in
            ["--message-buffer-size", "--client-ping-interval-ms", "--subscriber-ping-interval-ms"]
        {
            let err = refuse_unusable_args(&args_with(&["--public-access-enabled", flag, "0"]))
                .expect_err("zero is not a usable value");
            assert!(err.to_string().contains(flag), "the message must name {flag}: {err}");
        }
    }

    /// Combinations that start but cycle a connection forever, and the values
    /// just inside each boundary that do not.
    #[test]
    fn contradictory_timing_flags_are_refused_by_name() {
        let refused = [
            (
                &["--subscriber-ping-interval-ms", "2000", "--subscriber-pong-timeout-ms", "2000"]
                    [..],
                "--subscriber-pong-timeout-ms",
            ),
            (&["--subscriber-max-interval-ms", "0"][..], "--subscriber-max-interval-ms"),
            (
                &[
                    "--client-ping-enabled",
                    "--client-ping-interval-ms",
                    "15000",
                    "--client-pong-timeout-ms",
                    "15000",
                ][..],
                "--client-pong-timeout-ms",
            ),
        ];
        for (flags, named) in refused {
            let mut argv = vec!["--public-access-enabled"];
            argv.extend_from_slice(flags);
            let err = refuse_unusable_args(&args_with(&argv)).expect_err("must not start");
            assert!(err.to_string().contains(named), "the message must name {named}: {err}");
        }

        for flags in [
            &["--subscriber-ping-interval-ms", "2000", "--subscriber-pong-timeout-ms", "2001"][..],
            &["--subscriber-max-interval-ms", "1"][..],
            &[
                "--client-ping-enabled",
                "--client-ping-interval-ms",
                "15000",
                "--client-pong-timeout-ms",
                "15001",
            ][..],
            // Without client pings the pong timeout is not compared.
            &["--client-ping-interval-ms", "15000", "--client-pong-timeout-ms", "15000"][..],
        ] {
            let mut argv = vec!["--public-access-enabled"];
            argv.extend_from_slice(flags);
            refuse_unusable_args(&args_with(&argv)).unwrap_or_else(|e| panic!("{flags:?}: {e}"));
        }
    }
}

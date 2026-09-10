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

    /// Websocket URIs of the upstream flashblock sources. Repeat or comma-separate.
    #[arg(long, env, value_delimiter = ',')]
    upstream_ws: Vec<Uri>,

    /// Messages to buffer for lagging clients.
    #[arg(long, env, default_value = "20")]
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

    /// API keys as `<app>:<key>` pairs. Absent leaves the endpoint unauthenticated.
    #[arg(long, env, value_delimiter = ',')]
    api_keys: Vec<String>,

    /// Serve unauthenticated clients even when `--api-keys` is set.
    #[arg(long, env, default_value = "false")]
    public_access_enabled: bool,

    /// Serve a Prometheus endpoint.
    #[arg(long, env, default_value = "true")]
    metrics: bool,

    /// Address for the Prometheus endpoint.
    #[arg(long, env, default_value = "0.0.0.0:9000")]
    metrics_addr: SocketAddr,

    /// Labels added to every metric, as `label1=value1,label2=value2`.
    #[arg(long, env, default_value = "")]
    metrics_global_labels: String,

    /// Upper bound on upstream reconnect backoff, in milliseconds.
    #[arg(long, env, default_value = "20000")]
    subscriber_max_interval_ms: u64,

    /// Interval between pings sent upstream, in milliseconds.
    #[arg(long, env, default_value = "2000")]
    subscriber_ping_interval_ms: u64,

    /// How long to wait for an upstream pong before declaring the connection dead.
    #[arg(long, env, default_value = "4000")]
    subscriber_pong_timeout_ms: u64,

    /// Send ping frames to downstream clients as a health check.
    #[arg(long, env, default_value = "false")]
    client_ping_enabled: bool,

    /// Interval between pings sent to clients, in milliseconds.
    #[arg(long, env, default_value = "15000")]
    client_ping_interval_ms: u64,

    /// How long to wait for a client pong before dropping it, in milliseconds.
    #[arg(long, env, default_value = "30000")]
    client_pong_timeout_ms: u64,

    /// Timeout for a single send to a client, in milliseconds.
    #[arg(long, env, default_value = "1000")]
    client_send_timeout_ms: u64,
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    if args.upstream_ws.is_empty() {
        eyre::bail!("--upstream-ws is required: the proxy has nothing to subscribe to");
    }

    let api_keys: Vec<String> = args.api_keys.iter().filter(|s| !s.is_empty()).cloned().collect();
    let authentication = if api_keys.is_empty() {
        None
    } else {
        Some(
            Authentication::try_from(api_keys)
                .map_err(|e| eyre::eyre!("failed to parse --api-keys: {e}"))?,
        )
    };

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

    info!(target: "ws-proxy", uris = ?args.upstream_ws, "using upstream URIs");

    // `_rec` keeps the channel alive through any moment with no clients, and is
    // discounted from the connection gauge by `upstream_listener`.
    let (sender, _rec) = broadcast::channel(args.message_buffer_size);
    let listener = upstream_listener(sender.clone(), args.enable_compression);

    let token = CancellationToken::new();
    let mut subscriber_tasks = Vec::new();

    for (index, uri) in args.upstream_ws.iter().enumerate() {
        let options = SubscriberOptions::default()
            .with_max_backoff_interval(Duration::from_millis(args.subscriber_max_interval_ms))
            .with_ping_interval(Duration::from_millis(args.subscriber_ping_interval_ms))
            .with_pong_timeout(Duration::from_millis(args.subscriber_pong_timeout_ms))
            .with_backoff_initial_interval(Duration::from_millis(500))
            .with_initial_grace_period(Duration::from_secs(5));

        let uri = uri.clone();
        let mut subscriber = WebsocketSubscriber::new(uri.clone(), listener.clone(), options);
        let token = token.clone();

        subscriber_tasks.push(tokio::spawn(async move {
            info!(target: "ws-proxy", index, uri = %uri, "starting subscriber");
            subscriber.run(token).await;
        }));
    }

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
    );
    let server_task = server.listen(token.clone());

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = futures::future::join_all(subscriber_tasks) => {
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

    /// `--upstream-ws` is the one argument with no useful default: without it
    /// the proxy would start and serve an empty stream forever.
    #[test]
    fn upstream_ws_accepts_a_comma_separated_list() {
        let args =
            Args::try_parse_from(["mantle-ws-proxy", "--upstream-ws", "ws://a:1111,ws://b:1111"])
                .expect("comma-separated URIs parse");
        assert_eq!(args.upstream_ws.len(), 2);
    }
}

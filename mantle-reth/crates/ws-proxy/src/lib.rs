//! Flashblock websocket fan-out proxy.
//!
//! Holds one upstream connection to the sequencer's flashblock stream and
//! broadcasts each slice to many downstream subscribers, so the sequencer sees
//! a single client regardless of how many RPC nodes and third parties read the
//! stream. Adds the edge concerns the sequencer should not carry: per-IP and
//! global connection limits, API-key authentication, client-IP resolution
//! through trusted forwarding proxies, optional brotli compression, and
//! per-client address/topic filtering.
//!
//! Ported from Base's `websocket-proxy`. The payload is treated as opaque JSON
//! except in [`filter`], which reads `metadata.receipts` to evaluate a
//! subscriber's address and topic filters.

/// The frame type carried on the broadcast channel between the upstream
/// subscriber and the downstream registry, and the upstream address type.
/// Re-exported because [`Registry`] and [`WebsocketSubscriber`] cannot be
/// constructed without naming them.
pub use axum::{extract::ws::Message, http::Uri};

mod auth;
pub use auth::{Authentication, AuthenticationParseError};

mod broadcast;
pub use broadcast::upstream_listener;

mod client;
pub use client::ClientConnection;

mod filter;
pub use filter::{FilterType, MatchMode};

mod metrics;
pub use metrics::Metrics;

mod rate_limit;
pub use rate_limit::{InMemoryRateLimit, RateLimit, RateLimitError, RateLimitType, Ticket};

mod registry;
pub use registry::Registry;

mod server;
pub use server::Server;

mod subscriber;
pub use subscriber::{SubscriberOptions, WebsocketSubscriber};

mod trusted_proxy;
pub use trusted_proxy::{ForwardedClientIpError, TrustedProxyConfig};

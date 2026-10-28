# `mantle-reth-ws-proxy`

Subblock websocket fan-out proxy: one upstream connection to the sequencer's
slice stream, rebroadcast to many downstream subscribers. The crate doc on
`mantle_reth_ws_proxy` (`src/lib.rs`) says what the proxy adds over plain
rebroadcasting and how it treats the payload; this file is the operator's
view — flags, routes, metrics, builds.

"Subblock" is Optimism's current name for what this codebase calls a
flashblock; the two mean the same thing. Optimism renamed the term in its
documentation only, so every identifier here — flag names, metric series, type
names — still reads `flashblock`, and so does the rest of this document.

It is a standalone binary — it is not linked into `op-reth` and does not run
inside the node.

```
sequencer (mantle-reth)  ──→  mantle-ws-proxy  ──→  N subscribers
                              (this crate)          RPC nodes,
                                                    third parties
```

Mantle builds slices inside the sequencer rather than in a sidecar: the
publisher is `MantleFlashblocksPublisher` in `mantle-reth-preconf`, enabled with
`--flashblocks.enable` and listening on `127.0.0.1:1111` by default. Use
`--flashblocks.addr` and `--flashblocks.port` to change the bind address and port.
Base's lineage — this crate is a port of Base's `websocket-proxy` — puts a
separate builder and a rollup-boost hop ahead of the proxy. This document
describes the topology this crate is configured for: the sequencer itself as
the upstream.

## What it adds over plain rebroadcasting

| Concern | Flag |
|---|---|
| Per-IP and per-instance connection limits | `--per-ip-connection-limit`, `--instance-connection-limit` |
| API-key authentication | `--api-keys app:key,...`, `--public-access-enabled` |
| Client IP through forwarding proxies | `--ip-addr-http-header`, `--trusted-proxy-cidrs` |
| Brotli compression downstream | `--enable-compression` |
| Per-subscriber address/topic filtering ⚠️ **not supported — see below** | `--enable-filters` (refused at startup), query parameters on the subscribe URL |
| Upstream keepalive and backoff | `--subscriber-ping-interval-ms`, `--subscriber-pong-timeout-ms`, `--subscriber-max-interval-ms` |
| Downstream keepalive | `--client-ping-enabled`, `--client-ping-interval-ms`, `--client-pong-timeout-ms` |

Run `mantle-ws-proxy --help` for the full list. Every flag also reads from the
equivalent uppercase environment variable.

The keepalive flags are checked at startup, and a combination that would cycle
a connection forever is refused: `--subscriber-pong-timeout-ms` must exceed
`--subscriber-ping-interval-ms`, `--subscriber-max-interval-ms` must be
non-zero, and with `--client-ping-enabled` the client pong timeout must exceed
the client ping interval.

```bash
mantle-ws-proxy \
  --upstream-ws ws://mantle-sequencer:1111 \
  --listen-addr 0.0.0.0:8545 \
  --metrics-addr 0.0.0.0:9000 \
  --api-keys 'indexer:<key>'
```

The upstream URL above assumes the sequencer listens on an address reachable
from the proxy. For separate hosts or containers with separate network
namespaces, add `--flashblocks.addr <sequencer-private-ip>` to the sequencer's
node command, and make sure `mantle-sequencer` resolves to that address. In a
container, `--flashblocks.addr 0.0.0.0` can bind all container interfaces; restrict
access to the publisher port to the intended private network. If both processes
share a network namespace, keep the default bind address and use
`--upstream-ws ws://127.0.0.1:1111` instead.

`--api-keys` is not optional furniture: **the proxy refuses to start without
it**, unless `--public-access-enabled` is passed to say the network in front
of it is the access control. What it serves is the preconfirmation stream —
transactions that have been executed and not yet put on chain — and that is
not something a process should end up publishing by omission.

**There is no TLS anywhere in this binary.** `tokio-tungstenite` and axum's
`ws` are both pulled in with default features, so the upstream connection and
the downstream listener are plaintext `ws://`, and an API key travels in the
URL path in the clear. The network has to be the trust boundary; no
configuration makes it otherwise. The proxy says so in a startup warning.

**Run it behind an ingress or load balancer, and let that layer bound the
connection before the upgrade.** The proxy's own limits — `--instance-connection-limit`
and `--per-ip-connection-limit` — are applied once a request has been parsed,
so a connection that never finishes sending its headers is not counted by
either. Header read timeouts and a cap on open connections are the front
layer's job; the proxy does not implement them.

## Routing depends on whether keys are configured

Authentication decides the base routes; `--public-access-enabled` adds the
public `/ws` on top of them. A subscriber's URL has to match what the proxy
was started with.

| Started with | Routes served |
|---|---|
| no `--api-keys`, with `--public-access-enabled` | `/ws` only |
| `--api-keys` | `/ws/{api_key}` and `/ws/{api_key}/filter` only |
| `--api-keys --public-access-enabled` | all three |

Authentication is by **path segment**, not by header, so any client that can be
handed a URL can present a key. A reth node's consumer has no key flag and does
not need one:

```bash
op-reth node --flashblocks.websocket-url ws://ws-proxy:8545/ws/<key>
```

Two consequences worth planning around:

- `--public-access-enabled` re-adds an **unauthenticated** `/ws` carrying the
  full stream. A third party that can reach the port can bypass keys entirely,
  so do not enable it on an externally reachable listener.
- The key lands in the consumer's command line, since
  `--flashblocks.websocket-url` has no environment-variable form. It is visible
  to `ps` and to anything that logs the process arguments.

## Resolving client IPs behind proxies

Forwarding headers are ignored unless the direct peer falls inside
`--trusted-proxy-cidrs`; with no CIDRs configured they are never honoured,
which is correct for a directly exposed listener. Connection limits then bucket
by the peer address.

When the peer is trusted, the client is picked out of the
`--ip-addr-http-header` chain (default `X-Forwarded-For`) by
`TrustedProxyConfig::forwarded_client_ip`, whose doc gives the scan rule. Two
things follow for configuration:

- `--trusted-proxy-cidrs` must list **every** forwarding hop, not just the
  nearest. The scan peels off the hops it recognises as ours, so a hop missing
  from the list is reported as the client.
- IPv4-mapped IPv6 addresses are canonicalised, so an IPv4 CIDR matches peers
  arriving on a dual-stack listener and rate-limit buckets stay consistent
  across address forms.

## Frame handling

Upstream, text frames are taken as they are. Binary frames are sniffed — already-JSON, or
brotli to be decompressed — before being placed on the broadcast channel, so a
compressing upstream works; `decode_binary_slice` in `subscriber.rs` carries the
sniff rule and the reason dropping binary frames (as Base's proxy does) would
starve every subscriber here. The shared `MAX_DECOMPRESSED_FLASHBLOCK_BYTES`
limit is 17 MiB. Upstream WebSocket messages and individual frames are bounded
by that limit, as is decoded binary content. An oversized raw text message
fails at the transport layer and causes the upstream connection to reconnect.
A compressed message that fits the transport limit but decompresses beyond
17 MiB is dropped without reconnecting. Neither path forwards oversized content.

Downstream, **every slice goes out as a binary frame**, whatever frame type it
arrived in. Without `--enable-compression` the frame carries the slice's JSON
text byte-for-byte as UTF-8; with it, the same JSON brotli-compressed. Clients
must read binary frames — a browser `WebSocket` hands them over as a `Blob` or
`ArrayBuffer`, not a string — and, when compression is on, decompress them. The
flashblocks consumer in `op-reth` accepts either frame type and either encoding,
so it works behind the proxy with or without compression.

## The payload is opaque, with one exception

Beyond that framing, slices are forwarded unchanged. Nothing is validated or
re-signed — the proxy is not a trust boundary, and a downstream consumer must
still replay transactions locally rather than believe what arrives here.

The exception is `FilterType`, which parses the slice as JSON to evaluate a
subscriber's address and topic filters.

### ⚠️ Filters are not supported, because this upstream cannot feed them

Both filter kinds read `metadata.receipts` and `metadata.new_account_balances`,
which the Mantle producer leaves empty on every slice. A topic filter therefore
matches nothing at all, and an address filter answers differently depending on
how the address was written — neither usefully. The derivation lives next to
the code, in the `FilterType` docs in `src/filter.rs`.

Rather than accept a filter and silently return an empty stream,
`/ws/{api_key}/filter` refuses a subscription carrying filter parameters with
`501` and an explanation. The same route still serves an **unfiltered**
subscription, which is what it is without query parameters.
`flashblocks.proxy.filter_subscriptions_refused` counts subscribers who tried.

`--enable-filters` is refused at startup. Besides the missing data, filtering
runs once per filtered client per slice — a full brotli decompress and JSON
parse each time — so CPU grows with the number of filtered clients, and
nothing caps the addresses or topics in one filter. Lifting the refusal needs
all three fixed: a producer that fills those maps, one parse per slice shared
by every client, and a per-filter cap.

Which input feeds which filter, and why each spelling of an address behaves the
way it does, is the module doc on `filter::mantle_shape`.

## Reconnecting after the sequencer archive has advanced

The proxy remembers its last decoded upstream slice and reconnects to the
sequencer with `block_number` and `flashblock_index` query parameters. The
sequencer's archive retains a bounded number of slices. If the requested
position precedes the oldest retained slice, the sequencer sends the retained
slices and then continues live delivery. It increments
`flashblocks.resume_before_archive_total` (`flashblocks_resume_before_archive_total`
in Prometheus) on the sequencer, once per replay attempt detected in that state,
including attempts that later fail to send. This counts attempts, not missing slices.
Successful catch-up with a position before the archive also produces a warning.

The stream carries no explicit archive-gap message or close reason. Downstream
consumers must check slice indices and predecessor links, recover missing history
from the canonical chain, and wait for a usable base before rebuilding a preview
when the retained slices cannot extend their state. Reconnecting with the same
old position cannot retrieve slices already evicted from the archive.

## Metrics

Prometheus on `--metrics-addr`, series prefixed `flashblocks.proxy.`. Note this
differs from Base's `websocket_proxy_*`, so Base dashboards need their queries
rewritten.

`Metrics::init` publishes a zero for every counter and gauge at start-up, so a
series reading zero means "nothing has happened" rather than "not scraped".
Three series are the exception and stay absent until first observed:
`message_send_duration`, `connections_by_app` and `upstream_messages`. Do not
use those three to check that scraping works.

Useful first checks:

| Series | Reading |
|---|---|
| `flashblocks.proxy.upstream_messages` | **Absent** — not zero — means the upstream has delivered nothing; the proxy sends no subscribe request, it only reads. Labelled `upstream=<host:port>`: the path and query, which can carry an API key, are never used in labels or logs |
| `flashblocks.proxy.upstream_connection_failures` | Growing means the upstream URI or network is wrong |
| `flashblocks.proxy.active_connections` | Current downstream subscribers |
| `flashblocks.proxy.lagged_connections` | Subscribers dropped for falling more than `--message-buffer-size` behind |
| `flashblocks.proxy.per_ip_rate_limited_requests` | Rejections from `--per-ip-connection-limit` |

## Building and testing

The binary's package is not in the workspace's `default-members`, so `-p` is
required — without it `cargo` reports `no bin target named mantle-ws-proxy`.

```bash
# Build the binary; lands in target/release/mantle-ws-proxy
cargo build --release -p mantle-reth-ws-proxy --bin mantle-ws-proxy

# Unit tests, including the metrics-registration cases
cargo test -p mantle-reth-ws-proxy

# The two integration suites: the sequencer→proxy→consumer bridge, and the
# server's routing and admission behaviour
cargo test -p mantle-reth-integration-tests --test flashblocks ws_proxy
```

The integration suites bind loopback sockets. A local HTTP proxy exported into
the environment will intercept those connections and the tests will fail with
errors unrelated to the code; unset the proxy variables for the run:

```bash
env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
    -u all_proxy -u ALL_PROXY \
    cargo test -p mantle-reth-integration-tests --test flashblocks ws_proxy
```

# `mantle-reth-ws-proxy`

Flashblock websocket fan-out proxy. Holds one upstream connection to the
sequencer's slice stream and rebroadcasts each slice to many downstream
subscribers, so the sequencer serves a single client no matter how many RPC
nodes and third parties read the stream.

Ported from Base's `websocket-proxy`. It is a standalone binary — it is not
linked into `op-reth` and does not run inside the node.

```
producer (op-rbuilder)  ──→  rollup-boost  ──→  mantle-ws-proxy  ──→  N subscribers
                                                (this crate)          RPC nodes,
                                                                      third parties
```

## What it adds over plain rebroadcasting

| Concern | Flag |
|---|---|
| Per-IP and per-instance connection limits | `--per-ip-connection-limit`, `--instance-connection-limit` |
| API-key authentication | `--api-keys app:key,...`, `--public-access-enabled` |
| Client IP through forwarding proxies | `--ip-addr-http-header`, `--trusted-proxy-cidrs` |
| Brotli compression downstream | `--enable-compression` |
| Per-subscriber address/topic filtering | query parameters on the subscribe URL |
| Upstream keepalive and backoff | `--subscriber-ping-interval-ms`, `--subscriber-pong-timeout-ms`, `--subscriber-max-interval-ms` |
| Downstream keepalive | `--client-ping-enabled`, `--client-ping-interval-ms`, `--client-pong-timeout-ms` |

Run `mantle-ws-proxy --help` for the full list. Every flag also reads from the
equivalent uppercase environment variable.

```bash
mantle-ws-proxy \
  --upstream-ws ws://mantle-rollup-boost:1111 \
  --listen-addr 0.0.0.0:8545 \
  --metrics-addr 0.0.0.0:9000
```

## Routing depends on whether keys are configured

The two routing tables are disjoint — no configuration serves both, so a
subscriber's URL has to match the mode the proxy was started in.

| Started with | Routes served |
|---|---|
| no `--api-keys` | `/ws` only |
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

When the peer is trusted, every occurrence of `--ip-addr-http-header`
(default `X-Forwarded-For`) is concatenated in order into one chain, which is
scanned **right to left**, skipping addresses inside the trusted CIDRs. The
first untrusted address is taken as the client; if every hop is trusted,
resolution falls back to the peer address with a warning. Scanning right to
left peels off each of our own hops rather than stopping at the innermost one,
so clients arriving through several trusted hops still resolve individually.

`--trusted-proxy-cidrs` must therefore list **every** forwarding hop, not just
the nearest. IPv4-mapped IPv6 addresses are canonicalised, so an IPv4 CIDR
matches peers arriving on a dual-stack listener and rate-limit buckets stay
consistent across address forms.

## Frame handling

Text frames are forwarded as-is. Binary frames are sniffed the same way the
consumer's `try_decode_message` does it — a first non-whitespace byte of `{`
means the frame is already JSON, anything else is brotli — and decompressed
before being placed on the broadcast channel. A compressing upstream therefore
works; dropping binary frames, as Base's proxy does, would starve every
subscriber because a compressed slice is never valid UTF-8 and so never
arrives as a text frame.

Slices larger than 5 MiB are refused, matching the consumer's
`MAX_DECOMPRESSED_FLASHBLOCK_BYTES`. The check runs both before and after
decompression, so a small highly-compressible frame cannot force a large
allocation.

Downstream compression is separate and opt-in via `--enable-compression`.

## The payload is opaque, with one exception

Beyond that framing, slices are forwarded unchanged. Nothing is validated or
re-signed — the proxy is not a trust boundary, and a downstream consumer must
still replay transactions locally rather than believe what arrives here.

The exception is `filter`, which parses the slice as JSON to evaluate a
subscriber's address and topic filters.

### ⚠️ Two of the three filter inputs are empty on Mantle today

| Input | Used for | State on a Mantle slice |
|---|---|---|
| `metadata.new_account_balances` keys | address filters | **always empty** |
| `metadata.receipts[*].logs[*].{address,topics}` | address and topic filters | **always empty** |
| `diff.transactions` hex, substring match | address filters, last resort | populated |

Both metadata maps are `BTreeMap::default()` on every slice the Mantle producer
emits. They exist so the payload stays shaped like the upstream one; their doc
comments say consumers must not read them.

The consequences differ by filter kind:

- **Topic filters match nothing.** Topics only ever come from receipt logs.
- **Address filters fall back to substring matching over the raw transaction
  hex.** That still returns results, but it matches an address appearing
  anywhere in calldata, not just as a log emitter — so it over-matches rather
  than under-matches. A subscriber filtering on one contract will also receive
  slices whose transactions merely mention it as an argument.

Neither case produces an error on either side.
`filter::mantle_shape::an_empty_receipts_map_matches_nothing` locks the
receipts half so it is visible rather than surprising.

**For whoever implements the producer side:**

1. Populate `metadata.receipts` when building each slice.
2. `filter::mantle_shape` is the contract. Those tests build a real
   `MantleFlashblockPayload`, serialise it, and assert the filter matches — if
   they pass, this proxy will filter correctly.
3. `an_empty_receipts_map_matches_nothing` will start failing once the map is
   populated. That failure is the intended signal, not a regression: replace it
   with the populated-map assertion.
4. Keep the "consumers must not read it" guidance on the field. Once populated
   it carries *sequencer-claimed* receipts. This proxy may read them to route,
   because a wrong routing decision only costs a subscriber some slices. A
   consumer that believed them would be trusting unverified state.

### Two receipt encodings are accepted

`OpReceipt` serialises flat, with `logs` directly on the receipt:

```json
{ "type": "0x7e", "status": "0x1", "cumulativeGasUsed": "0x5208", "logs": [ ... ] }
```

Base's `BaseReceipt` is an externally tagged enum, one level deeper:

```json
{ "Deposit": { "logs": [ ... ] } }
```

`FilterType::receipt_logs` reads both, so the proxy works behind either a Mantle
or a Base-derived upstream. Reading only the nested form — as the code did when
first ported — makes every filter silently match nothing against a Mantle slice.

## Metrics

Prometheus on `--metrics-addr`, series prefixed `flashblocks.proxy.`. Note this
differs from Base's `websocket_proxy_*`, so Base dashboards need their queries
rewritten.

Every counter and gauge is published as `0` at start-up, so a series reading
zero means "nothing has happened" rather than "not scraped". Three series are
the exception and stay absent until first observed, because there is nothing to
publish for them up front: `message_send_duration` (a histogram has no zero
observation) and the two labelled series `connections_by_app` and
`upstream_messages` (their label values are unknown until a client connects or
an upstream delivers). Do not use those three to check that scraping works.

Useful first checks:

| Series | Reading |
|---|---|
| `flashblocks.proxy.upstream_messages` | **Absent** — not zero — means the upstream has delivered nothing; the proxy sends no subscribe request, it only reads |
| `flashblocks.proxy.upstream_connection_failures` | Growing means the upstream URI or network is wrong |
| `flashblocks.proxy.active_connections` | Current downstream subscribers |
| `flashblocks.proxy.lagged_connections` | Subscribers dropped for falling more than `--message-buffer-size` behind |
| `flashblocks.proxy.per_ip_rate_limited_requests` | Rejections from `--per-ip-connection-limit` |

## Building and testing

The binary is not in the workspace's default-run set, so `-p` is required —
without it `cargo` reports `no bin target named mantle-ws-proxy`.

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

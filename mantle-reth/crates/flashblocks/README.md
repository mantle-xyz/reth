# Flashblocks preview RPC behavior

Flashblocks expose a provisional view of the block being built. The canonical
chain remains authoritative; a preview may be replaced or discarded before
the block is accepted.

## Subscriptions

`eth_subscribe("newFlashblocks")` returns the latest complete RPC preview block
after each update. Each notification is a snapshot, not an incremental slice.
The block hash is zero because the preview has no canonical block hash yet.
The notification has no slice index or predecessor link and does not guarantee
reliable delivery or detect missing slices. After reconnecting, query the
current preview and reconcile previously observed transactions with the
canonical chain.

Consumers that need to reconstruct individual slices must use the sequencer's
raw flashblock WebSocket stream and validate its indices and predecessor links.
That stream also has a bounded replay archive; reconnection cannot recover
slices that have already been evicted.

## State overrides

Pending execution starts from one coherent preview snapshot, including its
execution header. Caller-supplied account overrides take precedence field by
field: overriding only a balance preserves that account's preview nonce, code
and storage. Explicit zero balances, zero nonces and empty code still override
the corresponding preview fields.

`state` replaces the account's storage; `stateDiff` changes only the supplied
slots. Supplying both for the same account is invalid. These rules apply to
pending calls, gas estimates and simulations using the shared pending context.

## Data availability fields

Under Jovian, `blobGasUsed` carries the protocol's data availability footprint
rather than Ethereum blob-transaction usage. Execution gas and data availability
footprint have independent allowances, each bounded by the same numeric block
gas limit; their sum is not compared with that limit.

A normal continuation can reuse cached execution and receipts. Canonical
reorganization and rebuilds beyond the retained preview depth discard the old
execution snapshot and re-execute against the new canonical state. Reusing a
receipt during a normal continuation therefore does not require executing its
transaction again solely to recompute `blobGasUsed`.

## Simulation timestamps

The default interval between simulated blocks comes from the chain's block-time
hint, unless the caller provides a timestamp override. It is not universally
two seconds: the development chain with ID 1337 uses a one-second interval,
while Mantle mainnet and Sepolia use two seconds.

## Metrics

`flashblocks.consumer.upstream_messages_total` counts every item read from the
upstream connection, not only slices: text and binary frames, pings, pongs,
close frames and read errors all add one, as do frames that fail to decode
(those also add to `flashblocks.consumer.upstream_decode_errors_total`). Its
help text says "Slices received from upstream"; read it as messages read. The
`_count` of `flashblocks.consumer.block_processing_duration` counts slices
actually folded into the preview.

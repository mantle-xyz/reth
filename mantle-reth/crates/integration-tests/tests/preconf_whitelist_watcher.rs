//! Isolated process: the whitelist watcher's reload counters must not include
//! other node tests, whose reorgs would move them too.

#![recursion_limit = "1024"]
#![allow(missing_docs)]

#[path = "preconf/helpers.rs"]
#[allow(dead_code)]
pub mod helpers;

use std::time::Duration;

use alloy_primitives::{Address, B256, Bytes, U256};
use helpers::{
    PreconfCfgBuilder, address_array_storage, l1_attrs, layout_version_storage,
    mantle_chain_spec_with_account, pair_array_storage,
};
use mantle_reth_preconf::{
    FROM_WILDCARDS_SLOT, LISTS_HASH_SLOT, PAIRS_SLOT, TO_WILDCARDS_SLOT, Whitelist,
    seed_preconf_metrics,
};
use reth_node_metrics::recorder::install_prometheus_recorder;

const WL: Address = Address::new([0xc0; 20]);
const CHAIN_ID: u64 = 5000;
const SKIPPED: &str = "reth_preconf_whitelist_reload_skipped_total";
const RELOADED: &str = "reth_preconf_whitelist_reload_ok";

/// Polls `scrape` until `metric` reads `want`.
async fn await_counter(scrape: impl Fn() -> String, metric: &str, want: u64) -> bool {
    let line = format!("{metric} {want}");
    for _ in 0..200 {
        if scrape().lines().any(|l| l == line) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn counter(scrape: impl Fn() -> String, metric: &str) -> Option<u64> {
    scrape().lines().find_map(|l| l.strip_prefix(metric)?.strip_prefix(' ')?.parse().ok())
}

/// A reorg that leaves `listsHash` where it was skips the full re-read; once the
/// shared lists carry a different hash, the next reorg re-reads them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reorg_rereads_the_allowlist_only_when_lists_hash_moved() {
    let metrics = install_prometheus_recorder().handle();
    let scrape = || metrics.render();
    // The binary does this at startup; the harness does not.
    seed_preconf_metrics();

    let on_chain = B256::repeat_byte(0xa0);
    let mut storage =
        pair_array_storage(PAIRS_SLOT, &[(Address::new([0x11; 20]), Address::new([0x22; 20]))]);
    storage.extend(address_array_storage(FROM_WILDCARDS_SLOT, &[]));
    storage.extend(address_array_storage(TO_WILDCARDS_SLOT, &[]));
    storage.push(layout_version_storage());
    storage.push((B256::from(U256::from(LISTS_HASH_SLOT)), on_chain));
    // Never called; it only has to hold code for the cold start to accept it.
    let spec = mantle_chain_spec_with_account(CHAIN_ID, WL, &Bytes::from_static(&[0x00]), &storage);

    let (node, _http, _wallet, _, classifier, _fifo, _fb) = launch_preconf_node!(
        @build PreconfCfgBuilder::new().whitelist_contract(WL).build(), spec,
        |svc| mantle_reth_cli::node::MantleNode::default().with_preconf(svc)
    )
    .await;
    assert_eq!(classifier.whitelist_snapshot().lists_hash, on_chain, "cold start read the hash");

    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let pid = fcu_v3_start!(node, genesis, l1_attrs(0, 1));
    let block = get_payload_v5!(node, pid);
    canonize_built!(node, block);

    // A marker the chain does not hold, under the hash the chain does: a full
    // re-read would drop it.
    let marker = (Address::new([0x55; 20]), Address::new([0x66; 20]));
    let mut current = (*classifier.whitelist_snapshot()).clone();
    current.pairs.insert(marker);
    classifier.install_whitelist(current);

    // Reorg: a sibling of that block on genesis.
    let pid = fcu_v3_start!(node, genesis, l1_attrs(0, 2));
    let sibling = get_payload_v5!(node, pid);
    canonize_built!(node, sibling);

    assert!(await_counter(scrape, SKIPPED, 1).await, "the reorg was skipped by listsHash");
    assert_eq!(counter(scrape, RELOADED), Some(0), "and nothing was re-read");
    assert!(classifier.whitelist_snapshot().pairs.contains(&marker));

    // Same marker, a hash the chain does not hold: the next reorg re-reads.
    classifier.install_whitelist(Whitelist {
        lists_hash: B256::repeat_byte(0xff),
        ..(*classifier.whitelist_snapshot()).clone()
    });
    let pid = fcu_v3_start!(node, genesis, l1_attrs(0, 3));
    let another = get_payload_v5!(node, pid);
    canonize_built!(node, another);

    assert!(await_counter(scrape, RELOADED, 1).await, "a moved hash is re-read");
    assert_eq!(counter(scrape, SKIPPED), Some(1));
    let reread = classifier.whitelist_snapshot();
    assert!(!reread.pairs.contains(&marker), "the re-read installed the chain's lists");
    assert_eq!(reread.lists_hash, on_chain);
}

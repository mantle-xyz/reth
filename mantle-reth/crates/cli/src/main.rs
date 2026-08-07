#![allow(missing_docs, rustdoc::missing_crate_level_docs)]

use clap::Parser;
use mantle_reth_cli::{
    MantleArgs, MantleChainSpecParser, MantleNode, seed_blockchain_tree_metrics,
};
use mantle_reth_preconf::{PreconfServiceBuilder, seed_preconf_metrics};
use tracing::info;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[cfg(all(feature = "jemalloc-prof", unix))]
#[unsafe(export_name = "_rjem_malloc_conf")]
static MALLOC_CONF: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

fn main() {
    reth_cli_util::sigsegv_handler::install();
    mantle_reth_cli::version::init_mantle_version();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }

    if let Err(err) = reth_optimism_cli::Cli::<MantleChainSpecParser, MantleArgs>::parse().run(
        async move |builder, args| {
            info!(target: "reth::cli", "Launching Mantle node");
            let mut node = MantleNode::new(args.rollup);
            let preconf_cfg = args.preconf.into_config();
            let preconf_enabled = preconf_cfg.is_some();
            match preconf_cfg {
                Some(cfg) => {
                    let all = cfg.all_preconfs;
                    let journal = cfg.journal_path.clone();
                    let svc = PreconfServiceBuilder::from_config(cfg)
                        .await
                        .map_err(|e| eyre::eyre!("preconf service init: {e}"))?;
                    node = node.with_preconf(svc);
                    info!(
                        target: "reth::cli",
                        "Mantle preconf ENABLED (all_preconfs={all}, journal={journal:?})",
                    );
                }
                None => {
                    info!(
                        target: "reth::cli",
                        "Mantle preconf DISABLED (pass --preconf.enable to opt in)",
                    );
                }
            }
            let handle = builder
                .node(node)
                .on_node_started(move |full_node| {
                    seed_blockchain_tree_metrics(&full_node.provider);
                    // Pre-register preconf metric series (0 baseline) so alerts
                    // don't see "no data" before the first emit. Preconf-only.
                    if preconf_enabled {
                        seed_preconf_metrics();
                    }
                    Ok(())
                })
                .launch()
                .await?;
            handle.node_exit_future.await
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

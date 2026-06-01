#![allow(missing_docs, rustdoc::missing_crate_level_docs)]

use clap::Parser;
use mantle_reth_cli::{MantleChainSpecParser, MantleNode};
use reth_optimism_node::args::RollupArgs;
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

    if let Err(err) = reth_optimism_cli::Cli::<MantleChainSpecParser, RollupArgs>::parse().run(
        async move |builder, args| {
            info!(target: "reth::cli", "Launching Mantle node");
            let handle = builder.node(MantleNode::new(args)).launch().await?;
            handle.node_exit_future.await
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

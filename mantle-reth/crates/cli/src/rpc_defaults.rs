//! Mantle's defaults for reth's RPC server flags.

use reth_node_core::args::DefaultRpcServerArgs;

/// The RPC gas cap a Mantle node starts with: the L2 block gas limit on Mantle mainnet and
/// Sepolia. It bounds `eth_call` and the `eth_estimateGas` search ceiling; reth's own default
/// (50M) would refuse calls and estimates a block can hold.
///
/// Raise it with the block gas limit. `--rpc.gascap` still overrides it per node.
pub const MANTLE_RPC_GAS_CAP: u64 = 60_000_000;

/// Installs Mantle's defaults for the RPC server flags.
///
/// Must be called **before** `Cli::parse()`: clap reads the defaults when it builds the command.
pub fn init_rpc_defaults() {
    // Only fails when the defaults are already installed, which leaves them in place.
    let _ = DefaultRpcServerArgs::default().with_rpc_gas_cap(MANTLE_RPC_GAS_CAP).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use reth_node_core::args::RpcServerArgs;

    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        rpc: RpcServerArgs,
    }

    #[test]
    fn the_rpc_gas_cap_defaults_to_the_mantle_block_gas_limit() {
        init_rpc_defaults();
        let command = Command::try_parse_from(["op-reth"]).unwrap();
        assert_eq!(command.rpc.rpc_gas_cap, MANTLE_RPC_GAS_CAP);
    }
}

//! Reproduces Mantle Sepolia block 286456 receipt-root behavior.
//!
//! Block 286456 (hash `0xa088c560…`) contains a FAILED deposit at tx index 1
//! (`0xbcdbae6a…`, `to` = L2StandardBridge `0x42..10`, `status = 0`) that mints
//! `eth_value` of BVM_ETH AND transfers it to the bridge. Per the OP deposit
//! spec op-geth runs the BVM_ETH mint AND the (successful) transfer *before*
//! taking its revert snapshot, so BOTH the `Mint` and `Transfer` event logs
//! survive the failure and end up in the receipt (2 logs).
//!
//! This is the case the first fix (`v98-mantle-arsia.2`) did NOT cover: that fix
//! re-surfaced only the `Mint` log (mint-only path), because the mainnet block it
//! was validated against (96442768) had a transfer that did NOT succeed, so only
//! the mint persisted. Here the transfer succeeds (transfer == mint == 0.001
//! ETH), so the canonical receipt carries Mint + Transfer.
//!
//! This test pins the causality at the receipts-root layer:
//!  - Mint + Transfer  -> root == canonical op-geth header root (the full fix)
//!  - Mint only        -> the wrong root reth computed & rejected on-chain
//!  - no logs          -> a different wrong root (the original pre-fix bug)
//!
//! Companion to the op-revm execution test
//! `test_failed_deposit_persists_mint_and_transfer_when_transfer_succeeds`,
//! which proves op-revm actually emits both logs once the transfer-log fix
//! (mantle-xyz/revm `fix/failed-deposit-transfer-log`) ships.

use alloy_consensus::Receipt;
use alloy_primitives::{b256, hex, Bytes, Log, LogData, B256};
use op_alloy_consensus::OpDepositReceipt;
use reth_optimism_chainspec::MANTLE_SEPOLIA;
use reth_optimism_consensus::calculate_receipt_root_no_memo_optimism;
use reth_optimism_primitives::OpReceipt;

const BLOCK_286456_TIMESTAMP: u64 = 1_781_028_032;

/// Canonical header `receiptsRoot` (op-geth) for Sepolia block 286456
/// (hash `0xa088c560…`).
const HEADER_RECEIPTS_ROOT: B256 =
    b256!("0x9f29d9b81a94d5ab324194493550e842b43a66fe2bf32c0bf3a29474de96d6f8");

/// The root reth logged & rejected on-chain (`receipt root mismatch ... got
/// 0xd1939af6...`). It is the **empty-logs** root — i.e. produced by a reth with
/// NO failed-deposit log fix at all (mint log dropped): reth `v1.9.3-mantle-arsia.2`,
/// which pins revm `v98-mantle-arsia.1` (cold-reset only, before the mint-log fix).
const RETH_FORK_ROOT_NO_LOGS: B256 =
    b256!("0xd1939af61deb278177b875b36eb990a7d1d1db0070044fb9adaba05f41b78b36");

/// The root a mint-only fix (reth `v1.9.3-mantle-arsia.3` -> revm
/// `v98-mantle-arsia.2`) produces: the Mint log is surfaced but the Transfer log
/// is still missing, so it STILL differs from the canonical root and still forks.
const MINT_ONLY_ROOT: B256 =
    b256!("0x57a968ee59d0868151e9eaf5eade1ce60f95d90d54ce6a75230b17dd00a0965b");

/// BVM_ETH `Mint(address,uint256)`: minter = 0x7466be34…, value = 0.001 ETH.
fn mint_log() -> Log {
    Log {
        address: hex!("dEAddEaDdeadDEadDEADDEAddEADDEAddead1111").into(),
        data: LogData::new_unchecked(
            vec![
                b256!("0x0f6798a560793a54c3bcfe86a93cde1e73087d944c0ea20544137d4121396885"),
                b256!("0x0000000000000000000000007466be349b17a0f966f97ddeefe393894b9faf06"),
            ],
            Bytes::copy_from_slice(&hex!(
                "00000000000000000000000000000000000000000000000000038d7ea4c68000"
            )),
        ),
    }
}

/// BVM_ETH `Transfer(address,address,uint256)`: 0x7466be34… -> L2StandardBridge
/// (0x42..10), value = 0.001 ETH.
fn transfer_log() -> Log {
    Log {
        address: hex!("dEAddEaDdeadDEadDEADDEAddEADDEAddead1111").into(),
        data: LogData::new_unchecked(
            vec![
                b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"),
                b256!("0x0000000000000000000000007466be349b17a0f966f97ddeefe393894b9faf06"),
                b256!("0x0000000000000000000000004200000000000000000000000000000000000010"),
            ],
            Bytes::copy_from_slice(&hex!(
                "00000000000000000000000000000000000000000000000000038d7ea4c68000"
            )),
        ),
    }
}

#[derive(Clone, Copy)]
enum FailedDepositLogs {
    /// op-geth parity: mint + successful transfer both persist.
    MintAndTransfer,
    /// `v98-mantle-arsia.2`: only the mint log re-surfaced.
    MintOnly,
    /// Pre-fix reth: the halt dropped all logs.
    None,
}

/// The two deposit receipts of block 286456, exactly as observed on-chain
/// (`eth_getBlockReceipts`), with the failed deposit's log set toggled.
fn block_receipts(logs: FailedDepositLogs) -> Vec<OpReceipt> {
    let failed_logs = match logs {
        FailedDepositLogs::MintAndTransfer => vec![mint_log(), transfer_log()],
        FailedDepositLogs::MintOnly => vec![mint_log()],
        FailedDepositLogs::None => vec![],
    };
    vec![
        // tx[0]: L1-attributes system deposit (to = 0x42..15), succeeds, no logs.
        OpReceipt::Deposit(OpDepositReceipt {
            inner: Receipt { status: true.into(), cumulative_gas_used: 57_475, logs: vec![] },
            deposit_nonce: Some(0x45ef7),
            deposit_receipt_version: None,
        }),
        // tx[1]: failed user deposit to L2StandardBridge (status = 0); the BVM_ETH
        // mint and the successful transfer both persist into the receipt.
        OpReceipt::Deposit(OpDepositReceipt {
            inner: Receipt { status: false.into(), cumulative_gas_used: 83_705, logs: failed_logs },
            deposit_nonce: Some(0x8d),
            deposit_receipt_version: None,
        }),
    ]
}

fn root(logs: FailedDepositLogs) -> B256 {
    calculate_receipt_root_no_memo_optimism(
        &block_receipts(logs),
        MANTLE_SEPOLIA.as_ref(),
        BLOCK_286456_TIMESTAMP,
    )
}

#[test]
fn mantle_sepolia_286456_failed_deposit_keeps_mint_and_transfer_log() {
    let full = root(FailedDepositLogs::MintAndTransfer);
    let mint_only = root(FailedDepositLogs::MintOnly);
    let empty = root(FailedDepositLogs::None);
    println!("mint+transfer = {full:?}");
    println!("mint-only     = {mint_only:?}");
    println!("no logs       = {empty:?}");

    // The full fix: only with BOTH the Mint and Transfer logs does the receipts
    // root match the canonical op-geth header.
    assert_eq!(
        full, HEADER_RECEIPTS_ROOT,
        "failed deposit must keep BOTH Mint and Transfer logs for the root to match op-geth"
    );

    // Mint-only (reth v1.9.3-mantle-arsia.3 / revm v98-mantle-arsia.2): the Mint
    // log is surfaced but the Transfer log is still dropped, so the root STILL
    // differs from canonical -> this block still forks until the transfer-log fix.
    assert_eq!(mint_only, MINT_ONLY_ROOT, "mint-only root is stable/known");
    assert_ne!(mint_only, HEADER_RECEIPTS_ROOT, "mint-only is NOT sufficient for this block");

    // No logs (reth v1.9.3-mantle-arsia.2 / revm v98-mantle-arsia.1): reproduces
    // the exact wrong root reth logged & rejected on-chain (0xd1939af6...).
    assert_eq!(
        empty, RETH_FORK_ROOT_NO_LOGS,
        "empty-logs root reproduces the receipt root reth rejected on-chain"
    );
    assert_ne!(empty, HEADER_RECEIPTS_ROOT, "empty logs must NOT match canonical");
}

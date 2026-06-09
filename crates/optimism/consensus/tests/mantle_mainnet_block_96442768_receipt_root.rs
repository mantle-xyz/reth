//! Reproduces Mantle mainnet block 96442768 receipt-root behavior.
//!
//! Block 96442768 contains a FAILED deposit (tx index 1: `to` = EOA, empty
//! calldata, `eth_value` + `eth_tx_value` set, `status = 0`). Per the OP deposit
//! spec the BVM_ETH mint still persists on failure, and op-geth emits the ERC20
//! `Mint` event *before* taking its revert snapshot, so the failed-deposit
//! receipt keeps that one log.
//!
//! op-revm used to return `ExecutionResult::Halt` with no logs for a failed
//! deposit, so reth built a receipt with empty logs / zero bloom and computed a
//! receipts root that differed from op-geth -> the block was rejected
//! (`receipt root mismatch`) and the node forked.
//!
//! This test pins the causality at the receipts-root layer: with the BVM_ETH
//! `Mint` log present the root equals the canonical header root; without it the
//! root equals the wrong root reth rejected on-chain.

use alloy_consensus::Receipt;
use alloy_primitives::{b256, hex, Bytes, Log, LogData, B256};
use op_alloy_consensus::OpDepositReceipt;
use reth_optimism_chainspec::MANTLE_MAINNET;
use reth_optimism_consensus::calculate_receipt_root_no_memo_optimism;
use reth_optimism_primitives::OpReceipt;

const BLOCK_96442768_TIMESTAMP: u64 = 1_781_015_848;

/// Canonical header `receiptsRoot` (built by the op-geth sequencer) for mainnet
/// block 96442768.
const HEADER_RECEIPTS_ROOT: B256 =
    b256!("0x8a7630cea4e4adc07e8b680213ce46ceccc440935a138f0f34b33a19218ee666");

/// The root reth computed (and rejected) before the fix — from the
/// `receipt root mismatch` newPayload error and the forked node's own block.
const RETH_FORK_ROOT: B256 =
    b256!("0xc241638060c10d591d9e3beda456b402c9326ad349fa1439e20720cacc9d607b");

/// The single BVM_ETH `Mint(address,uint256)` log emitted on the failed deposit
/// (tx index 1, hash `0x6b7cbddd…7482`). minter = `0xc214…8de4`, value = 0.001 ETH.
fn bvm_eth_mint_log() -> Log {
    Log {
        address: hex!("dEAddEaDdeadDEadDEADDEAddEADDEAddead1111").into(),
        data: LogData::new_unchecked(
            vec![
                // keccak("Mint(address,uint256)")
                b256!("0x0f6798a560793a54c3bcfe86a93cde1e73087d944c0ea20544137d4121396885"),
                // indexed minter address
                b256!("0x000000000000000000000000c214b42e093c7739179833496791fbd50ec68de4"),
            ],
            // 0x38d7ea4c68000 = 1_000_000_000_000_000 wei (0.001 ETH)
            Bytes::copy_from_slice(&hex!(
                "00000000000000000000000000000000000000000000000000038d7ea4c68000"
            )),
        ),
    }
}

/// The two deposit receipts of block 96442768, as reth's executor builds them.
///
/// `failed_deposit_keeps_mint_log` toggles the failed deposit's log set:
/// `true` = the fixed behavior (op-geth parity), `false` = the pre-fix reth bug.
fn block_receipts(failed_deposit_keeps_mint_log: bool) -> Vec<OpReceipt> {
    vec![
        // tx[0]: L1-attributes system deposit (to = 0x42..15), succeeds, no logs.
        OpReceipt::Deposit(OpDepositReceipt {
            inner: Receipt { status: true.into(), cumulative_gas_used: 57_499, logs: vec![] },
            deposit_nonce: Some(0x21a_30a6),
            deposit_receipt_version: None,
        }),
        // tx[1]: failed user deposit (status = 0); the BVM_ETH mint persists, and
        // with it the `Mint` event log.
        OpReceipt::Deposit(OpDepositReceipt {
            inner: Receipt {
                status: false.into(),
                cumulative_gas_used: 157_499,
                logs: if failed_deposit_keeps_mint_log {
                    vec![bvm_eth_mint_log()]
                } else {
                    vec![]
                },
            },
            deposit_nonce: Some(0),
            deposit_receipt_version: None,
        }),
    ]
}

#[test]
fn mantle_mainnet_96442768_failed_deposit_mint_log_matches_receipt_root() {
    // With the BVM_ETH `Mint` log on the failed deposit (the fix), the receipts
    // root matches the canonical op-geth header.
    let root_with_log = calculate_receipt_root_no_memo_optimism(
        &block_receipts(true),
        MANTLE_MAINNET.as_ref(),
        BLOCK_96442768_TIMESTAMP,
    );
    assert_eq!(
        root_with_log, HEADER_RECEIPTS_ROOT,
        "failed-deposit BVM_ETH Mint log must be in the receipt for the root to match op-geth"
    );

    // Dropping that log (the pre-fix reth behavior) reproduces the exact wrong
    // root that caused the on-chain fork at block 96442768.
    let root_without_log = calculate_receipt_root_no_memo_optimism(
        &block_receipts(false),
        MANTLE_MAINNET.as_ref(),
        BLOCK_96442768_TIMESTAMP,
    );
    assert_ne!(
        root_without_log, HEADER_RECEIPTS_ROOT,
        "an empty failed-deposit receipt must NOT match the canonical root"
    );
    assert_eq!(
        root_without_log, RETH_FORK_ROOT,
        "dropping the Mint log reproduces the receipt root reth rejected on mainnet"
    );
}

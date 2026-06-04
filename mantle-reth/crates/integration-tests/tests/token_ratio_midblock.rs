#![allow(clippy::doc_markdown)]
//! Regression test for the mid-block `token_ratio` over-credit on the L1FeeVault.
//!
//! ## What this reproduces
//!
//! On Mantle, an operator tx that calls the gas oracle (0x42..000F) updates the
//! MNT/ETH `token_ratio`. By Mantle's rule, the new ratio applies to *subsequent*
//! txs in the same block. op-revm implements this by calling `reset_l2_block()` in
//! `validate_initial_tx_gas` when a tx targets the gas oracle, so the *next* tx
//! re-`try_fetch`es `L1BlockInfo` (and thus the freshly-SSTORE'd ratio).
//!
//! In op-revm 19.0.0 (`mantle-elysium`, e637f61e) a second refetch guard was added
//! to `validate_against_state_and_deduct_caller`:
//!
//! ```ignore
//! if chain.l2_block != Some(block.number()) {
//!     *chain = L1BlockInfo::try_fetch(journal.db_mut(), block.number(), spec)?;
//! }
//! ```
//!
//! This guard runs inside the *same* gas-oracle tx, *before* that tx's SSTORE, so it
//! re-pins `l2_block` to the current block with the stale (pre-update) ratio —
//! undoing the `reset_l2_block()`. The next tx then never refetches and is L1-fee'd
//! at the stale ratio, over-crediting the L1FeeVault (0x42..001A).
//!
//! op-revm 12.0.2 (`v98-mantle-arsia.1`, `v2.2.2`) has **no** such guard in
//! `validate_against_state_and_deduct_caller`, so the reset survives and the next tx
//! refetches the updated ratio — no over-credit.
//!
//! ## The two runs, condensed
//!
//! Two txs in one block; base L1 fee = 96 per ratio unit; ratio 2984 -> 2980:
//! * canonical (v98 / op-geth / reth receipt path): 96*2984 + 96*2980 = 572544
//! * elysium execution (this binary): 96*2984 + 96*2984 = 572928  (+384)
//!
//! The assertion below pins the **canonical** value, so on the current elysium
//! op-revm this test fails by exactly +384 (documenting the bug); once the upstream
//! op-revm ordering is fixed it passes.
//!
//! Run with:
//!   cargo test -p mantle-reth-integration-tests --test token_ratio_midblock -- --nocapture

use alloy_primitives::{Address, Bytes, U256};
use op_revm::{
    DefaultOp, OpBuilder, OpSpecId, OpTransaction,
    constants::{
        ECOTONE_L1_FEE_SCALARS_SLOT, GAS_ORACLE_CONTRACT, L1_BASE_FEE_SLOT, L1_BLOCK_CONTRACT,
        L1_FEE_RECIPIENT, TOKEN_RATIO_SLOT,
    },
    transaction::deposit::DepositTransactionParts,
};
use revm::{
    ExecuteCommitEvm, ExecuteEvm,
    bytecode::Bytecode,
    context::{BlockEnv, Context, TxEnv},
    database::{CacheDB, EmptyDB},
    primitives::{KECCAK_EMPTY, TxKind, keccak256},
    state::AccountInfo,
};

const RATIO_BEFORE: u64 = 2984;
const RATIO_AFTER: u64 = 2980;
/// L1 fee per token_ratio unit produced by the storage config below:
/// estimated_size_scaled(1e8) * l1_fee_scaled(960000) / 1e12 = 96.
const BASE_L1_FEE: u64 = 96;
const BLOCK_NUMBER: u64 = 87_212;

/// Gas-oracle bytecode that SSTOREs the new ratio (2980) to slot 0:
/// PUSH2 0x0BA4; PUSH1 0x00; SSTORE; STOP
const ORACLE_SET_RATIO_2980: [u8; 7] = [0x61, 0x0b, 0xa4, 0x60, 0x00, 0x55, 0x00];

/// Gas-oracle bytecode that decrements the stored ratio by 4 each call, so a
/// sequence of oracle txs walks slot 0 down (2984 -> 2980 -> 2976 ...):
/// PUSH1 0x00; SLOAD; PUSH1 0x04; SWAP1; SUB; PUSH1 0x00; SSTORE; STOP
/// stack: [0]->[v]->[v,4]->[4,v]->[v-4]->[v-4,0]-> SSTORE(slot 0 = v-4)
const ORACLE_DEC_RATIO_4: [u8; 11] =
    [0x60, 0x00, 0x54, 0x60, 0x04, 0x90, 0x03, 0x60, 0x00, 0x55, 0x00];
const RATIO_DEC_STEP: u64 = 4;

/// Short non-deposit envelope (first byte != 0x7E) that compresses well below the
/// MIN_TX_SIZE floor, so `estimate_tx_compressed_size` returns exactly 1e8.
fn short_envelope() -> Bytes {
    Bytes::from(vec![0x02u8; 40])
}

fn user_tx(caller: Address, to: Address, nonce: u64) -> OpTransaction<TxEnv> {
    OpTransaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(to),
            gas_limit: 200_000,
            gas_price: 0,
            gas_priority_fee: Some(0),
            value: U256::ZERO,
            data: Bytes::new(),
            nonce,
            chain_id: Some(5000),
            ..Default::default()
        },
        enveloped_tx: Some(short_envelope()),
        deposit: DepositTransactionParts::default(),
    }
}

/// Build a CacheDB with a funded caller, the gas oracle (given bytecode + starting
/// ratio at slot 0), and an L1Block contract configured so each L1 fee unit is
/// exactly `BASE_L1_FEE` per token_ratio unit (l1_base_fee=1, base-fee scalar=60000,
/// blob terms=0; the short envelope floors the compressed size to 1e8).
fn build_db(caller: Address, initial_ratio: u64, oracle_code: &[u8]) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());

    db.insert_account_info(
        caller,
        AccountInfo {
            balance: U256::from(u128::MAX),
            nonce: 0,
            code_hash: KECCAK_EMPTY,
            code: None,
            account_id: None,
        },
    );

    let code = Bytecode::new_raw(Bytes::copy_from_slice(oracle_code));
    db.insert_account_info(
        GAS_ORACLE_CONTRACT,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 0,
            code_hash: keccak256(oracle_code),
            code: Some(code),
            account_id: None,
        },
    );
    db.insert_account_storage(GAS_ORACLE_CONTRACT, TOKEN_RATIO_SLOT, U256::from(initial_ratio))
        .unwrap();

    db.insert_account_info(
        L1_BLOCK_CONTRACT,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 0,
            code_hash: KECCAK_EMPTY,
            code: None,
            account_id: None,
        },
    );
    db.insert_account_storage(L1_BLOCK_CONTRACT, L1_BASE_FEE_SLOT, U256::from(1u64)).unwrap();
    db.insert_account_storage(
        L1_BLOCK_CONTRACT,
        ECOTONE_L1_FEE_SCALARS_SLOT,
        U256::from(60_000u64) << 96,
    )
    .unwrap();

    db
}

fn block_env() -> BlockEnv {
    BlockEnv {
        number: U256::from(BLOCK_NUMBER),
        beneficiary: "0x3333333333333333333333333333333333333333".parse().unwrap(),
        timestamp: U256::from(1_776_841_200u64),
        gas_limit: 30_000_000,
        basefee: 0,
        ..Default::default()
    }
}

#[test]
#[ignore = "requires op-revm fix (remove stale try_fetch guard in validate_against_state_and_deduct_caller)"]
fn token_ratio_midblock_l1_fee_vault_no_overcredit() {
    let caller: Address = "0x1111111111111111111111111111111111111111".parse().unwrap();
    let eoa: Address = "0x2222222222222222222222222222222222222222".parse().unwrap();

    // Gas oracle holds the starting ratio (2984) and bytecode that lowers it to 2980.
    let db = build_db(caller, RATIO_BEFORE, &ORACLE_SET_RATIO_2980);

    let mut evm = Context::op()
        .with_db(db)
        .with_block(block_env())
        .modify_cfg_chained(|cfg| {
            cfg.spec = OpSpecId::ARSIA;
            cfg.chain_id = 5000;
        })
        .build_op();

    // tx1: operator tx calling the gas oracle -> SSTORE ratio 2984 -> 2980.
    let r1 = evm.transact(user_tx(caller, GAS_ORACLE_CONTRACT, 0)).expect("tx1 executes");
    assert!(r1.result.is_success(), "tx1 (gas-oracle update) should succeed");
    let tx1_l1 = r1.state.get(&L1_FEE_RECIPIENT).map(|a| a.info.balance).unwrap_or_default();
    evm.commit(r1.state);

    // tx2: ordinary user tx -> its L1 fee must use the *updated* ratio 2980.
    let r2 = evm.transact(user_tx(caller, eoa, 1)).expect("tx2 executes");
    assert!(r2.result.is_success(), "tx2 (user transfer) should succeed");
    let total_l1 = r2.state.get(&L1_FEE_RECIPIENT).map(|a| a.info.balance).unwrap_or_default();
    let tx2_l1 = total_l1 - tx1_l1;

    let canonical = U256::from(BASE_L1_FEE * (RATIO_BEFORE + RATIO_AFTER)); // 572544
    let elysium_bug = U256::from(BASE_L1_FEE * (RATIO_BEFORE + RATIO_BEFORE)); // 572928

    println!(
        "L1FeeVault: tx1={tx1_l1} tx2={tx2_l1} total={total_l1} | canonical(v98)={canonical} \
         elysium_bug={elysium_bug} delta={}",
        elysium_bug - canonical,
    );

    // tx1 is charged at the pre-update ratio under every version (the update applies to the
    // *next* tx), so this holds regardless of the bug.
    assert_eq!(
        tx1_l1,
        U256::from(BASE_L1_FEE * RATIO_BEFORE),
        "tx1 L1 fee must use pre-update ratio {RATIO_BEFORE}"
    );

    // The regression guard: tx2 must be charged at the post-update ratio 2980. On the buggy
    // elysium op-revm this fails by exactly +384 (tx2 charged at 2984).
    assert_eq!(
        total_l1, canonical,
        "L1FeeVault over-credit: tx2 was charged at the stale token_ratio. \
         expected canonical {canonical} (96*({RATIO_BEFORE}+{RATIO_AFTER})), got {total_l1}"
    );
}

/// Per-tx L1-fee correctness across a `U U | O | U U | O | U U` sequence with two
/// mid-block gas-oracle updates (ratio 2984 -> 2980 -> 2976, decremented by the
/// `ORACLE_DEC_RATIO_4` bytecode).
///
/// Mantle's rule: a gas-oracle SSTORE applies to *subsequent* txs. So each tx's L1
/// fee must use the ratio in effect *at the start of that tx*:
/// * the oracle tx itself is charged at the pre-update ratio;
/// * the next user txs are charged at the post-update ratio.
///
/// This asserts the canonical (v98) per-tx fee for every tx. On the buggy elysium
/// op-revm the first user tx *after each* oracle update is charged at the stale
/// ratio (the deduct-caller refetch guard undoes `reset_l2_block` before the
/// oracle's own SSTORE), so this fails at tx3 by +384.
#[test]
#[ignore = "requires op-revm fix (remove stale try_fetch guard in validate_against_state_and_deduct_caller)"]
fn token_ratio_multi_oracle_per_tx_l1_fee_correct() {
    let caller: Address = "0x1111111111111111111111111111111111111111".parse().unwrap();
    let eoa: Address = "0x2222222222222222222222222222222222222222".parse().unwrap();

    const R0: u64 = 2984;
    const R1: u64 = R0 - RATIO_DEC_STEP; // 2980, after oracle #1
    const R2: u64 = R1 - RATIO_DEC_STEP; // 2976, after oracle #2

    // (is_oracle, ratio_in_effect_at_tx_start)
    let steps: [(bool, u64); 8] = [
        (false, R0), // tx0 user
        (false, R0), // tx1 user
        (true, R0),  // tx2 oracle: charged at R0, then SSTORE R0->R1
        (false, R1), // tx3 user  (first tx after update #1)
        (false, R1), // tx4 user
        (true, R1),  // tx5 oracle: charged at R1, then SSTORE R1->R2
        (false, R2), // tx6 user  (first tx after update #2)
        (false, R2), // tx7 user
    ];

    let db = build_db(caller, R0, &ORACLE_DEC_RATIO_4);
    let mut evm = Context::op()
        .with_db(db)
        .with_block(block_env())
        .modify_cfg_chained(|cfg| {
            cfg.spec = OpSpecId::ARSIA;
            cfg.chain_id = 5000;
        })
        .build_op();

    let mut prev_vault = U256::ZERO;
    let mut results: Vec<(usize, bool, u64, U256, U256)> = Vec::with_capacity(steps.len());
    for (i, &(is_oracle, ratio)) in steps.iter().enumerate() {
        let to = if is_oracle { GAS_ORACLE_CONTRACT } else { eoa };
        let r = evm.transact(user_tx(caller, to, i as u64)).expect("tx executes");
        assert!(r.result.is_success(), "tx{i} should succeed");

        let vault = r.state.get(&L1_FEE_RECIPIENT).map(|a| a.info.balance).unwrap_or_default();
        let got = vault - prev_vault;
        let want = U256::from(BASE_L1_FEE * ratio);
        results.push((i, is_oracle, ratio, got, want));

        prev_vault = vault;
        evm.commit(r.state);
    }

    // Print the full per-tx table first so a single run shows the complete elysium
    // signature (oracle txs self-correct via the refetch; only the user txs that
    // follow each update are over-charged at the stale ratio).
    for &(i, is_oracle, ratio, got, want) in &results {
        println!(
            "tx{i} {:<6} ratio_expected={ratio} l1_fee got={got} want={want}{}",
            if is_oracle { "oracle" } else { "user" },
            if got == want { "" } else { "  <-- MISMATCH" },
        );
    }

    for &(i, is_oracle, ratio, got, want) in &results {
        assert_eq!(
            got,
            want,
            "tx{i} ({}) L1 fee must use the ratio in effect at tx start ({ratio}); \
             got {got}, want {want} (= {BASE_L1_FEE}*{ratio})",
            if is_oracle { "oracle" } else { "user" },
        );
    }
}

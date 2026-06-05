//! Mainnet transaction replay tests.
//!
//! Replays real Mantle mainnet transactions through op-revm's `OpHandler`
//! (the same execution path reth uses internally) and verifies gasUsed
//! matches on-chain receipts exactly.
//!
//! State is loaded from pre-fetched JSON fixtures (generated via
//! `debug_traceTransaction` + `prestateTracer`), so tests run offline
//! with no RPC dependency.
//!
//! Run with:
//!   cargo test -p mantle-reth-integration-tests --test replay -- --nocapture

use alloy_op_hardforks::{MANTLE_MAINNET_ARSIA_TIMESTAMP, MANTLE_MAINNET_SKADI_TIMESTAMP};
use alloy_primitives::{Address, B256, Bytes, U256};
use op_revm::{
    DefaultOp, L1BlockInfo, OpBuilder, OpSpecId, OpTransaction, handler::OpHandler,
    transaction::deposit::DepositTransactionParts,
};
use revm::{
    context::Context,
    context_interface::result::EVMError,
    database::CacheDB,
    handler::{EthFrame, Handler},
    interpreter::interpreter::EthInterpreter,
};
use serde_json::Value;

fn spec_for_timestamp(ts: u64) -> OpSpecId {
    if ts >= MANTLE_MAINNET_ARSIA_TIMESTAMP {
        OpSpecId::ARSIA
    } else if ts >= MANTLE_MAINNET_SKADI_TIMESTAMP {
        OpSpecId::ISTHMUS
    } else {
        OpSpecId::BEDROCK
    }
}

fn parse_u256(v: &Value) -> U256 {
    let s = v.as_str().unwrap_or("0x0");
    U256::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or_default()
}

/// Assert the decoded `L1BlockInfo` scalars and the computed L1 fee against the
/// chain's own receipt-decoded oracle values. The maxed sender balance masks any
/// L1-fee error in the gasUsed/status checks, so these assertions are what actually
/// exercise scalar extraction. Only present on post-Arsia, non-deposit fixtures.
fn assert_l1_oracle(fixture: &Value, l1: &L1BlockInfo, raw_tx: &Bytes, spec_id: OpSpecId) {
    let tag = fixture["tx_hash"].as_str().unwrap();
    if let Some(v) = fixture.get("expected_l1_base_fee_scalar") {
        assert_eq!(l1.l1_base_fee_scalar, parse_u256(v), "{tag}: l1_base_fee_scalar");
    }
    if let Some(v) = fixture.get("expected_l1_blob_base_fee_scalar") {
        assert_eq!(
            l1.l1_blob_base_fee_scalar.unwrap_or_default(),
            parse_u256(v),
            "{tag}: l1_blob_base_fee_scalar"
        );
    }
    if let Some(v) = fixture.get("expected_operator_fee_scalar") {
        assert_eq!(
            l1.operator_fee_scalar.unwrap_or_default(),
            parse_u256(v),
            "{tag}: operator_fee_scalar"
        );
    }
    if let Some(v) = fixture.get("expected_operator_fee_constant") {
        assert_eq!(
            l1.operator_fee_constant.unwrap_or_default(),
            parse_u256(v),
            "{tag}: operator_fee_constant"
        );
    }
    if let Some(v) = fixture.get("expected_da_footprint_gas_scalar").and_then(Value::as_u64) {
        assert_eq!(
            l1.da_footprint_gas_scalar.map(u64::from).unwrap_or_default(),
            v,
            "{tag}: da_footprint_gas_scalar"
        );
    }
    if let Some(v) = fixture.get("expected_l1_fee") {
        let mut l1c = l1.clone();
        let got = l1c.calculate_tx_l1_cost(raw_tx, spec_id);
        assert_eq!(got, parse_u256(v), "{tag}: l1_fee");
    }
}

fn load_prestate(
    prestate: &serde_json::Map<String, Value>,
    sender: Address,
    db: &mut CacheDB<revm::database::EmptyDB>,
) {
    for (addr_str, state) in prestate {
        let addr: Address = addr_str.parse().expect("valid address");

        let mut balance = state.get("balance").map(parse_u256).unwrap_or_default();

        if addr == sender {
            balance = U256::from(u128::MAX);
        }

        let nonce = state.get("nonce").and_then(|v| v.as_u64()).unwrap_or(0);

        let code_bytes = state
            .get("code")
            .and_then(|v| v.as_str())
            .filter(|s| s.len() > 2)
            .map(|s| alloy_primitives::hex::decode(s.trim_start_matches("0x")).unwrap_or_default())
            .unwrap_or_default();

        let (code, code_hash) = if code_bytes.is_empty() {
            (None, revm::primitives::KECCAK_EMPTY)
        } else {
            (
                Some(revm::bytecode::Bytecode::new_raw(Bytes::from(code_bytes.clone()))),
                revm::primitives::keccak256(&code_bytes),
            )
        };

        db.insert_account_info(
            addr,
            revm::state::AccountInfo { balance, nonce, code_hash, code, account_id: None },
        );

        if let Some(storage) = state.get("storage").and_then(|v| v.as_object()) {
            for (slot_str, val) in storage {
                let slot =
                    U256::from_str_radix(slot_str.trim_start_matches("0x"), 16).unwrap_or_default();
                let value = parse_u256(val);
                db.insert_account_storage(addr, slot, value).expect("insert storage");
            }
        }
    }
}

fn replay_fixture(fixture_json: &str) {
    let fixture: Value = serde_json::from_str(fixture_json).expect("valid fixture JSON");

    let block_number = fixture["block_number"].as_u64().unwrap();
    let timestamp = fixture["timestamp"].as_u64().unwrap();
    let expected_gas_used = fixture["expected_gas_used"].as_u64().unwrap();
    let sender: Address = fixture["sender"].as_str().unwrap().parse().unwrap();
    let spec_id = spec_for_timestamp(timestamp);

    let raw_tx_bytes = Bytes::from(
        alloy_primitives::hex::decode(fixture["raw_tx"].as_str().unwrap().trim_start_matches("0x"))
            .unwrap(),
    );

    // Load prestate into CacheDB
    let mut cache_db = CacheDB::new(revm::database::EmptyDB::default());
    let prestate = fixture["prestate"].as_object().unwrap();
    load_prestate(prestate, sender, &mut cache_db);

    // Build L1BlockInfo via op-revm's canonical loader (`try_fetch`), which decodes the
    // packed scalar storage words by byte offset exactly as production reth does. We inject
    // the raw system-contract slots (fetched at this block) into a throwaway DB and let
    // try_fetch decode them, making scalar mis-extraction structurally impossible. A separate
    // DB is used so the execution cache_db is never polluted: the L1-attributes deposit tx
    // itself writes these slots, and overwriting them would corrupt its SSTORE gas accounting.
    let l1i = &fixture["l1_block_info"];
    let l1_block_contract: Address = "0x4200000000000000000000000000000000000015".parse().unwrap();
    let gas_oracle_contract: Address =
        "0x420000000000000000000000000000000000000F".parse().unwrap();
    let mut l1_db = CacheDB::new(revm::database::EmptyDB::default());
    for (addr, slot, key) in [
        (l1_block_contract, 1u64, "l1_base_fee"),
        (l1_block_contract, 3, "ecotone_scalars"),
        (l1_block_contract, 5, "l1_fee_overhead"),
        (l1_block_contract, 6, "l1_fee_scalar"),
        (l1_block_contract, 7, "blob_base_fee"),
        (l1_block_contract, 8, "operator_fee_packed"),
        (gas_oracle_contract, 0, "token_ratio"),
    ] {
        l1_db
            .insert_account_storage(addr, U256::from(slot), parse_u256(&l1i[key]))
            .expect("insert l1 slot");
    }
    let l1_block_info =
        L1BlockInfo::try_fetch(&mut l1_db, U256::from(block_number), spec_id).expect("try_fetch");

    assert_l1_oracle(&fixture, &l1_block_info, &raw_tx_bytes, spec_id);

    // Build block env
    let benv = &fixture["block_env"];
    let block_env = revm::context::BlockEnv {
        number: U256::from(block_number),
        beneficiary: benv["beneficiary"].as_str().unwrap().parse().unwrap(),
        timestamp: U256::from(timestamp),
        gas_limit: benv["gas_limit"].as_u64().unwrap() as u64,
        basefee: benv["base_fee_per_gas"].as_u64().unwrap(),
        prevrandao: Some(benv["mix_hash"].as_str().unwrap().parse().unwrap()),
        ..Default::default()
    };

    // Build transaction env
    let txd = &fixture["tx"];
    let to: Option<Address> = fixture["to"].as_str().map(|s| s.parse().unwrap());
    // A `source_hash` field marks a deposit (type 0x7e) fixture; op-revm keys
    // deposit handling off a non-zero source_hash.
    let deposit = match fixture["source_hash"].as_str() {
        Some(sh) => DepositTransactionParts {
            source_hash: sh.parse::<B256>().unwrap(),
            mint: fixture["mint"].as_u64().map(u128::from),
            is_system_transaction: fixture["is_system_transaction"].as_bool().unwrap_or(false),
            ..Default::default()
        },
        None => DepositTransactionParts::default(),
    };
    let op_tx = OpTransaction {
        base: revm::context::TxEnv {
            caller: sender,
            kind: match to {
                Some(a) => revm::primitives::TxKind::Call(a),
                None => revm::primitives::TxKind::Create,
            },
            gas_limit: txd["gas_limit"].as_u64().unwrap(),
            gas_price: txd["max_fee_per_gas"].as_u64().unwrap() as u128,
            gas_priority_fee: Some(txd["max_priority_fee_per_gas"].as_u64().unwrap() as u128),
            value: parse_u256(&txd["value"]),
            data: Bytes::from(
                alloy_primitives::hex::decode(
                    txd["input"].as_str().unwrap().trim_start_matches("0x"),
                )
                .unwrap(),
            ),
            nonce: txd["nonce"].as_u64().unwrap(),
            chain_id: Some(txd["chain_id"].as_u64().unwrap()),
            ..Default::default()
        },
        enveloped_tx: Some(raw_tx_bytes),
        deposit,
    };

    // Execute
    let ctx = Context::op()
        .with_db(cache_db)
        .with_chain(l1_block_info)
        .with_block(block_env)
        .modify_cfg_chained(|cfg| {
            cfg.spec = spec_id;
            cfg.chain_id = 5000;
        })
        .with_tx(op_tx);

    let mut evm = ctx.build_op();
    let mut handler = OpHandler::<
        _,
        EVMError<_, op_revm::transaction::error::OpTransactionError>,
        EthFrame<EthInterpreter>,
    >::new();

    let result = handler.run(&mut evm);

    match result {
        Ok(ref exec_result) => {
            let actual_gas = exec_result.tx_gas_used();
            let status = exec_result.is_success();
            println!(
                "TX {}: block={}, spec={:?}, expected={}, actual={}, status={}",
                fixture["tx_hash"].as_str().unwrap(),
                block_number,
                spec_id,
                expected_gas_used,
                actual_gas,
                status
            );
            let expected_status = fixture["expected_status"].as_bool().unwrap_or(true);
            assert_eq!(
                status, expected_status,
                "status mismatch: expected={}, actual={}",
                expected_status, status
            );
            assert_eq!(
                expected_gas_used, actual_gas,
                "gasUsed mismatch: expected={}, actual={}",
                expected_gas_used, actual_gas
            );
        }
        Err(e) => {
            panic!("TX {} replay failed: {:?}", fixture["tx_hash"].as_str().unwrap(), e);
        }
    }
}

// ============ Test Cases ============

#[test]
fn replay_skadi_swap() {
    replay_fixture(include_str!("fixtures/skadi_swap.json"));
}

#[test]
fn replay_limb_transfer() {
    replay_fixture(include_str!("fixtures/limb_transfer.json"));
}

#[test]
fn replay_arsia_eip1559() {
    replay_fixture(include_str!("fixtures/arsia_eip1559.json"));
}

#[test]
fn replay_arsia_single_user_tx() {
    replay_fixture(include_str!("fixtures/arsia_single.json"));
}

/// Replay an Arsia-era deposit transaction (type 0x7e, the per-block L1-attributes tx).
#[test]
fn replay_arsia_deposit() {
    replay_fixture(include_str!("fixtures/arsia_deposit.json"));
}

/// Replay an Arsia-era reverted transaction (on-chain status=0, gas still consumed).
#[test]
fn replay_arsia_revert() {
    replay_fixture(include_str!("fixtures/arsia_revert.json"));
}

/// Replay an Arsia-era contract deployment (CREATE, to == null).
#[test]
fn replay_arsia_create() {
    replay_fixture(include_str!("fixtures/arsia_create.json"));
}

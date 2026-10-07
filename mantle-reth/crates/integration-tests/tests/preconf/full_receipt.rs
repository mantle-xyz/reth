//! The preconf receipt against the one the chain ends up recording.
//!
//! Every field asserted here is also a field of `eth_getTransactionReceipt`.
//! The fork exists so that preconf transactions execute against the same
//! in-flight state the block is built from; that is only worth anything if the
//! two receipts agree, so these compare them directly rather than checking
//! shapes.
//!
//! **Known limitation of the L1 fee comparison.** [`chain_spec_with_l1_fee_scalars`]
//! seeds the `L1Block` predeploy's Ecotone fee-scalar storage directly,
//! because this predeploy's deployed bytecode predates the Jovian/Arsia
//! `setL1BlockValues*` selectors — there is no transaction this test (or
//! production, against this same snapshot) could send to set them instead.
//! That makes `assert_receipts_agree`'s whole-`l1_block_info` comparison
//! weaker than it looks: of the twelve fields it compares, `l1BaseFeeScalar`,
//! `l1BlobBaseFee`, `l1BlobBaseFeeScalar` and `daFootprintGasScalar` are
//! hand-matched constants (seeded on one side, described by [`l1_info_tx`]'s
//! calldata on the other) and prove nothing beyond "two literals copied from
//! each other are equal", and `operatorFeeScalar` / `operatorFeeConstant` are
//! `0` on both sides and vanish to `None` at the wire before the comparison
//! even runs. Only `tokenRatio`, `l1Fee`, `l1GasUsed` and `l1GasPrice` are
//! computed independently on each side and so carry real evidence that the
//! shared `OpReceiptFieldsBuilder` and the token-ratio ordering are correct.
//! A reader should not take "twelve fields compared" to mean "twelve fields
//! tested".

use super::helpers::{PreconfCfgBuilder, mantle_chain_spec_with_predeploys_for, send_preconf};
use crate::{canonicalize_payload, launch_preconf_node};
use alloy_consensus::TxReceipt;
use alloy_genesis::Genesis;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, address};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::{PreconfStatus, PreconfTxEvent};
use op_alloy_consensus::TxDeposit;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_optimism_chainspec::OpChainSpec;
use std::sync::Arc;

/// `L1Block`'s Ecotone fee-scalar inputs this test's genesis seeds — see
/// [`chain_spec_with_l1_fee_scalars`] and [`l1_info_tx`].
const L1_BASE_FEE_SCALAR: u32 = 1_368;
const L1_BLOB_BASE_FEE_SCALAR: u32 = 810_949;
const L1_BLOB_BASE_FEE: u64 = 1;
/// `L1Block`'s Jovian DA-footprint-gas-scalar input — see the same two
/// functions. Packed into the same slot as the (unused, left zero) Isthmus
/// operator fee scalar/constant.
const DA_FOOTPRINT_GAS_SCALAR: u16 = 100;

/// [`super::helpers::mantle_chain_spec_with_predeploys_for`], with `L1Block`'s
/// Ecotone-era fee-scalar slot (`ECOTONE_L1_FEE_SCALARS_SLOT`) and blob-base-fee
/// slot (`ECOTONE_L1_BLOB_BASE_FEE_SLOT`) pre-populated instead of left at their
/// default zero.
///
/// This predeploy's deployed implementation predates Arsia/Jovian — none of
/// the four `setL1BlockValues*` selectors exist in its bytecode (verified
/// against `tests/assets/predeploys.json`'s `code` field) — so nothing in this
/// test, or in production against this same snapshot, can ever populate those
/// slots via a transaction. Left at zero, `op_revm::L1BlockInfo::try_fetch`
/// reads that as "Ecotone scalars not yet set" and falls back to adding a
/// legacy `l1FeeOverhead` field that no Jovian/Arsia-format calldata can
/// carry — making an exact match against `eth_getTransactionReceipt`'s
/// calldata-parsed L1 fields structurally impossible regardless of what
/// `l1_info_tx` sends. Seeding the slots here, and having `l1_info_tx`'s
/// calldata describe the same numbers, closes that gap without needing the
/// (nonexistent) setter to actually run.
fn chain_spec_with_l1_fee_scalars(chain_id: u64) -> Arc<OpChainSpec> {
    let base = mantle_chain_spec_with_predeploys_for(chain_id);
    let mut genesis: serde_json::Value =
        serde_json::to_value(base.genesis()).expect("chain spec genesis serialises");

    let mut fee_scalars_slot = [0u8; 32];
    fee_scalars_slot[16..20].copy_from_slice(&L1_BASE_FEE_SCALAR.to_be_bytes());
    fee_scalars_slot[20..24].copy_from_slice(&L1_BLOB_BASE_FEE_SCALAR.to_be_bytes());

    let storage = genesis["alloc"]["0x4200000000000000000000000000000000000015"]["storage"]
        .as_object_mut()
        .expect("L1Block predeploy must already carry storage");
    storage.insert(
        format!("{:#x}", B256::from(U256::from(3u64))),
        format!("{:#x}", B256::from(fee_scalars_slot)).into(),
    );
    storage.insert(
        format!("{:#x}", B256::from(U256::from(7u64))),
        format!("{:#x}", B256::from(U256::from(L1_BLOB_BASE_FEE))).into(),
    );

    // Slot 8 — shared by Isthmus's operator-fee scalar/constant (left zero,
    // unused) and Jovian's DA-footprint-gas-scalar. `mantle_chain_spec_with_predeploys_for`'s
    // alloc entry for this predeploy replaces the base genesis's wholesale (no merge), and
    // its own alloc carries no slot 8 at all — so under the predeploy chain spec this reads
    // as 0 unless seeded here, independent of `tests/assets/genesis.json`'s own (unrelated)
    // slot 8 value.
    let mut operator_and_da_footprint_slot = [0u8; 32];
    operator_and_da_footprint_slot[18..20].copy_from_slice(&DA_FOOTPRINT_GAS_SCALAR.to_be_bytes());
    storage.insert(
        format!("{:#x}", B256::from(U256::from(8u64))),
        format!("{:#x}", B256::from(operator_and_da_footprint_slot)).into(),
    );

    let genesis: Genesis = serde_json::from_value(genesis).expect("patched genesis deserialises");
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

/// The L1-info system transaction every real block carries as transaction 0.
/// `mantle_payload_attributes` sends none (see its doc comment in
/// `tests/preconf/helpers.rs`), which every other preconf test is fine with —
/// but `eth_getTransactionReceipt`'s L1 fee fields come from
/// `reth_optimism_evm::extract_l1_info`, which unconditionally reads the
/// block's first transaction as this one. Without it, a block whose own
/// first transaction is an ordinary preconf submission fails receipt lookup
/// with "unexpected l1 block info tx calldata length found" rather than
/// computing fields — this test needs the receipt to exist at all, so the
/// usual "attrs carries no transactions" shortcut does not apply here.
///
/// Jovian/Arsia-format calldata, matching
/// [`chain_spec_with_l1_fee_scalars`]'s seeded storage field for field —
/// `op_revm::L1BlockInfo::try_fetch` (the preconf side, reading seeded
/// storage) and `reth_optimism_evm::parse_l1_info`'s Jovian/Arsia branch (the
/// sealed-receipt side, reading this calldata) must land on identical
/// `L1BlockInfo` values for `assert_receipts_agree`'s L1 fee comparison to be
/// meaningful, and this tx never actually executes against a real setter (see
/// [`chain_spec_with_l1_fee_scalars`]), so the two can only agree if told the
/// same numbers twice. `operatorFeeScalar`/`operatorFeeConstant` are left `0`
/// (unused by every assertion — both vanish to `None` at the wire regardless
/// of source once they are zero) and `daFootprintGasScalar` matches
/// [`chain_spec_with_l1_fee_scalars`]'s seeded slot 8.
///
/// Duplicates `helpers::l1_info_deposit` (`tests/preconf/helpers.rs:924`),
/// which this test does not reuse because its hardcoded `base_fee_scalar`
/// and lack of DA-footprint/blob fields can't be told the numbers this test
/// needs. Two further differences from that helper are also deliberate, not
/// drift:
/// - `from` is the real op-stack L1-info depositor address
///   (`0xdead…0001`), not `Address::ZERO` — `assert_receipts_agree` checks
///   `from` against the sealed receipt's, so it has to be the address
///   `reth_optimism_evm::extract_l1_info` actually attributes the deposit to.
/// - `is_system_transaction` is left `false` (`..Default::default()`), not
///   `true` — this transaction is actually executed (unlike
///   `l1_info_deposit`'s callers, which only hash or re-encode its bytes
///   without running it through the EVM), and `OpRevm`'s `validate_env`
///   rejects a system-flagged deposit outright once Regolith is active
///   (`OpTransactionError::DepositSystemTxPostRegolith`,
///   `op-revm/src/handler.rs`) — which this chain is, from genesis.
fn l1_info_tx(timestamp: u64) -> alloy_primitives::Bytes {
    let mut payload = [0u8; 174];
    payload[0..4].copy_from_slice(&L1_BASE_FEE_SCALAR.to_be_bytes());
    payload[4..8].copy_from_slice(&L1_BLOB_BASE_FEE_SCALAR.to_be_bytes());
    payload[32..64].copy_from_slice(&U256::from(1_000_000_000u64).to_be_bytes::<32>()); // baseFee
    payload[64..96].copy_from_slice(&U256::from(L1_BLOB_BASE_FEE).to_be_bytes::<32>());
    payload[172..174].copy_from_slice(&DA_FOOTPRINT_GAS_SCALAR.to_be_bytes());

    let mut calldata = Vec::with_capacity(4 + payload.len());
    calldata.extend_from_slice(&[0x49, 0xe7, 0x23, 0x83]); // Arsia L1 attributes selector.
    calldata.extend_from_slice(&payload);
    let deposit = TxDeposit {
        source_hash: B256::from(U256::from(timestamp)),
        from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
        to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
        gas_limit: 1_000_000,
        input: calldata.into(),
        ..Default::default()
    };
    deposit.encoded_2718().into()
}

/// Chain id the predeploy-bearing genesis below is built for — Mantle Mainnet.
const CHAIN_ID: u64 = 5000;

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// WETH9 predeploy — used for the log-bearing and reverting cases. Only
/// present under [`mantle_chain_spec_with_predeploys_for`]; the bare
/// `mantle_test_chain_spec` genesis leaves this address empty.
const WETH9: Address = Address::new([
    0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x06,
]);
/// `deposit()` — `keccak256("deposit()")[0..4]`.
const DEPOSIT_SELECTOR: [u8; 4] = [0xd0, 0xe3, 0x0d, 0xb0];

async fn signed_transfer(chain_id: u64, wallet: &Wallet, nonce: u64) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(RECIPIENT.parse::<Address>().unwrap().into()),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(2e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// A WETH `deposit()` of 1 wei — succeeds and emits one `Deposit` log.
async fn signed_weth_deposit(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(WETH9.into()),
        gas: Some(200_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(2e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::new(DEPOSIT_SELECTOR.into()),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Every field the preconf receipt carries, against the sealed receipt's.
///
/// `gasUsed` is the one that would silently diverge: the executor's canonical
/// figure is what the chain records, and taking the EVM's raw figure instead
/// would read high by exactly the post-exec refund.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_receipt_matches_sealed_receipt() {
    let cfg = PreconfCfgBuilder::new().all_preconfs().preconf_timeout_ms(3_000).build();
    let (mut node, http, wallet, chain_id) =
        launch_preconf_node!(cfg, chain_spec_with_l1_fee_scalars(CHAIN_ID)).await;

    let mut attrs = node.payload.next_attributes();
    attrs.0.transactions = Some(vec![l1_info_tx(attrs.0.payload_attributes.timestamp)]);
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU must succeed")
        .payload_id
        .expect("payload_id present");

    let raw_tx = signed_weth_deposit(chain_id, &wallet, 0).await;
    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, raw_tx).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let preconf = rpc_task.await.expect("rpc join").expect("preconf must succeed");
    assert!(matches!(preconf.status, PreconfStatus::Success), "reason={:?}", preconf.reason);

    // Canonicalise so `eth_getTransactionReceipt` can serve the sealed receipt.
    let _new_head = canonicalize_payload!(node, payload).await;
    let sealed = sealed_receipt(&http, preconf.tx_hash).await;

    // The log-bearing case: one `Deposit` log, indexed as the chain indexes it.
    // Per-log fields (including `blockTimestamp`) are checked inside
    // `assert_receipts_agree`.
    assert_receipts_agree(&preconf, &sealed);
}

/// A revert is an outcome, not a missing receipt. Status is 0, the logs are
/// empty (the EVM rolled them back), and the gas still matches the chain's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revert_tx_receipt_matches_sealed_receipt() {
    let cfg = PreconfCfgBuilder::new().all_preconfs().preconf_timeout_ms(3_000).build();
    let (mut node, http, wallet, chain_id) =
        launch_preconf_node!(cfg, chain_spec_with_l1_fee_scalars(CHAIN_ID)).await;

    let mut attrs = node.payload.next_attributes();
    attrs.0.transactions = Some(vec![l1_info_tx(attrs.0.payload_attributes.timestamp)]);
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU must succeed")
        .payload_id
        .expect("payload_id present");

    // `transfer(recipient, 1)` from an account holding zero WETH — reverts
    // inside the ERC-20 balance check.
    let mut calldata = Vec::with_capacity(4 + 32 + 32);
    calldata.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]); // transfer(address,uint256)
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(RECIPIENT.parse::<Address>().unwrap().as_slice());
    calldata.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(0),
        to: Some(WETH9.into()),
        gas: Some(200_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(2e9 as u128),
        value: Some(U256::ZERO),
        input: TransactionInput::new(calldata.into()),
        ..Default::default()
    };
    let raw_tx: alloy_primitives::Bytes =
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into();

    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, raw_tx).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let preconf = rpc_task.await.expect("rpc join").expect("preconf call must return a receipt");
    assert!(matches!(preconf.status, PreconfStatus::Failed), "a revert is a `failed` outcome");
    assert_eq!(preconf.receipt.status, Some(0), "EVM status must be 0 on revert");
    assert_eq!(
        preconf.receipt.logs.as_deref(),
        Some(&[][..]),
        "a reverted tx emits no logs, but it did execute — `[]`, not `null`",
    );
    assert!(preconf.receipt.gas_used.is_some_and(|g| g > 0), "a revert still burns gas");

    let _new_head = canonicalize_payload!(node, payload).await;
    // A reverted transaction still lands, so it still has a canonical receipt.
    let sealed = sealed_receipt(&http, preconf.tx_hash).await;
    assert_receipts_agree(&preconf, &sealed);
}

/// `logIndex` is block-global. A transaction landing behind another that
/// already emitted logs must report indices continuing from there, not from 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_indices_continue_across_transactions() {
    let cfg = PreconfCfgBuilder::new().all_preconfs().preconf_timeout_ms(3_000).build();
    let (mut node, http, wallet, chain_id) =
        launch_preconf_node!(cfg, chain_spec_with_l1_fee_scalars(CHAIN_ID)).await;

    let mut attrs = node.payload.next_attributes();
    attrs.0.transactions = Some(vec![l1_info_tx(attrs.0.payload_attributes.timestamp)]);
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU must succeed")
        .payload_id
        .expect("payload_id present");

    // Two deposits from the same sender, in nonce order. The second one's log
    // must not be numbered 0.
    let first = signed_weth_deposit(chain_id, &wallet, 0).await;
    let second = signed_weth_deposit(chain_id, &wallet, 1).await;
    let (h1, h2) = (http.clone(), http.clone());
    let t1 = tokio::spawn(async move { send_preconf(&h1, first).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let t2 = tokio::spawn(async move { send_preconf(&h2, second).await });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let a = t1.await.expect("join").expect("first preconf");
    let b = t2.await.expect("join").expect("second preconf");

    let idx_a = a.receipt.logs.as_ref().expect("logs")[0].log_index.expect("logIndex");
    let idx_b = b.receipt.logs.as_ref().expect("logs")[0].log_index.expect("logIndex");
    assert!(idx_b > idx_a, "the later transaction's log index must advance: {idx_a} then {idx_b}");

    let _new_head = canonicalize_payload!(node, payload).await;
    for event in [&a, &b] {
        let sealed = sealed_receipt(&http, event.tx_hash).await;
        assert_receipts_agree(event, &sealed);
    }
}

/// A `TokenRatioUpdated` earlier in the same block changes what a later
/// transaction's L1 fee costs. The ratio that decides it is the one in force
/// *before* it runs — the same one `eth_getTransactionReceipt` uses when it
/// replays the block's `TokenRatioUpdated` logs. Reading the slot after the
/// apply instead would let a transaction's own update contaminate its own fee.
///
/// `GasPriceOracle.setTokenRatio` is `onlyOperator`. The predeploy genesis
/// (`mantle_chain_spec_with_predeploys_for`) seeds `owner` to the standard
/// Hardhat test mnemonic's account 7, funded exactly like every other account
/// in `genesis.json` — so the three transactions below (`setOperator`,
/// `setTokenRatio`, then an ordinary transfer) are all ordinary preconf
/// submissions, no L1 deposit or governance path required.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l1_fee_uses_the_token_ratio_in_force_before_the_tx() {
    /// `setOperator(address)` — `keccak256("setOperator(address)")[0..4]`.
    const SET_OPERATOR_SELECTOR: [u8; 4] = [0xb3, 0xab, 0x15, 0xfb];
    /// `setTokenRatio(uint256)` — `keccak256("setTokenRatio(uint256)")[0..4]`.
    const SET_TOKEN_RATIO_SELECTOR: [u8; 4] = [0xe3, 0x8e, 0x91, 0xf9];
    /// `GasPriceOracle` predeploy — `owner`/`operator` are seeded by
    /// `tests/assets/predeploys.json`.
    const GAS_PRICE_ORACLE: Address = Address::new([
        0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x0f,
    ]);
    /// Account index 7 of the standard Hardhat mnemonic — the predeploy
    /// genesis's `GasPriceOracle.owner`, funded identically to every other
    /// account in `genesis.json`.
    const OWNER_ACCOUNT_INDEX: usize = 7;
    const NEW_TOKEN_RATIO: u64 = 9_000;

    let cfg = PreconfCfgBuilder::new().all_preconfs().preconf_timeout_ms(3_000).build();
    let (mut node, http, wallet, chain_id) =
        launch_preconf_node!(cfg, chain_spec_with_l1_fee_scalars(CHAIN_ID)).await;
    let owner = Wallet::new(OWNER_ACCOUNT_INDEX + 1).with_chain_id(chain_id).wallet_gen()
        [OWNER_ACCOUNT_INDEX]
        .clone();

    let mut attrs = node.payload.next_attributes();
    attrs.0.transactions = Some(vec![l1_info_tx(attrs.0.payload_attributes.timestamp)]);
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU must succeed")
        .payload_id
        .expect("payload_id present");

    // tx1: owner -> setOperator(wallet) — hands operator rights to the
    // ordinary test wallet used for the rest of the suite.
    let mut operator_calldata = Vec::with_capacity(4 + 32);
    operator_calldata.extend_from_slice(&SET_OPERATOR_SELECTOR);
    operator_calldata.extend_from_slice(&[0u8; 12]);
    operator_calldata.extend_from_slice(wallet.inner.address().as_slice());
    let set_operator_tx = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(GAS_PRICE_ORACLE.into()),
            gas: Some(100_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(2e9 as u128),
            value: Some(U256::ZERO),
            input: TransactionInput::new(operator_calldata.into()),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(owner.clone(), request).await.encoded_2718()
    };

    // tx2: wallet (now operator) -> setTokenRatio(NEW_TOKEN_RATIO).
    let mut ratio_calldata = Vec::with_capacity(4 + 32);
    ratio_calldata.extend_from_slice(&SET_TOKEN_RATIO_SELECTOR);
    ratio_calldata.extend_from_slice(&U256::from(NEW_TOKEN_RATIO).to_be_bytes::<32>());
    let set_ratio_tx = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(GAS_PRICE_ORACLE.into()),
            gas: Some(100_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(2e9 as u128),
            value: Some(U256::ZERO),
            input: TransactionInput::new(ratio_calldata.into()),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718()
    };

    // tx3: wallet -> ordinary transfer. Its L1 fee must reflect NEW_TOKEN_RATIO,
    // set by tx2, not the ratio in force at block start.
    let ordinary_tx = signed_transfer(chain_id, &wallet, 1).await;

    // Dispatched in order, each given time to land ahead of the next so the
    // in-block execution order matches the dependency chain above.
    let h1 = http.clone();
    let t1 = tokio::spawn(async move { send_preconf(&h1, set_operator_tx.into()).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let h2 = http.clone();
    let t2 = tokio::spawn(async move { send_preconf(&h2, set_ratio_tx.into()).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let h3 = http.clone();
    let t3 = tokio::spawn(async move { send_preconf(&h3, ordinary_tx).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let set_operator = t1.await.expect("join").expect("setOperator must succeed");
    assert!(
        matches!(set_operator.status, PreconfStatus::Success),
        "setOperator must succeed; reason={:?}",
        set_operator.reason,
    );
    let set_ratio = t2.await.expect("join").expect("setTokenRatio must succeed");
    assert!(
        matches!(set_ratio.status, PreconfStatus::Success),
        "setTokenRatio must succeed once the wallet is operator; reason={:?}",
        set_ratio.reason,
    );
    let ordinary = t3.await.expect("join").expect("ordinary transfer must succeed");
    assert!(
        matches!(ordinary.status, PreconfStatus::Success),
        "ordinary transfer must succeed; reason={:?}",
        ordinary.reason,
    );

    let _new_head = canonicalize_payload!(node, payload).await;

    // The transaction that actually distinguishes "refresh before apply" from
    // "refresh after apply" is `set_ratio` itself (tx2), not the ordinary
    // transfer that follows it: tx3's before-ratio and after-ratio are both
    // `NEW_TOKEN_RATIO` (nothing changes it again), so tx3 passes identically
    // whichever side of its own apply `refresh_token_ratio` runs on, and a
    // bug there would not turn this test red. tx2's before-ratio
    // (`GasPriceOracle`'s genesis-seeded `tokenRatio`, 4_500 — see
    // `tests/assets/predeploys.json`'s slot 0) and after-ratio
    // (`NEW_TOKEN_RATIO`, its own write) genuinely differ: if
    // `refresh_token_ratio` ran after the apply instead of before, tx2's own
    // L1 fee would be contaminated by the ratio it had just written, while
    // `eth_getTransactionReceipt`'s log-replay reconstruction always charges
    // an emitting transaction at the ratio in force *before* its own logs —
    // so the two would disagree exactly on this transaction.
    const INITIAL_TOKEN_RATIO: u64 = 4_500;
    let sealed_ratio_tx = sealed_receipt(&http, set_ratio.tx_hash).await;
    assert_receipts_agree(&set_ratio, &sealed_ratio_tx);
    assert_eq!(
        sealed_ratio_tx.l1_block_info.token_ratio,
        Some(u128::from(INITIAL_TOKEN_RATIO)),
        "setTokenRatio's own fee must be charged at the ratio in force before it ran",
    );

    // The ordinary transfer (tx3) is still checked as supplementary coverage of
    // the common case — the ratio an unrelated later transaction sees.
    let sealed = sealed_receipt(&http, ordinary.tx_hash).await;
    assert_receipts_agree(&ordinary, &sealed);
    assert_eq!(
        sealed.l1_block_info.token_ratio,
        Some(u128::from(NEW_TOKEN_RATIO)),
        "the ordinary transfer must be charged at the ratio set earlier in this block",
    );
}

/// Every field the two receipts share, compared in one place so each test
/// covers all of them rather than whichever the author remembered.
fn assert_receipts_agree(
    preconf: &PreconfTxEvent,
    sealed: &op_alloy_rpc_types::OpTransactionReceipt,
) {
    let r = &preconf.receipt;
    assert_eq!(r.gas_used, Some(sealed.inner.gas_used), "gasUsed");
    assert_eq!(
        r.cumulative_gas_used,
        Some(sealed.inner.inner.cumulative_gas_used()),
        "cumulativeGasUsed",
    );
    assert_eq!(r.transaction_index, sealed.inner.transaction_index, "transactionIndex");
    assert_eq!(r.logs_bloom, Some(sealed.inner.inner.bloom()), "logsBloom");
    assert_eq!(r.effective_gas_price, Some(sealed.inner.effective_gas_price), "effectiveGasPrice",);
    assert_eq!(r.from, Some(sealed.inner.from), "from");
    assert_eq!(r.to, sealed.inner.to, "to");
    assert_eq!(r.contract_address, sealed.inner.contract_address, "contractAddress");
    assert_eq!(r.status, Some(u64::from(sealed.inner.inner.status())), "status");

    // The L1 fee group, compared whole: anything op-reth adds to it later is
    // covered without touching this test.
    assert_eq!(r.l1_fields.l1_block_info, sealed.l1_block_info, "L1 fee fields");

    // Per-log fields, including `blockTimestamp` — set at
    // `payload_builder.rs`'s `receipt.block_timestamp = limits.timestamp` and
    // carried onto the wire by `PreconfLog::block_timestamp`, but otherwise
    // unasserted anywhere in this suite until now.
    let logs = r.logs.as_ref().expect("apply happened");
    assert_eq!(logs.len(), sealed.inner.inner.logs().len(), "log count");
    for (got, want) in logs.iter().zip(sealed.inner.inner.logs()) {
        assert_eq!(got.address, want.address(), "log address");
        assert_eq!(got.log_index, want.log_index, "logIndex");
        assert_eq!(got.transaction_index, want.transaction_index, "log transactionIndex");
        assert_eq!(got.block_number, want.block_number, "log blockNumber");
        assert_eq!(got.block_timestamp, want.block_timestamp, "log blockTimestamp");
        assert_eq!(got.removed, Some(want.removed), "log removed");
    }
}

/// Poll `eth_getTransactionReceipt` until the canonical receipt shows up.
///
/// Typed rather than `serde_json::Value` on purpose: deserializing into
/// `OpTransactionReceipt` is itself a check that the preconf receipt's field
/// names and encodings match the ones this RPC emits.
async fn sealed_receipt(
    client: &jsonrpsee::http_client::HttpClient,
    hash: alloy_primitives::B256,
) -> op_alloy_rpc_types::OpTransactionReceipt {
    use jsonrpsee::core::client::ClientT;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(receipt) = client
                .request::<Option<op_alloy_rpc_types::OpTransactionReceipt>, _>(
                    "eth_getTransactionReceipt",
                    jsonrpsee::rpc_params![hash],
                )
                .await
                .expect("receipt RPC")
            {
                return receipt;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a canonical block's transaction must have a receipt")
}

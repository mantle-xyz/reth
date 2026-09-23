//! The account view is kept current by the build loop.
//!
//! `PreconfTxSet`'s account view records what the block being built has done to
//! each sender's nonce, so admission can answer nonce questions against that
//! block rather than against its parent. Nothing reads it yet — admission
//! starts consulting it a stage later — and that is exactly why these tests
//! exist: a view that is written in the wrong place, or not written at all,
//! would otherwise cost nothing until the reader arrives and quietly gets
//! stale answers.
//!
//! Every execution path the builder has must reach it. There are four, and
//! they are not interchangeable:
//!
//! - the **preconf arm** (`apply_preconf_with_da`),
//! - the **pool arm** (`apply_one_best_tx`, the only one that goes through `record_journalable`),
//! - **stage 2** (`execute_sequencer_transactions_watching_whitelist`), which on a derivation build
//!   carries the batched user transactions,
//! - the **post-execution** refund transaction, which carries no nonce of its own and so is counted
//!   rather than read.
//!
//! `both_arms_...` covers the first two, `a_stage_two_transaction_...` the
//! third plus the ordering the reset depends on, and the type gate that keeps
//! the fourth from writing nonsense.

use super::helpers::{PreconfCfgBuilder, l1_attrs_with, send_preconf};
use crate::launch_preconf_node_with_fifo;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

async fn signed_transfer(
    chain_id: u64,
    signer: alloy_signer_local::PrivateKeySigner,
    nonce: u64,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse::<Address>().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(signer, request).await.encoded_2718().into()
}

/// **Both arms, one block.** The preconf arm and the pool arm reach the view
/// through different code — the pool arm alone goes through
/// `record_journalable` — so covering one says nothing about the other.
///
/// Two senders rather than one: a single sender interleaving the channels is
/// deferred by a block (`race_pool_arm::one_sender_interleaving_...`), which
/// would leave only one of the two arms exercised here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_arms_advance_the_account_view_in_one_block() {
    use super::helpers::mantle_test_chain_spec;
    use reth_chainspec::EthChainSpec;

    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id_for_addrs = mantle_test_chain_spec().chain().id();
    let signers = Wallet::new(3).with_chain_id(chain_id_for_addrs).wallet_gen();
    let preconf_sender = signers[0].address();
    // signers[1] collides with RECIPIENT; signers[2] is off the allowlist, so
    // the pool arm is its only route.
    let pool_signer = signers[2].clone();
    let pool_sender = pool_signer.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(preconf_sender)
        .whitelist_to(recipient)
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, preconf_wallet, chain_id, fifo) =
        launch_preconf_node_with_fifo!(cfg).await;
    assert_eq!(preconf_wallet.inner.address(), preconf_sender);

    // Before the FCU: the pool iterator is snapshotted at build start, so a
    // transaction arriving later cannot be in this block whatever its nonce.
    let pool_tx = signed_transfer(chain_id, pool_signer, 0).await;
    let pool_hash: B256 = node.rpc.inject_tx(pool_tx).await.expect("pool sendTx accepted");

    let attrs = node.payload.next_attributes();
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

    let preconf_tx = signed_transfer(chain_id, preconf_wallet.inner.clone(), 0).await;
    let preconf_hash = keccak256(&preconf_tx);
    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, preconf_tx).await });

    // Long enough for the preconf arm to take its turn and for one sweep tick
    // (200ms default) to open the pool arm's quota past 21k.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    rpc_task.await.expect("rpc join").expect("the preconf tx must be accepted");

    // The premise: both really did execute in this block. Without it, an empty
    // view below would be the right answer and the test would prove nothing.
    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed.contains(&preconf_hash), "preconf tx must land; sealed = {sealed:?}");
    assert!(sealed.contains(&pool_hash), "pool tx must land; sealed = {sealed:?}");

    // No canonicalisation in between: the next build would reset the view.
    assert_eq!(
        fifo.next_nonce(&preconf_sender, 0),
        1,
        "the preconf arm must advance the view for its sender",
    );
    assert_eq!(
        fifo.next_nonce(&pool_sender, 0),
        1,
        "the pool arm must too — it is the one that goes through record_journalable",
    );

    let block_base_fee =
        payload.block().base_fee_per_gas.expect("post-London block carries a base fee");
    assert_eq!(
        fifo.build_base_fee(),
        Some(block_base_fee),
        "the view must be opened for this block, with this block's base fee",
    );
}

/// **Stage 2, and the ordering the reset depends on.**
///
/// The transaction here is never submitted to the pool; it rides in the
/// payload attributes, which is how a derivation build receives the batched
/// user transactions. It is therefore executed by stage 2 and by nothing else.
///
/// That makes this the test that pins *where* the view is reset. Stage 2 runs
/// before the rest of the build, so a reset placed anywhere after it wipes
/// what stage 2 recorded and this assertion reads zero.
///
/// The second assertion is about the other half of the same hook: the
/// L1-attributes deposit in tx[0] advances its sender's nonce but reports `0`
/// for it, so it must be *counted* from the chain rather than read. Recording
/// the reported zero would answer 99 here instead of 100.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stage_two_transaction_reaches_the_account_view() {
    let placeholder = Address::from([0xFE; 20]);
    let cfg =
        PreconfCfgBuilder::new().whitelist_from(placeholder).whitelist_to(placeholder).build();

    let (node, _http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;
    let sender = wallet.inner.address();

    let raw = signed_transfer(chain_id, wallet.inner.clone(), 0).await;
    let tx_hash = keccak256(&raw);

    let attrs = l1_attrs_with(0, 1, vec![raw]);
    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let payload_id = crate::fcu_v3_start!(node, genesis, attrs);

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let payload = crate::get_payload_v5!(node, payload_id);

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed.contains(&tx_hash), "stage 2 must execute it; sealed = {sealed:?}");

    assert_eq!(
        fifo.next_nonce(&sender, 0),
        1,
        "stage 2 must reach the view, and the reset must precede it",
    );

    // `l1_info_deposit` sends from the zero address; what matters is that the
    // deposit was counted rather than read.
    assert_eq!(
        fifo.next_nonce(&Address::ZERO, 99),
        100,
        "a deposit must be counted from the chain, not recorded as the zero it reports",
    );
}

/// **A deposit in this block moves the sender's nonce, and admission has to
/// know.**
///
/// The sender's nonce on the parent block is 0. The deposit in tx[0]'s wake
/// takes it to 1 inside the block being built, so 1 is the nonce its next
/// transaction must use — but the parent state, which is all the admission
/// path can read directly, still says 0.
///
/// The account view is what closes that gap: the deposit carries no nonce of
/// its own, so it is counted, and the count is added to the chain value. This
/// test is the only thing that proves the count is actually consulted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preconf_tx_following_a_deposit_from_the_same_sender_lands_in_that_block() {
    use super::helpers::user_deposit;
    use mantle_reth_rpc_ext::PreconfStatus;

    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .preconf_timeout_ms(3_000)
        .build();

    let (node, http, wallet, chain_id, _fifo) = launch_preconf_node_with_fifo!(cfg).await;

    // The deposit rides in the attributes, so it is executed by stage 2 before
    // the preconf arm ever runs.
    let attrs =
        l1_attrs_with(0, 1, vec![user_deposit(sender, recipient, Default::default(), 100_000)]);
    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let payload_id = crate::fcu_v3_start!(node, genesis, attrs);

    // Submitted after the build opened, so stage 2 has already consumed nonce 0.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let preconf_tx = signed_transfer(chain_id, wallet.inner.clone(), 1).await;
    let preconf_hash = keccak256(&preconf_tx);
    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, preconf_tx).await });

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let payload = crate::get_payload_v5!(node, payload_id);

    let event = rpc_task
        .await
        .expect("rpc join")
        .expect("nonce 1 is the sender's real next nonce once the deposit has run");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "expected Success, got {:?} (reason={:?})",
        event.status,
        event.reason,
    );

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed.contains(&preconf_hash), "it must land in that same block; sealed = {sealed:?}");
}

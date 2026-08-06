//! Happy-path preconf submission.
//!
//! Every test in this module starts a preconf-enabled `MantleNode` from
//! scratch, submits a raw tx through `eth_sendRawTransactionWithPreconf`,
//! and verifies:
//!
//! 1. the RPC response shape (`PreconfTxEvent { status, block_height, receipt.logs, ... }`), and
//! 2. the resulting on-chain state after a single slot is advanced.

use super::helpers::{
    PreconfCfgBuilder, mantle_chain_spec_with_predeploys_for, mantle_test_chain_spec, send_preconf,
    wait_pending_nonce,
};
use crate::{canonize_built, launch_preconf_node};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_provider::StateProviderFactory;

/// Recipient hard-coded across happy-path tests. Pre-funded in
/// `assets/genesis.json` via the Hardhat test mnemonic (account 1).
const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Build a signed 21k-gas transfer for `wallet[0]` → `RECIPIENT` at
/// the given nonce. Returned as RLP-encoded bytes ready for RPC
/// submission.
async fn signed_transfer(chain_id: u64, wallet: &Wallet, nonce: u64) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(RECIPIENT.parse::<Address>().unwrap().into()),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Whitelisted (sender, to) pair + `send_preconf` returns `Success`;
/// advancing the slot lands the tx on chain with matching `block_height`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn success_returns_receipt_and_lands_on_chain() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new().whitelist_from(sender).whitelist_to(recipient).build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;

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
        .expect("payload_id must be present when attributes are supplied");

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw_tx).await });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind not cancelled")
        .expect("payload build must produce a sealed payload");

    let event = rpc_task.await.expect("rpc task join").expect("preconf RPC must succeed");

    assert!(
        matches!(event.status, PreconfStatus::Success),
        "expected Success, got {:?} (reason={:?})",
        event.status,
        event.reason
    );
    let expected_hash = event.tx_hash;
    assert_eq!(event.block_height, 1, "predicted height is parent + 1 = 1");
    assert!(
        event.receipt.logs.is_some(),
        "success path must carry Some(logs), not None (which signals no-apply)"
    );

    let block = payload.block();
    assert_eq!(block.number, 1, "sealed block matches predicted height");
    let sealed_hashes: Vec<B256> = block
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed_hashes.contains(&expected_hash),
        "preconf tx must land in block 1; sealed = {sealed_hashes:?}"
    );
}

/// One sender submits three preconf txs with sequential nonces (0/1/2)
/// against the same slot; every tx must land in that slot and their
/// indices in the sealed block body must reflect **EVM nonce order**.
///
/// Note: because these three txs share a sender, EVM already forces
/// `nonce=1` to apply after `nonce=0` (and so on), so `idx0<idx1<idx2`
/// is dominated by that hard constraint — not by preconf fifo
/// ordering. Cross-sender fifo ordering (where nonce imposes no
/// constraint between txs) is exercised by
/// `multi_sender_land_in_one_block`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_nonce_same_sender_land_in_one_block() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new().whitelist_from(wallet_addr).whitelist_to(recipient).build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

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

    let tx0 = signed_transfer(chain_id, &wallet, 0).await;
    let tx1 = signed_transfer(chain_id, &wallet, 1).await;
    let tx2 = signed_transfer(chain_id, &wallet, 2).await;
    let hash0 = alloy_primitives::keccak256(&tx0);
    let hash1 = alloy_primitives::keccak256(&tx1);
    let hash2 = alloy_primitives::keccak256(&tx2);

    // Serialise submissions so nonces enter the pool in ascending order;
    // `add_transaction` rejects out-of-order nonces at admission, so
    // parallel spawns can race into `PoolRejected(nonce gap)`. Note that
    // EVM's per-sender nonce constraint (not fifo) is what ultimately
    // dictates the sealed-block index order asserted below.
    let http_c = http.clone();
    let t0 = tokio::spawn(async move { send_preconf(&http_c, tx0).await });
    wait_pending_nonce(&http, wallet_addr, 1).await;
    let http_c = http.clone();
    let t1 = tokio::spawn(async move { send_preconf(&http_c, tx1).await });
    wait_pending_nonce(&http, wallet_addr, 2).await;
    let http_c = http.clone();
    let t2 = tokio::spawn(async move { send_preconf(&http_c, tx2).await });

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    for (i, task) in [t0, t1, t2].into_iter().enumerate() {
        let event = task.await.expect("rpc join").expect("preconf must succeed");
        assert!(
            matches!(event.status, PreconfStatus::Success),
            "tx{i} status: {:?} reason={:?}",
            event.status,
            event.reason,
        );
    }

    let sealed_hashes: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();

    // All three land, and they appear in strictly ascending nonce order
    // (an EVM invariant, not a preconf fifo property — see docstring).
    let idx0 = sealed_hashes.iter().position(|h| *h == hash0).expect("tx0 in block");
    let idx1 = sealed_hashes.iter().position(|h| *h == hash1).expect("tx1 in block");
    let idx2 = sealed_hashes.iter().position(|h| *h == hash2).expect("tx2 in block");
    assert!(
        idx0 < idx1 && idx1 < idx2,
        "preconf txs must sit in sealed block in EVM nonce order; got idx0={idx0} idx1={idx1} idx2={idx2}",
    );
}

/// Two independent senders each submit one preconf tx against the same
/// slot. Both land in that slot and the sealed block preserves the
/// order in which the RPCs arrived (fifo across senders).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_sender_land_in_one_block() {
    let recipient: Address = RECIPIENT.parse().unwrap();

    let chain_id_for_addrs = mantle_test_chain_spec().chain().id();
    let signers = Wallet::new(3).with_chain_id(chain_id_for_addrs).wallet_gen();
    let sender_a_addr = signers[0].address();
    // Skip index 1 — that's the RECIPIENT above.
    let sender_b_addr = signers[2].address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender_a_addr)
        .whitelist_from(sender_b_addr)
        .whitelist_to(recipient)
        .build();

    let (mut node, http, wallet_a, chain_id) = launch_preconf_node!(cfg).await;
    // Sanity: the wallet returned by the macro is the same account as
    // `sender_a_addr` — otherwise `sender_a`'s tx would fail the whitelist.
    assert_eq!(wallet_a.inner.address(), sender_a_addr);

    // Second signer must sign against the *launched* chain id. `Wallet`
    // has private fields so we can't construct one; drop to the raw
    // `PrivateKeySigner` and hand-roll the tx below.
    let signer_b = Wallet::new(3).with_chain_id(chain_id).wallet_gen()[2].clone();

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

    let tx_a = signed_transfer(chain_id, &wallet_a, 0).await;
    let tx_b: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(recipient.into()),
            gas: Some(21_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            value: Some(U256::from(1u64)),
            input: TransactionInput::default(),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(signer_b, request).await.encoded_2718().into()
    };
    let hash_a = alloy_primitives::keccak256(&tx_a);
    let hash_b = alloy_primitives::keccak256(&tx_b);

    let http_c = http.clone();
    let task_a = tokio::spawn(async move { send_preconf(&http_c, tx_a).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let task_b = tokio::spawn(async move { send_preconf(&http_c, tx_b).await });

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let ev_a = task_a.await.expect("join a").expect("sender A preconf must succeed");
    let ev_b = task_b.await.expect("join b").expect("sender B preconf must succeed");
    assert!(matches!(ev_a.status, PreconfStatus::Success), "sender A: {:?}", ev_a.reason);
    assert!(matches!(ev_b.status, PreconfStatus::Success), "sender B: {:?}", ev_b.reason);

    let sealed_hashes: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    let idx_a = sealed_hashes.iter().position(|h| *h == hash_a).expect("tx_a in block");
    let idx_b = sealed_hashes.iter().position(|h| *h == hash_b).expect("tx_b in block");
    assert!(
        idx_a < idx_b,
        "sender-A tx arrived first ⇒ must land at a lower index; got idx_a={idx_a} idx_b={idx_b}",
    );
}

/// `all_preconfs=true` bypasses the (from, to) whitelist entirely: an
/// arbitrary sender's tx to an arbitrary recipient is still applied via
/// the preconf pipeline and lands on chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_preconfs_mode_accepts_arbitrary_sender() {
    // No whitelist calls — validate `all_preconfs=true` skips the check.
    let cfg = PreconfCfgBuilder::new().all_preconfs().build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

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

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
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

    let event = rpc_task.await.expect("rpc join").expect("preconf must succeed");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "all_preconfs=true must accept arbitrary sender; got {:?} reason={:?}",
        event.status,
        event.reason,
    );

    let sealed: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed.contains(&event.tx_hash),
        "all_preconfs tx must land in block 1; sealed = {sealed:?}",
    );
}

/// Compute the storage slot for `balanceOf[addr]` on a canonical WETH9
/// layout (slot 3 holds the `mapping(address => uint) balanceOf`). Key
/// derivation follows Solidity's `keccak256(abi.encode(addr, slot))`.
fn weth_balance_slot(addr: Address) -> alloy_primitives::B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(addr.as_slice());
    buf[63] = 3; // storage slot index for balanceOf mapping
    alloy_primitives::keccak256(buf)
}

/// A WETH `transfer` for more than the sender's WETH balance triggers
/// the ERC20 `require(balanceOf[src] >= wad)` inside the WETH9
/// predeploy, which reverts. The tx still lands on chain — receipt
/// status is `false`, and the wire event surfaces as
/// `PreconfStatus::Failed` with `receipt.logs = Some(vec![])` (apply
/// happened, no logs emitted because the revert aborts before the
/// `Transfer` event).
///
/// This distinguishes the "on-chain but reverted" state from the
/// "never on chain" states (`Timeout`, builder pre-apply reject) that
/// carry `receipt.logs = None`.
///
/// Requires the predeploy-populated genesis so `0x4200...06` has WETH9
/// bytecode; the bare genesis leaves that slot empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn weth_transfer_over_balance_lands_as_reverted() {
    /// Canonical WETH9 predeploy on every OP-stack chain.
    const WETH9: Address = Address::new([
        0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x06,
    ]);
    /// `transfer(address,uint256)` — keccak256("transfer(address,uint256)")[0..4].
    const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new().whitelist_from(wallet_addr).whitelist_to(WETH9).build();

    let (mut node, http, wallet, chain_id) =
        launch_preconf_node!(cfg, mantle_chain_spec_with_predeploys_for(5000)).await;

    // Sender has 0 WETH; sending 1 wei of WETH must revert inside the
    // ERC20 balance check.
    let mut calldata = Vec::with_capacity(4 + 32 + 32);
    calldata.extend_from_slice(&TRANSFER_SELECTOR);
    calldata.extend_from_slice(&[0u8; 12]); // left-pad recipient to 32 bytes
    calldata.extend_from_slice(recipient.as_slice());
    calldata.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>()); // amount = 1 wei

    let raw_tx: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(WETH9.into()),
            gas: Some(100_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            value: Some(U256::ZERO),
            input: TransactionInput::new(calldata.into()),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
    };
    let expected_hash = alloy_primitives::keccak256(&raw_tx);

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

    let event = rpc_task
        .await
        .expect("rpc join")
        .expect("reverted tx must still return Ok(event) — revert is on-chain, not an RPC error");

    assert!(
        matches!(event.status, PreconfStatus::Failed),
        "reverted tx must surface as PreconfStatus::Failed; got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    // `Some(vec![])` distinguishes "applied + reverted" from "never applied"
    // (`None`); the revert aborts before WETH9's `Transfer` event fires.
    assert_eq!(
        event.receipt.logs.as_ref().map(|l| l.len()),
        Some(0),
        "reverted apply must carry Some(empty) logs, not None",
    );

    let sealed: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed.contains(&expected_hash),
        "reverted tx must still land on chain; hash {expected_hash:?} not in {sealed:?}",
    );

    // Canonicalise the block so the state provider serves post-execution
    // state, then confirm the failed transfer left `balanceOf[recipient]`
    // untouched at 0 — the revert must have rolled back EVM state.
    canonize_built!(node, payload);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let state = node.inner.provider.latest().expect("state provider");
    let recipient_balance = state
        .storage(WETH9, weth_balance_slot(recipient))
        .expect("storage lookup")
        .unwrap_or_default();
    assert_eq!(
        recipient_balance,
        U256::ZERO,
        "revert must roll back state; balanceOf[recipient] should stay 0, got {recipient_balance}",
    );
}

/// A successful WETH `deposit()` call emits the `Deposit(dst, wad)`
/// event. The preconf receipt must carry the log through to the wire,
/// exercising the `logs = Some(vec![log])` branch of the tri-state (the
/// third and last variant, alongside `None` and `Some([])` covered by
/// timeout / revert tests).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn weth_deposit_carries_log_through_to_receipt() {
    const WETH9: Address = Address::new([
        0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x06,
    ]);
    /// `deposit()` — `keccak256("deposit()`")[0..4].
    const DEPOSIT_SELECTOR: [u8; 4] = [0xd0, 0xe3, 0x0d, 0xb0];
    /// `keccak256("Deposit(address,uint256)")` — WETH9's Deposit event.
    const DEPOSIT_EVENT_TOPIC: [u8; 32] = [
        0xe1, 0xff, 0xfc, 0xc4, 0x92, 0x3d, 0x04, 0xb5, 0x59, 0xf4, 0xd2, 0x9a, 0x8b, 0xfc, 0x6c,
        0xda, 0x04, 0xeb, 0x5b, 0x0d, 0x3c, 0x46, 0x07, 0x51, 0xc2, 0x40, 0x2c, 0x5c, 0x5c, 0xc9,
        0x10, 0x9c,
    ];

    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new().whitelist_from(wallet_addr).whitelist_to(WETH9).build();

    let (mut node, http, wallet, chain_id) =
        launch_preconf_node!(cfg, mantle_chain_spec_with_predeploys_for(5000)).await;

    let deposit_amount = U256::from(100_000u64);
    let raw_tx: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(WETH9.into()),
            gas: Some(100_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            value: Some(deposit_amount),
            input: TransactionInput::new(DEPOSIT_SELECTOR.to_vec().into()),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
    };

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

    let event = rpc_task.await.expect("rpc join").expect("deposit must succeed");

    assert!(
        matches!(event.status, PreconfStatus::Success),
        "deposit must succeed; got {:?} reason={:?}",
        event.status,
        event.reason,
    );

    let logs = event.receipt.logs.as_ref().expect("apply happened ⇒ logs = Some(_)");
    assert_eq!(
        logs.len(),
        1,
        "WETH9.deposit emits exactly one Deposit event; got {} logs",
        logs.len(),
    );
    let log = &logs[0];
    assert_eq!(log.address, WETH9, "log.address must be the WETH9 predeploy");
    assert_eq!(
        log.topics.first().map(|t| t.0),
        Some(DEPOSIT_EVENT_TOPIC),
        "topic[0] must be keccak(\"Deposit(address,uint256)\")",
    );
    // topic[1] = sender address, left-padded to 32 bytes.
    let mut expected_sender_topic = [0u8; 32];
    expected_sender_topic[12..].copy_from_slice(wallet_addr.as_slice());
    assert_eq!(
        log.topics.get(1).map(|t| t.0),
        Some(expected_sender_topic),
        "topic[1] must be the depositor address",
    );
    // data = 32-byte big-endian wad.
    assert_eq!(
        log.data,
        alloy_primitives::Bytes::copy_from_slice(&deposit_amount.to_be_bytes::<32>()),
        "log.data must be the deposited amount as a 32-byte word",
    );

    // Canonicalise and confirm the EVM state transition landed on chain:
    // WETH9's `balanceOf[wallet_addr]` must now equal the deposited wad.
    // This guards against a hypothetical regression where the receipt
    // reports success but the state write was silently dropped.
    canonize_built!(node, payload);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let state = node.inner.provider.latest().expect("state provider");
    let wallet_weth = state
        .storage(WETH9, weth_balance_slot(wallet_addr))
        .expect("storage lookup")
        .unwrap_or_default();
    assert_eq!(
        wallet_weth, deposit_amount,
        "WETH9.balanceOf[wallet] must equal the deposit amount post-canon; got {wallet_weth}",
    );
}

//! EIP-7702 delegated accounts on the preconf path.
//!
//! Three transactions get confused with one another whenever 7702 comes up, and
//! the preconf path treats them differently, so they are named here once:
//!
//! 1. **Setting a delegation** — a type-0x04 transaction carrying an authorization list. Refused on
//!    the preconf path (see `tx_type_whitelist.rs`).
//! 2. **Calling a delegated account** — an ordinary 0x02 transaction whose recipient happens to
//!    have a delegation designator. Accepted.
//! 3. **Sending from a delegated account** — an ordinary 0x02 transaction whose *sender* has one.
//!    Accepted (EIP-3607 carves 7702 out of its "no transactions from accounts with code" rule).
//!
//! (2) and (3) are the day-to-day use of a delegated account and must keep
//! working. They are the reason the refusal in (1) is scoped to a transaction
//! *type* rather than to "anything involving a delegation": the cheap
//! over-broad implementation — refuse whenever sender or recipient has code —
//! would pass a test that only checked (1) and break every delegated user.
//! `a_preconf_call_to_a_delegated_eoa_lands` and
//! `a_preconf_tx_from_a_delegated_eoa_lands` exist to fail in that case, and so
//! are deliberately **not** `#[ignore]`d: they pass today and must still pass
//! after.
//!
//! The third test covers the in-flight bound a delegated *sender* is held to,
//! which the preconf queue does not have yet.

use super::helpers::{PreconfCfgBuilder, mantle_chain_spec_with_delegated_account, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::ClientError;
use mantle_reth_rpc_ext::PreconfStatus;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

/// The account given a delegation designator in genesis when the test needs a
/// delegated *recipient*. Arbitrary, and deliberately not the test wallet.
const DELEGATED_RECIPIENT: Address = Address::new([0xD1; 20]);

/// What both delegated accounts point at. Its code is `STOP`, so calling
/// through a delegation succeeds and does nothing — a test asserting on the
/// *transaction* is then not also asserting on the delegate's behaviour.
const DELEGATE_TARGET: Address = Address::new([0xD2; 20]);

/// Plain EIP-1559 transfer. Explicitly **not** a 7702 transaction — every test
/// in this file is about ordinary transactions meeting delegated accounts.
async fn signed_transfer(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
    to: Address,
    gas_limit: u64,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(to)),
        gas: Some(gas_limit),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Drive one build and return the sealed block's transaction hashes, with the
/// preconf RPC's own result alongside.
///
/// The RPC call has to be in flight while the payload resolves — `send_preconf`
/// does not return until the builder has applied the transaction — so it is
/// spawned before the resolve and joined after.
macro_rules! build_one_block_with_preconf {
    ($node:expr, $http:expr, $raw_tx:expr) => {{
        let attrs = $node.payload.next_attributes();
        let fcu_state = $node.current_forkchoice_state().expect("forkchoice state");
        let payload_id = $node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(fcu_state, Some(attrs))
            .await
            .expect("FCU must succeed")
            .payload_id
            .expect("payload_id present when attributes are supplied");

        let http_clone = $http.clone();
        let raw = $raw_tx;
        let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw).await });

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let payload = $node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind not cancelled")
            .expect("payload build must produce a sealed payload");

        let event = rpc_task.await.expect("rpc task join");
        let sealed: Vec<B256> = payload
            .block()
            .body()
            .transactions()
            .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
            .collect();
        (event, sealed)
    }};
}

/// **Calling** a delegated account over preconf works.
///
/// The recipient carries `0xef0100 ‖ DELEGATE_TARGET`, so the call runs the
/// delegate's code. Nothing about that is a preconf concern: the transaction is
/// an ordinary 0x02.
///
/// Green today; must stay green. A type filter that keyed on "recipient has
/// code" instead of on the transaction type would fail here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preconf_call_to_a_delegated_eoa_lands() {
    let sender = Wallet::default().with_chain_id(1).inner.address();
    let spec = mantle_chain_spec_with_delegated_account(5000, DELEGATED_RECIPIENT, DELEGATE_TARGET);

    let cfg =
        PreconfCfgBuilder::new().whitelist_from(sender).whitelist_to(DELEGATED_RECIPIENT).build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg, spec).await;

    // Generous gas: the call runs the delegate's code, so it is not a bare
    // 21k transfer. The delegate is `STOP`, so almost none of it is used.
    let raw_tx = signed_transfer(chain_id, &wallet, 0, DELEGATED_RECIPIENT, 100_000).await;
    let (event, sealed) = build_one_block_with_preconf!(node, http, raw_tx);

    let event = event.expect("calling a delegated account is an ordinary tx and must be accepted");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "expected Success, got {:?} (reason={:?})",
        event.status,
        event.reason
    );
    assert!(
        sealed.contains(&event.tx_hash),
        "a preconf call to a delegated account must land; sealed = {sealed:?}"
    );
}

/// **Sending from** a delegated account over preconf works.
///
/// The test wallet itself carries the designator, so its account has code —
/// which EIP-3607 would normally bar from sending transactions at all. EIP-7702
/// carves exactly this case out, and reth implements the carve-out in the pool
/// validator, which the preconf path shares.
///
/// Green today; must stay green. This is the test a "refuse anything delegated"
/// implementation fails hardest — it locks a whitelisted sender out of the
/// preconf path entirely the moment they adopt 7702.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preconf_tx_from_a_delegated_eoa_lands() {
    let recipient: Address = Address::new([0x70; 20]);
    let sender = Wallet::default().with_chain_id(1).inner.address();
    // The wallet keeps its genesis balance — see
    // `mantle_chain_spec_with_delegated_account`, which merges rather than
    // replaces. An unfunded sender would fail for the wrong reason.
    let spec = mantle_chain_spec_with_delegated_account(5000, sender, DELEGATE_TARGET);

    let cfg = PreconfCfgBuilder::new().whitelist_from(sender).whitelist_to(recipient).build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg, spec).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0, recipient, 21_000).await;
    let (event, sealed) = build_one_block_with_preconf!(node, http, raw_tx);

    let event = event.expect(
        "EIP-3607 carves out 7702-delegated senders; a delegated account must still be \
         able to use the preconf path",
    );
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "expected Success, got {:?} (reason={:?})",
        event.status,
        event.reason
    );
    assert!(
        sealed.contains(&event.tx_hash),
        "a preconf tx from a delegated account must land; sealed = {sealed:?}"
    );
}

/// A delegated sender is held to one in-flight preconf transaction.
///
/// The transaction pool bounds delegated accounts this way
/// (`check_delegation_limit`, default `max_inflight_delegated_slot_limit = 1`)
/// because a delegated account can re-delegate, which changes what every
/// transaction queued behind it would execute — one transaction's cost
/// invalidates N. The preconf queue has no such bound, so the same account
/// gets a different answer depending on which endpoint it uses.
///
/// Setup detail worth knowing: **no build is started**, so the first
/// transaction stays `Waiting` in the queue rather than being applied and
/// leaving it. That is what makes it "in flight" when the second arrives. The
/// first request is spawned and never joined — it will time out, which is
/// expected and not what this test is about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegated_sender_may_hold_only_one_inflight_preconf() {
    let recipient: Address = Address::new([0x70; 20]);
    let sender = Wallet::default().with_chain_id(1).inner.address();
    let spec = mantle_chain_spec_with_delegated_account(5000, sender, DELEGATE_TARGET);

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        // Wide enough that the first request is still parked when the second
        // arrives; the second is expected to be refused synchronously, so the
        // test does not pay this.
        .preconf_timeout_ms(3_000)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg, spec).await;

    let first = signed_transfer(chain_id, &wallet, 0, recipient, 21_000).await;
    let http_clone = http.clone();
    let _parked = tokio::spawn(async move { send_preconf(&http_clone, first).await });

    // Let the first submission reach the queue before measuring occupancy.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let second = signed_transfer(chain_id, &wallet, 1, recipient, 21_000).await;
    let err = send_preconf(&http, second)
        .await
        .expect_err("a delegated sender's second in-flight preconf must be refused");

    // Word for word what `eth_sendRawTransaction` answers when the pool's own
    // delegation limit fires, so a client reads one sentence either way.
    match err {
        ClientError::Call(ref e) => {
            assert_eq!(e.message(), "in-flight transaction limit reached for delegated accounts");
        }
        other => panic!("expected Call error, got {other:?}"),
    }
}

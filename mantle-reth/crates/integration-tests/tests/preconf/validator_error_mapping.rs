//! Every validator refusal reason still reaches the client, and still names
//! itself.
//!
//! The preconf handler shares reth's (Mantle → OP → Eth) validator chain with
//! the transaction pool, and today wraps whatever it returns in a single
//! catch-all: `PreconfError::PoolRejected(format!("{}", e.kind))`. The direct-
//! admission design translates those refusals itself, so **the wrapper goes
//! away**.
//!
//! That makes two things worth separating:
//!
//! - **The wrapper text is allowed to change.** No assertion here mentions `pool rejected`. A test
//!   that pinned it would fail by design and teach nothing.
//! - **The reason must survive.** A client that could tell "your gas limit is below the intrinsic
//!   cost" from "you cannot afford this" must still be able to. That is what every assertion below
//!   checks, and why they are **green today and must stay green** rather than being staged behind
//!   `#[ignore]`.
//!
//! This file supersedes `validation_reject.rs`'s three `*_pool_rejects` tests,
//! which assert the wrapper itself and so cannot survive the change.
//!
//! Two reasons from the full set are deliberately absent:
//!
//! - **`--rpc.txfeecap`** — set from the node's RPC config, which this suite's launch harness does
//!   not plumb through. Covered indirectly by the `MantleTransactionValidator` unit tests.
//! - **EIP-3607 (sender has code)** — needs a funded account whose genesis code is *not* a
//!   delegation designator, since 7702 accounts are exempt. The exempt half is what matters here
//!   and is covered by `delegated_sender.rs`.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_op_hardforks::MANTLE_META_TX_PREFIX;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::ClientError;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Launch a node whose allowlist admits the test wallet to **any** recipient,
/// so nothing below is refused by the allowlist instead of by the reason under
/// test.
///
/// A macro rather than a function: the launch harness yields a node whose type
/// is not nameable here.
macro_rules! node_admitting_everything {
    () => {{
        let sender = Wallet::default().with_chain_id(1).inner.address();
        let cfg = PreconfCfgBuilder::new()
            .whitelist_from_wildcard(sender)
            // Short: every case here is refused before any queue entry exists,
            // so a correct implementation never waits. A regression that
            // defers the refusal shows up as a slow test rather than a hang.
            .preconf_timeout_ms(1_500)
            .build();
        launch_preconf_node!(cfg).await
    }};
}

/// Assert the submission was refused and the message still identifies why.
///
/// `alternatives` are accepted interchangeably: reth words some of these
/// differently across versions, and this file is pinning *that a reason is
/// stated*, not one exact phrasing.
fn assert_refused_naming(err: &ClientError, alternatives: &[&str], what: &str) {
    match err {
        ClientError::Call(e) => {
            let msg = e.message().to_lowercase();
            assert!(
                alternatives.iter().any(|needle| msg.contains(needle)),
                "the refusal for {what} must still name its reason (one of {alternatives:?}); \
                 got: {}",
                e.message()
            );
        }
        other => panic!("expected Call error for {what}, got {other:?}"),
    }
}

/// `gas_limit` below the intrinsic cost of the transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn intrinsic_gas_too_low_is_refused_with_its_reason() {
    let (_node, http, wallet, chain_id) = node_admitting_everything!();

    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(0),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        // Below the 21 000 floor for a bare call.
        gas: Some(20_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    let raw: Bytes =
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into();

    let err = send_preconf(&http, raw).await.expect_err("sub-intrinsic gas must be refused");
    assert_refused_naming(
        &err,
        &["intrinsic gas", "gas limit too low", "gas too low"],
        "sub-intrinsic gas limit",
    );
}

/// Transaction value plus fees beyond the sender's balance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn insufficient_funds_is_refused_with_its_reason() {
    let (_node, http, wallet, chain_id) = node_admitting_everything!();

    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(0),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        // Far beyond any genesis allocation.
        value: Some(U256::MAX / U256::from(2u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    let raw: Bytes =
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into();

    let err = send_preconf(&http, raw).await.expect_err("an unaffordable tx must be refused");
    assert_refused_naming(
        &err,
        &["insufficient", "funds", "balance", "overdraft"],
        "insufficient funds",
    );
}

/// `gas_limit` above the block gas limit — no block could ever hold it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn over_block_gas_limit_is_refused_with_its_reason() {
    let (_node, http, wallet, chain_id) = node_admitting_everything!();

    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(0),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        // Genesis gas limit is 0x1c9c380 (30 000 000).
        gas: Some(40_000_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    let raw: Bytes =
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into();

    let err = send_preconf(&http, raw).await.expect_err("an over-block-limit tx must be refused");
    assert_refused_naming(
        &err,
        &["gas limit", "gas_limit", "exceeds"],
        "gas limit above the block's",
    );
}

/// Contract creation whose init code exceeds the EIP-3860 ceiling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_init_code_is_refused_with_its_reason() {
    let (_node, http, wallet, chain_id) = node_admitting_everything!();

    // EIP-3860 caps init code at 49 152 bytes; go clearly past it.
    let init_code = Bytes::from(vec![0x60u8; 60_000]);
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(0),
        to: Some(TxKind::Create),
        // Comfortably above this payload's intrinsic cost (~1.02M: 21k base +
        // 32k create + 16/byte calldata + EIP-3860's 2/word) and below the
        // default `preconf_max_gas_per_tx` of 2M — otherwise the preconf
        // ceiling refuses it first and the test proves nothing about init code.
        gas: Some(1_900_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::ZERO),
        input: TransactionInput::from(init_code),
        ..Default::default()
    };
    let raw: Bytes =
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into();

    let err = send_preconf(&http, raw).await.expect_err("oversized init code must be refused");
    assert_refused_naming(
        &err,
        // reth words this "input size N exceeds max_init_code_size M"; the
        // underscored spelling is the one that actually appears, so both
        // spacings are accepted rather than betting on one.
        &["init_code", "init code", "initcode", "input size"],
        "oversized init code",
    );
}

/// Mantle refuses legacy `MetaTx` payloads outright (disabled since
/// `MantleEverest`) — a Mantle-specific rule layered on top of reth's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meta_tx_is_refused_with_its_reason() {
    let (_node, http, wallet, chain_id) = node_admitting_everything!();

    let mut input = MANTLE_META_TX_PREFIX.to_vec();
    input.push(0xF8); // minimal payload past the 32-byte prefix
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(0),
        to: Some(TxKind::Call(Address::ZERO)),
        gas: Some(100_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::ZERO),
        input: TransactionInput::from(Bytes::from(input)),
        ..Default::default()
    };
    let raw: Bytes =
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into();

    let err = send_preconf(&http, raw).await.expect_err("a MetaTx payload must be refused");
    assert_refused_naming(&err, &["meta tx", "metatx"], "legacy MetaTx");
}

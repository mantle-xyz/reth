//! Which EIP-2718 transaction types the preconf path accepts.
//!
//! Accepted: `Legacy` (0x00), `EIP-2930` (0x01), `EIP-1559` (0x02). Everything
//! else is refused by type, up front, with `PreconfError::UnsupportedTxType`.
//!
//! The **accepted** side is not re-tested here — every other file in this suite
//! submits 0x02 transactions and would fail loudly if they stopped being
//! admitted. What needs its own coverage is the refusals, and in particular
//! that they are decided by *type*:
//!
//! - **0x04 (EIP-7702 `SetCode`)** — refused so that no authorization list ever reaches this path.
//!   That is what lets the preconf queue skip the pool's `AuthorityReserved` accounting, which
//!   would need to see an authority's in-flight transactions — and those live in the pool, which
//!   this path deliberately never reads. The scope of the refusal is exactly "setting a
//!   delegation"; *using* a delegated account is untouched and is covered by `delegated_sender.rs`.
//! - **0x03 (EIP-4844 blob)** — already refused today, but by the pool validator, which reports it
//!   through the catch-all `pool rejected: …` wrapper. Deciding it by type moves the refusal
//!   earlier and gives it a name, which changes the error an SDK sees.
//!
//! Both tests whitelist the sender with a from-wildcard on purpose: if the
//! refusal came from the allowlist instead of the type gate the assertions
//! below would still see an `Err`, and the test would prove nothing. Passing
//! the allowlist first makes the type the only remaining reason.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::launch_preconf_node;
use jsonrpsee::core::ClientError;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

/// Assert a refusal names the transaction type rather than some earlier gate.
fn assert_unsupported_tx_type(err: &ClientError, ty: &str) {
    match err {
        ClientError::Call(e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("unsupported transaction type"),
                "expected a by-type refusal for {ty}, got: {}",
                e.message()
            );
            // Discriminating: had the allowlist refused first, the message
            // would say so and this test would be asserting nothing about
            // the type gate.
            assert!(
                !msg.contains("not preconf eligible"),
                "refused by the allowlist, not by type — the test setup no longer \
                 isolates the type gate: {}",
                e.message()
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }
}

/// A type-0x04 `SetCode` transaction is refused by type.
///
/// This is the one 7702 operation the preconf path does not serve: establishing
/// a delegation. Clients needing a preconfirmation for it do not get one and
/// must submit it through `eth_sendRawTransaction`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_code_tx_is_refused_by_type() {
    let sender = Wallet::default().with_chain_id(1).inner.address();
    // From-wildcard: the allowlist admits this sender whatever the recipient,
    // and `set_code_tx` picks a random one. See the module note on why.
    let cfg = PreconfCfgBuilder::new().whitelist_from_wildcard(sender).build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // Delegate the signer's own account to an arbitrary address — the
    // delegation target is irrelevant here; carrying an authorization list
    // at all is what makes this a 0x04.
    let delegate_to = alloy_primitives::Address::new([0xD2; 20]);
    let raw_tx =
        TransactionTestContext::set_code_tx_bytes(chain_id, delegate_to, wallet.inner.clone())
            .await;

    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("an EIP-7702 SetCode tx must be refused on the preconf path");
    assert_unsupported_tx_type(&err, "EIP-7702 (0x04)");
}

/// A type-0x03 blob transaction is refused **by type, not by the pool**.
///
/// The outcome is unchanged — blob transactions have never been admitted — but
/// the error an SDK receives is: today the pool validator's refusal arrives
/// wrapped as `pool rejected: …`, afterwards it is a named
/// `UnsupportedTxType`. Pinned here because that wrapper is a documented part
/// of the wire contract (`types.rs::preconf_error_display_wording_is_stable`)
/// and the change is easy to ship unnoticed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_tx_is_refused_by_type_not_by_the_pool() {
    let sender = Wallet::default().with_chain_id(1).inner.address();
    let cfg = PreconfCfgBuilder::new().whitelist_from_wildcard(sender).build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = TransactionTestContext::tx_with_blobs_bytes(chain_id, wallet.inner.clone())
        .await
        .expect("blob tx builds");

    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("a blob tx must be refused on the preconf path");
    assert_unsupported_tx_type(&err, "EIP-4844 (0x03)");

    // The point of the test: not merely that it was refused, but that the
    // refusal no longer comes through the pool's catch-all wrapper.
    if let ClientError::Call(e) = &err {
        assert!(
            !e.message().to_lowercase().contains("pool rejected"),
            "blob refusal must be decided by type before the pool is consulted; got: {}",
            e.message()
        );
    }
}

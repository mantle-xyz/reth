//! State export for debugging Mantle state root mismatches.
//!
//! Implements [`InvalidBlockHook`] to export full state (parent + bundle changes) to JSON
//! when a state root mismatch is detected.
//!
//! # Usage
//!
//! This hook is **compiled but not registered by default**. To activate it, wire it into
//! the node launch flow. For example, in `main.rs` before `builder.node(...).launch()`:
//!
//! ```rust,ignore
//! use mantle_reth_eth_api::debug::MantleStateExportHook;
//!
//! // Register as the invalid block hook (replaces the default Noop hook)
//! let hook = MantleStateExportHook::new(provider_factory.clone());
//! // Pass to engine tree via InvalidBlockHookExt or custom launch logic
//! ```
//!
//! Alternatively, use reth's built-in `--debug.invalid-block-hook witness` for lighter-weight
//! mismatch debugging (outputs diffs, not full state).

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, U256, hex, keccak256};
use reth_db::{tables, transaction::DbTx};
use reth_db_api::cursor::{DbCursorRO, DbDupCursorRO};
use reth_engine_primitives::InvalidBlockHook;
use reth_execution_types::BlockExecutionOutput;
use reth_primitives_traits::{Account, NodePrimitives, RecoveredBlock, SealedHeader};
use reth_provider::{DBProvider, DatabaseProviderFactory};
use reth_trie::HashedStorage;
use reth_trie_common::updates::TrieUpdates;
use reth_trie_db::{
    DatabaseHashedCursorFactory, DatabaseStorageRoot, DatabaseTrieCursorFactory, LegacyKeyAdapter,
};
use revm::database::BundleState;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    sync::Arc,
};
use tracing::info;

/// [`InvalidBlockHook`] that exports full state to JSON on state root mismatch.
///
/// Compiled but **not registered by default**—zero runtime overhead until wired in.
/// See module-level docs for activation instructions.
#[derive(Debug)]
pub struct MantleStateExportHook<F> {
    provider_factory: Arc<F>,
}

impl<F> MantleStateExportHook<F> {
    /// Creates a new hook. Does nothing until registered with the engine.
    pub fn new(provider_factory: Arc<F>) -> Self {
        Self { provider_factory }
    }
}

impl<N, F> InvalidBlockHook<N> for MantleStateExportHook<F>
where
    N: NodePrimitives,
    F: DatabaseProviderFactory + Send + Sync + 'static,
    F::Provider: DBProvider,
{
    fn on_invalid_block(
        &self,
        _parent_header: &SealedHeader<N::BlockHeader>,
        block: &RecoveredBlock<N::Block>,
        output: &BlockExecutionOutput<N::Receipt>,
        trie_updates: Option<(&TrieUpdates, B256)>,
    ) {
        let block_number = block.header().number();
        let computed_root = trie_updates.map(|(_, root)| root);
        let expected_root = block.header().state_root();
        let filename = format!("mantle_state_export_{block_number}.json");

        info!(
            target: "mantle::debug::state_export",
            block_number, ?computed_root, ?expected_root, %filename,
            "Exporting state on invalid block"
        );

        let provider = match self.provider_factory.database_provider_ro() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(target: "mantle::debug::state_export", %e, "Failed to get provider");
                return;
            }
        };

        if let Err(e) =
            export_full_state_with_bundle(&provider, &output.state, &filename, computed_root, true)
        {
            tracing::error!(target: "mantle::debug::state_export", %e, "State export failed");
        } else {
            info!(target: "mantle::debug::state_export", %filename, "State exported successfully");
        }
    }
}

// ——— Core export logic (ported from legacy state_export.rs) ———

fn extract_storage_key_mapping(bundle: &BundleState) -> BTreeMap<B256, BTreeMap<B256, B256>> {
    let mut mapping = BTreeMap::new();
    for (address, account) in &bundle.state {
        let hashed_addr = keccak256(address.as_slice());
        for slot in account.storage.keys() {
            let key = B256::from(*slot);
            mapping.entry(hashed_addr).or_insert_with(BTreeMap::new).insert(keccak256(key), key);
        }
    }
    mapping
}

fn extract_address_mapping(bundle: &BundleState) -> BTreeMap<B256, alloy_primitives::Address> {
    bundle.state.keys().map(|a| (keccak256(a.as_slice()), *a)).collect()
}

/// Export full state (database + bundle changes) to a JSON file.
///
/// This reads every account from the database, overlays `BundleState` changes,
/// and writes a single JSON containing all accounts with their storage.
pub fn export_full_state_with_bundle<Provider: DBProvider>(
    provider: &Provider,
    bundle_state: &BundleState,
    filename: &str,
    state_root: Option<B256>,
    bundle_storage_only: bool,
) -> eyre::Result<()> {
    // 1. Collect all accounts from database
    let mut all_accounts: BTreeMap<B256, Option<Account>> = BTreeMap::new();
    let mut cursor = provider.tx_ref().cursor_read::<tables::HashedAccounts>()?;
    while let Some((addr, info)) = cursor.next()? {
        all_accounts.insert(addr, Some(info));
    }

    // Apply bundle changes
    for (address, account) in &bundle_state.state {
        let hashed = keccak256(address.as_slice());
        if let Some(info) = &account.info {
            all_accounts.insert(hashed, Some(Account::from(info.clone())));
        } else {
            all_accounts.insert(hashed, None);
        }
    }
    all_accounts.retain(|_, a| a.is_some());

    // 2. Address + storage key mappings
    let mut addr_map: BTreeMap<B256, alloy_primitives::Address> = BTreeMap::new();
    let mut plain_cursor = provider.tx_ref().cursor_read::<tables::PlainAccountState>()?;
    while let Some((address, _)) = plain_cursor.next()? {
        addr_map.insert(keccak256(address.as_slice()), address);
    }
    addr_map.extend(extract_address_mapping(bundle_state));

    let mut key_map = BTreeMap::new();
    let mut storage_cursor = provider.tx_ref().cursor_dup_read::<tables::PlainStorageState>()?;
    while let Some((address, entry)) = storage_cursor.next()? {
        let ha = keccak256(address.as_slice());
        key_map.entry(ha).or_insert_with(BTreeMap::new).insert(keccak256(entry.key), entry.key);
    }
    for (ha, keys) in extract_storage_key_mapping(bundle_state) {
        key_map.entry(ha).or_insert_with(BTreeMap::new).extend(keys);
    }

    // 3. Write JSON
    let file = File::create(filename)?;
    let mut w = BufWriter::with_capacity(8 * 1024 * 1024, file);

    write!(
        w,
        "{{\n  \"state_root\": \"0x{}\",\n  \"accounts\": {{\n",
        hex::encode(state_root.unwrap_or_default())
    )?;

    let total = all_accounts.len();
    let mut first = true;
    let mut count = 0;

    for (hashed_addr, account_opt) in &all_accounts {
        let Some(account) = account_opt else { continue };
        count += 1;

        let resolved = addr_map.get(hashed_addr).copied();
        let is_bundle = resolved.is_some_and(|a| bundle_state.state.contains_key(&a));
        let export_storage = !bundle_storage_only || is_bundle;

        // Read hashed storage from DB
        let mut storage: BTreeMap<B256, U256> = BTreeMap::new();
        let mut hashed_for_root: BTreeMap<B256, U256> = BTreeMap::new();
        let mut hs_cursor = provider.tx_ref().cursor_dup_read::<tables::HashedStorages>()?;
        if let Some((found, entry)) = hs_cursor.seek_exact(*hashed_addr)? &&
            found == *hashed_addr
        {
            let orig = key_map
                .get(hashed_addr)
                .and_then(|m| m.get(&entry.key))
                .copied()
                .unwrap_or(entry.key);
            if export_storage {
                storage.insert(orig, entry.value);
            }
            hashed_for_root.insert(entry.key, entry.value);
            while let Some((_, e)) = hs_cursor.next_dup()? {
                let orig =
                    key_map.get(hashed_addr).and_then(|m| m.get(&e.key)).copied().unwrap_or(e.key);
                if export_storage {
                    storage.insert(orig, e.value);
                }
                hashed_for_root.insert(e.key, e.value);
            }
        }

        // Apply bundle storage changes
        if let Some(ba) = resolved.and_then(|a| bundle_state.state.get(&a)) {
            for (slot, sv) in &ba.storage {
                let orig = B256::from(*slot);
                let hk = keccak256(orig);
                if sv.present_value == U256::ZERO {
                    storage.remove(&orig);
                    hashed_for_root.remove(&hk);
                } else {
                    if export_storage {
                        storage.insert(orig, sv.present_value);
                    }
                    hashed_for_root.insert(hk, sv.present_value);
                }
            }
        }

        // Storage root
        let storage_hash = if hashed_for_root.is_empty() {
            "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421".into()
        } else if let Some(address) = resolved {
            let hs = HashedStorage::from_iter(false, hashed_for_root.iter().map(|(&k, &v)| (k, v)));
            type SR<'a, TX> = reth_trie::StorageRoot<
                DatabaseTrieCursorFactory<&'a TX, LegacyKeyAdapter>,
                DatabaseHashedCursorFactory<&'a TX>,
            >;
            match <SR<'_, _> as DatabaseStorageRoot<'_, _>>::overlay_root(
                provider.tx_ref(),
                address,
                hs,
            ) {
                Ok(r) => format!("0x{}", hex::encode(r)),
                Err(_) => {
                    "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421".into()
                }
            }
        } else {
            match <reth_trie::StorageRoot<
                DatabaseTrieCursorFactory<&'_ _, LegacyKeyAdapter>,
                DatabaseHashedCursorFactory<&'_ _>,
            > as DatabaseStorageRoot<'_, _>>::from_tx_hashed(
                provider.tx_ref(), *hashed_addr
            )
            .root()
            {
                Ok(r) => format!("0x{}", hex::encode(r)),
                Err(_) => {
                    "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421".into()
                }
            }
        };

        // Code
        let (code, code_hash) = match account.bytecode_hash {
            Some(h) if h != B256::ZERO => {
                let code = provider
                    .tx_ref()
                    .get_by_encoded_key::<tables::Bytecodes>(&h)
                    .ok()
                    .flatten()
                    .map(|b: reth_primitives_traits::Bytecode| {
                        format!("0x{}", hex::encode(b.original_bytes()))
                    })
                    .unwrap_or_else(|| "0x".into());
                (code, format!("0x{}", hex::encode(h)))
            }
            _ => ("0x".into(), format!("0x{}", hex::encode(alloy_primitives::KECCAK256_EMPTY))),
        };

        let addr_str = match resolved {
            Some(a) => format!("{a:?}"),
            None => format!("hashed:0x{}", hex::encode(hashed_addr)),
        };

        if !first {
            w.write_all(b",\n")?;
        }
        first = false;

        writeln!(w, "    \"{addr_str}\": {{")?;
        writeln!(w, "      \"hashed_address\": \"0x{}\",", hex::encode(hashed_addr))?;
        writeln!(w, "      \"balance\": \"{}\",", account.balance)?;
        writeln!(w, "      \"nonce\": {},", account.nonce)?;
        writeln!(w, "      \"code\": \"{code}\",")?;
        writeln!(w, "      \"code_hash\": \"{code_hash}\",")?;
        writeln!(w, "      \"storage_hash\": \"{storage_hash}\",")?;
        write!(w, "      \"storage\": {{")?;

        if export_storage && !storage.is_empty() {
            let mut first_s = true;
            for (key, val) in &storage {
                if !first_s {
                    w.write_all(b",")?;
                }
                first_s = false;
                let hk = keccak256(key);
                write!(
                    w,
                    "\n        \"0x{}\": {{ \"original_key\": \"0x{}\", \"hashed_key\": \"0x{}\", \"value\": \"0x{}\" }}",
                    hex::encode(key),
                    hex::encode(key),
                    hex::encode(hk),
                    hex::encode(val.to_be_bytes::<32>())
                )?;
            }
            w.write_all(b"\n      ")?;
        }
        w.write_all(b"}\n    }")?;

        if count % 1000 == 0 {
            info!(target: "mantle::debug::state_export", "Processed {count}/{total} accounts");
        }
    }

    w.write_all(b"\n  }\n}\n")?;
    w.flush()?;
    info!(target: "mantle::debug::state_export", %filename, total, "Export completed");
    Ok(())
}

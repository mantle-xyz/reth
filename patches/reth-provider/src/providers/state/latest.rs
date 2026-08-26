use crate::{
    AccountReader, BlockHashReader, HashedPostStateProvider, StateProvider, StateRootProvider,
};
use alloy_primitives::{Address, BlockNumber, Bytes, StorageKey, StorageValue, B256};
use reth_db_api::{cursor::DbDupCursorRO, tables, transaction::DbTx};
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{
    BytecodeReader, DBProvider, StateProofProvider, StorageRootProvider, StorageSettingsCache,
};
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use reth_trie::{
    hashed_cursor::HashedPostStateCursorFactory,
    proof::{Proof, StorageProof},
    trie_cursor::InMemoryTrieCursorFactory,
    updates::TrieUpdates,
    witness::TrieWitness,
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedStorage, KeccakKeyHasher,
    MultiProof, MultiProofTargets, StateRoot, StorageMultiProof, StorageRoot, TrieInput,
    TrieInputSorted,
};
use reth_trie_db::{DatabaseProof, DatabaseStateRoot, DatabaseStorageProof, DatabaseStorageRoot};

type DbStateRoot<'a, TX, A> = StateRoot<
    reth_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    reth_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;
type DbStorageRoot<'a, TX, A> = StorageRoot<
    reth_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    reth_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;
type DbStorageProof<'a, TX, A> = StorageProof<
    'static,
    reth_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    reth_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;
type DbProof<'a, TX, A> = Proof<
    reth_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    reth_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;
/// State provider over latest state that takes tx reference.
///
/// Wraps a [`DBProvider`] to get access to database.
#[derive(Debug)]
pub struct LatestStateProviderRef<'b, Provider>(&'b Provider);

impl<'b, Provider: DBProvider> LatestStateProviderRef<'b, Provider> {
    /// Create new state provider
    pub const fn new(provider: &'b Provider) -> Self {
        Self(provider)
    }

    fn tx(&self) -> &Provider::Tx {
        self.0.tx_ref()
    }

    fn hashed_storage_lookup(
        &self,
        hashed_address: B256,
        hashed_slot: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let mut cursor = self.tx().cursor_dup_read::<tables::HashedStorages>()?;
        Ok(cursor
            .seek_by_key_subkey(hashed_address, hashed_slot)?
            .filter(|e| e.key == hashed_slot)
            .map(|e| e.value))
    }
}

impl<Provider: DBProvider + StorageSettingsCache> AccountReader
    for LatestStateProviderRef<'_, Provider>
{
    /// Get basic account information.
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        if self.0.cached_storage_settings().use_hashed_state() {
            let hashed_address = alloy_primitives::keccak256(address);
            self.tx()
                .get_by_encoded_key::<tables::HashedAccounts>(&hashed_address)
                .map_err(Into::into)
        } else {
            // [MANTLE PATCH] (hashed-snapshot exec fix): on a legacy (storage_v2=false) DB built via
            // `init-state --without-evm` from a hashed-only snapshot, accounts whose address
            // preimage was unavailable live ONLY in `HashedAccounts` (never in `PlainAccountState`).
            // The plain lookup below misses them and returns None → the executor reads their balance
            // as 0 → stateRoot diverges → consensus fork. When the hashed-snapshot marker is set,
            // fall back to the hashed table so execution reads the real value. This only triggers on
            // snapshot-imported DBs; ordinary legacy DBs always hit plain and never reach the fallback.
            if let Some(account) =
                self.tx().get_by_encoded_key::<tables::PlainAccountState>(address)?
            {
                return Ok(Some(account));
            }
            if super::has_hashed_snapshot_marker(self.tx())? {
                let hashed_address = alloy_primitives::keccak256(address);
                return self
                    .tx()
                    .get_by_encoded_key::<tables::HashedAccounts>(&hashed_address)
                    .map_err(Into::into);
            }
            Ok(None)
        }
    }
}

impl<Provider: BlockHashReader> BlockHashReader for LatestStateProviderRef<'_, Provider> {
    /// Get block hash by number.
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.0.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.0.canonical_hashes_range(start, end)
    }
}

impl<Provider: DBProvider + StorageSettingsCache> StateRootProvider
    for LatestStateProviderRef<'_, Provider>
{
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        reth_trie_db::with_adapter!(self.0, |A| {
            let sorted = hashed_state.into_sorted();
            Ok(<DbStateRoot<'_, _, A> as DatabaseStateRoot<_>>::overlay_root(self.tx(), &sorted)?)
        })
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        reth_trie_db::with_adapter!(self.0, |A| {
            Ok(<DbStateRoot<'_, _, A> as DatabaseStateRoot<_>>::overlay_root_from_nodes(
                self.tx(),
                TrieInputSorted::from_unsorted(input),
            )?)
        })
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        reth_trie_db::with_adapter!(self.0, |A| {
            let sorted = hashed_state.into_sorted();
            Ok(<DbStateRoot<'_, _, A> as DatabaseStateRoot<_>>::overlay_root_with_updates(
                self.tx(),
                &sorted,
            )?)
        })
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        reth_trie_db::with_adapter!(self.0, |A| {
            Ok(
                <DbStateRoot<'_, _, A> as DatabaseStateRoot<_>>::overlay_root_from_nodes_with_updates(
                    self.tx(),
                    TrieInputSorted::from_unsorted(input),
                )?,
            )
        })
    }
}

impl<Provider: DBProvider + StorageSettingsCache> StorageRootProvider
    for LatestStateProviderRef<'_, Provider>
{
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        reth_trie_db::with_adapter!(self.0, |A| {
            <DbStorageRoot<'_, _, A>>::overlay_root(self.tx(), address, hashed_storage)
                .map_err(|err| ProviderError::Database(err.into()))
        })
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<reth_trie::StorageProof> {
        reth_trie_db::with_adapter!(self.0, |A| {
            <DbStorageProof<'_, _, A>>::overlay_storage_proof(
                self.tx(),
                address,
                slot,
                hashed_storage,
            )
            .map_err(ProviderError::from)
        })
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        reth_trie_db::with_adapter!(self.0, |A| {
            <DbStorageProof<'_, _, A>>::overlay_storage_multiproof(
                self.tx(),
                address,
                slots,
                hashed_storage,
            )
            .map_err(ProviderError::from)
        })
    }
}

impl<Provider: DBProvider + StorageSettingsCache> StateProofProvider
    for LatestStateProviderRef<'_, Provider>
{
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        reth_trie_db::with_adapter!(self.0, |A| {
            let proof = <DbProof<'_, _, A> as DatabaseProof>::from_tx(self.tx());
            proof.overlay_account_proof(input, address, slots).map_err(ProviderError::from)
        })
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        reth_trie_db::with_adapter!(self.0, |A| {
            let proof = <DbProof<'_, _, A> as DatabaseProof>::from_tx(self.tx());
            proof.overlay_multiproof(input, targets).map_err(ProviderError::from)
        })
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        reth_trie_db::with_adapter!(self.0, |A| {
            let nodes_sorted = input.nodes.into_sorted();
            let state_sorted = input.state.into_sorted();
            let witness = TrieWitness::new(
                InMemoryTrieCursorFactory::new(
                    reth_trie_db::DatabaseTrieCursorFactory::<_, A>::new(self.tx()),
                    &nodes_sorted,
                ),
                HashedPostStateCursorFactory::new(
                    reth_trie_db::DatabaseHashedCursorFactory::new(self.tx()),
                    &state_sorted,
                ),
            )
            .with_prefix_sets_mut(input.prefix_sets)
            .with_execution_witness_mode(mode);
            let witness =
                if mode.is_canonical() { witness } else { witness.always_include_root_node() };
            let mut values: Vec<_> = witness.compute(target)?.into_values().collect();
            if mode.is_canonical() {
                values.sort_unstable();
            }
            Ok(values)
        })
    }
}

impl<Provider: DBProvider> HashedPostStateProvider for LatestStateProviderRef<'_, Provider> {
    fn hashed_post_state(&self, bundle_state: &revm_database::BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<Provider: DBProvider + BlockHashReader + StorageSettingsCache> StateProvider
    for LatestStateProviderRef<'_, Provider>
{
    /// Get storage by plain (unhashed) storage key slot.
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        if self.0.cached_storage_settings().use_hashed_state() {
            self.hashed_storage_lookup(
                alloy_primitives::keccak256(account),
                alloy_primitives::keccak256(storage_key),
            )
        } else {
            let mut cursor = self.tx().cursor_dup_read::<tables::PlainStorageState>()?;
            if let Some(entry) = cursor.seek_by_key_subkey(account, storage_key)? &&
                entry.key == storage_key
            {
                return Ok(Some(entry.value));
            }
            // [MANTLE PATCH] (hashed-snapshot exec fix): mirror basic_account. Storage of a
            // preimage-less account imported from a hashed-only snapshot lives only in
            // `HashedStorages`. Fall back to it when the marker is set so execution reads the
            // real slot value instead of 0.
            if super::has_hashed_snapshot_marker(self.tx())? {
                return self.hashed_storage_lookup(
                    alloy_primitives::keccak256(account),
                    alloy_primitives::keccak256(storage_key),
                );
            }
            Ok(None)
        }
    }
}

impl<Provider: DBProvider + BlockHashReader> BytecodeReader
    for LatestStateProviderRef<'_, Provider>
{
    /// Get account code by its hash
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.tx().get_by_encoded_key::<tables::Bytecodes>(code_hash).map_err(Into::into)
    }
}

/// State provider for the latest state.
#[derive(Debug)]
pub struct LatestStateProvider<Provider>(Provider);

impl<Provider: DBProvider> LatestStateProvider<Provider> {
    /// Create new state provider
    pub const fn new(db: Provider) -> Self {
        Self(db)
    }

    /// Returns a new provider that takes the `TX` as reference
    #[inline(always)]
    const fn as_ref(&self) -> LatestStateProviderRef<'_, Provider> {
        LatestStateProviderRef::new(&self.0)
    }
}

// Delegates all provider impls to [LatestStateProviderRef]
reth_storage_api::macros::delegate_provider_impls!(LatestStateProvider<Provider> where [Provider: DBProvider + BlockHashReader + StorageSettingsCache]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{create_test_provider_factory, MockNodeTypesWithDB};
    use crate::ProviderFactory;
    use alloy_primitives::{address, b256, keccak256, U256};
    use reth_db_api::{
        models::StorageSettings,
        tables,
        transaction::{DbTx, DbTxMut},
    };
    use reth_primitives_traits::StorageEntry;
    use reth_storage_api::StorageSettingsCache;

    const fn assert_state_provider<T: StateProvider>() {}
    #[expect(dead_code)]
    const fn assert_latest_state_provider<
        T: DBProvider + BlockHashReader + StorageSettingsCache,
    >() {
        assert_state_provider::<LatestStateProvider<T>>();
    }

    #[test]
    fn test_latest_storage_hashed_state() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(StorageSettings::v2());

        let address = address!("0x0000000000000000000000000000000000000001");
        let slot = b256!("0x0000000000000000000000000000000000000000000000000000000000000001");

        let hashed_address = keccak256(address);
        let hashed_slot = keccak256(slot);

        let tx = factory.provider_rw().unwrap().into_tx();
        tx.put::<tables::HashedStorages>(
            hashed_address,
            StorageEntry { key: hashed_slot, value: U256::from(42) },
        )
        .unwrap();
        tx.commit().unwrap();

        let db = factory.provider().unwrap();
        let provider_ref = LatestStateProviderRef::new(&db);

        assert_eq!(provider_ref.storage(address, slot).unwrap(), Some(U256::from(42)));

        let other_address = address!("0x0000000000000000000000000000000000000099");
        let other_slot =
            b256!("0x0000000000000000000000000000000000000000000000000000000000000099");
        assert_eq!(provider_ref.storage(other_address, other_slot).unwrap(), None);

        let tx = factory.provider_rw().unwrap().into_tx();
        let plain_address = address!("0x0000000000000000000000000000000000000002");
        let plain_slot =
            b256!("0x0000000000000000000000000000000000000000000000000000000000000002");
        tx.put::<tables::PlainStorageState>(
            plain_address,
            StorageEntry { key: plain_slot, value: U256::from(99) },
        )
        .unwrap();
        tx.commit().unwrap();

        let db = factory.provider().unwrap();
        let provider_ref = LatestStateProviderRef::new(&db);
        assert_eq!(provider_ref.storage(plain_address, plain_slot).unwrap(), None);
    }

    #[test]
    fn test_latest_storage_hashed_state_returns_none_for_missing() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(StorageSettings::v2());

        let address = address!("0x0000000000000000000000000000000000000001");
        let slot = b256!("0x0000000000000000000000000000000000000000000000000000000000000001");

        let db = factory.provider().unwrap();
        let provider_ref = LatestStateProviderRef::new(&db);
        assert_eq!(provider_ref.storage(address, slot).unwrap(), None);
    }

    #[test]
    fn test_latest_storage_legacy() {
        let factory = create_test_provider_factory();
        assert!(!factory.provider().unwrap().cached_storage_settings().use_hashed_state());

        let address = address!("0x0000000000000000000000000000000000000001");
        let slot = b256!("0x0000000000000000000000000000000000000000000000000000000000000005");

        let tx = factory.provider_rw().unwrap().into_tx();
        tx.put::<tables::PlainStorageState>(
            address,
            StorageEntry { key: slot, value: U256::from(42) },
        )
        .unwrap();
        tx.commit().unwrap();

        let db = factory.provider().unwrap();
        let provider_ref = LatestStateProviderRef::new(&db);

        assert_eq!(provider_ref.storage(address, slot).unwrap(), Some(U256::from(42)));

        let other_slot =
            b256!("0x0000000000000000000000000000000000000000000000000000000000000099");
        assert_eq!(provider_ref.storage(address, other_slot).unwrap(), None);
    }

    #[test]
    fn test_latest_storage_legacy_does_not_read_hashed() {
        let factory = create_test_provider_factory();
        assert!(!factory.provider().unwrap().cached_storage_settings().use_hashed_state());

        let address = address!("0x0000000000000000000000000000000000000001");
        let slot = b256!("0x0000000000000000000000000000000000000000000000000000000000000005");
        let hashed_address = keccak256(address);
        let hashed_slot = keccak256(slot);

        let tx = factory.provider_rw().unwrap().into_tx();
        tx.put::<tables::HashedStorages>(
            hashed_address,
            StorageEntry { key: hashed_slot, value: U256::from(42) },
        )
        .unwrap();
        tx.commit().unwrap();

        let db = factory.provider().unwrap();
        let provider_ref = LatestStateProviderRef::new(&db);
        assert_eq!(provider_ref.storage(address, slot).unwrap(), None);
    }

    // ==================== [MANTLE PATCH] hashed-snapshot exec-read fallback ====================
    // Coverage matrix for the fix. Legacy layout (storage_v2=false) is the rpc41/init-state case.
    // The marker (`__hashed_only_state_snapshot__`) gates whether a plain-state miss falls back to
    // the hashed tables. These tests pin the full behavior matrix so the fix can't silently regress
    // into either the v1.9.3 bug (historical RPC) or the pre-fix v2.2.1 bug (execution fork).

    /// Writes the hashed-snapshot marker so `has_hashed_snapshot_marker` returns true.
    fn set_hashed_snapshot_marker(factory: &ProviderFactory<MockNodeTypesWithDB>) {
        let tx = factory.provider_rw().unwrap().into_tx();
        tx.put::<tables::StageCheckpointProgresses>(
            super::super::HASHED_SNAPSHOT_MARKER.to_string(),
            vec![1u8],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    const A: Address = address!("0x00000000000000000000000000000000000000aa");

    fn put_hashed_account(factory: &ProviderFactory<MockNodeTypesWithDB>, addr: Address, bal: u64) {
        let tx = factory.provider_rw().unwrap().into_tx();
        tx.put::<tables::HashedAccounts>(
            keccak256(addr),
            Account { nonce: 0, balance: U256::from(bal), bytecode_hash: None },
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn put_plain_account(factory: &ProviderFactory<MockNodeTypesWithDB>, addr: Address, bal: u64) {
        let tx = factory.provider_rw().unwrap().into_tx();
        tx.put::<tables::PlainAccountState>(
            addr,
            Account { nonce: 0, balance: U256::from(bal), bytecode_hash: None },
        )
        .unwrap();
        tx.commit().unwrap();
    }

    /// 缺原像账户（只在 HashedAccounts），legacy + 无 marker → 返回 None（保留旧语义，防回归）。
    #[test]
    fn basic_account_legacy_no_marker_ignores_hashed() {
        let factory = create_test_provider_factory();
        assert!(!factory.provider().unwrap().cached_storage_settings().use_hashed_state());
        put_hashed_account(&factory, A, 100);

        let db = factory.provider().unwrap();
        assert_eq!(LatestStateProviderRef::new(&db).basic_account(&A).unwrap(), None);
    }

    /// 缺原像账户，legacy + 有 marker → 回退读 HashedAccounts，返回真实值（修复行为，修分叉）。
    #[test]
    fn basic_account_legacy_marker_reads_hashed() {
        let factory = create_test_provider_factory();
        put_hashed_account(&factory, A, 100);
        set_hashed_snapshot_marker(&factory);

        let db = factory.provider().unwrap();
        let acc = LatestStateProviderRef::new(&db).basic_account(&A).unwrap();
        assert_eq!(acc.map(|a| a.balance), Some(U256::from(100)));
    }

    /// 有原像的普通账户（在 PlainAccountState），legacy + 有 marker → 仍读 plain，marker 不干扰。
    #[test]
    fn basic_account_legacy_marker_prefers_plain() {
        let factory = create_test_provider_factory();
        put_plain_account(&factory, A, 55);
        // 同时在 hashed 放一个不同值，确认命中的是 plain 而非 hashed。
        put_hashed_account(&factory, A, 999);
        set_hashed_snapshot_marker(&factory);

        let db = factory.provider().unwrap();
        let acc = LatestStateProviderRef::new(&db).basic_account(&A).unwrap();
        assert_eq!(acc.map(|a| a.balance), Some(U256::from(55)));
    }

    /// 全新/从不存在的账户，legacy + 有 marker + 两表皆无 → 返回 None（不会误报，仍正确返 0）。
    #[test]
    fn basic_account_legacy_marker_absent_account_is_none() {
        let factory = create_test_provider_factory();
        set_hashed_snapshot_marker(&factory);

        let db = factory.provider().unwrap();
        assert_eq!(LatestStateProviderRef::new(&db).basic_account(&A).unwrap(), None);
    }

    /// storage：缺原像账户的槽只在 HashedStorages，legacy + 有 marker → 回退读到真实值。
    #[test]
    fn storage_legacy_marker_reads_hashed() {
        let factory = create_test_provider_factory();
        let slot = b256!("0x0000000000000000000000000000000000000000000000000000000000000005");
        let tx = factory.provider_rw().unwrap().into_tx();
        tx.put::<tables::HashedStorages>(
            keccak256(A),
            StorageEntry { key: keccak256(slot), value: U256::from(42) },
        )
        .unwrap();
        tx.commit().unwrap();
        set_hashed_snapshot_marker(&factory);

        let db = factory.provider().unwrap();
        assert_eq!(
            LatestStateProviderRef::new(&db).storage(A, slot).unwrap(),
            Some(U256::from(42))
        );
    }

    /// storage：全新槽，legacy + 有 marker + 两表皆无 → 返回 None。
    #[test]
    fn storage_legacy_marker_absent_slot_is_none() {
        let factory = create_test_provider_factory();
        set_hashed_snapshot_marker(&factory);
        let slot = b256!("0x0000000000000000000000000000000000000000000000000000000000000005");

        let db = factory.provider().unwrap();
        assert_eq!(LatestStateProviderRef::new(&db).storage(A, slot).unwrap(), None);
    }
}

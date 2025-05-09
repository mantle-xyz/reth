//! Merkle trie proofs.

use crate::{Nibbles, TrieAccount};
use alloc::vec::Vec;
use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{
    keccak256,
    map::{hash_map, B256Map, B256Set, HashMap},
    Address, Bytes, B256, U256,
};
use alloy_rlp::{encode_fixed_size, Decodable, EMPTY_STRING_CODE};
use alloy_trie::{
    nodes::TrieNode,
    proof::{verify_proof, DecodedProofNodes, ProofNodes, ProofVerificationError},
    TrieMask, EMPTY_ROOT_HASH,
};
use itertools::Itertools;
use reth_primitives_traits::Account;

/// Proof targets map.
pub type MultiProofTargets = B256Map<B256Set>;

/// The state multiproof of target accounts and multiproofs of their storage tries.
/// Multiproof is effectively a state subtrie that only contains the nodes
/// in the paths of target accounts.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct MultiProof {
    /// State trie multiproof for requested accounts.
    pub account_subtree: ProofNodes,
    /// The hash masks of the branch nodes in the account proof.
    pub branch_node_hash_masks: HashMap<Nibbles, TrieMask>,
    /// The tree masks of the branch nodes in the account proof.
    pub branch_node_tree_masks: HashMap<Nibbles, TrieMask>,
    /// Storage trie multiproofs.
    pub storages: B256Map<StorageMultiProof>,
}

impl MultiProof {
    /// Returns true if the multiproof is empty.
    pub fn is_empty(&self) -> bool {
        self.account_subtree.is_empty() &&
            self.branch_node_hash_masks.is_empty() &&
            self.branch_node_tree_masks.is_empty() &&
            self.storages.is_empty()
    }

    /// Return the account proof nodes for the given account path.
    pub fn account_proof_nodes(&self, path: &Nibbles) -> Vec<(Nibbles, Bytes)> {
        self.account_subtree.matching_nodes_sorted(path)
    }

    /// Return the storage proof nodes for the given storage slots of the account path.
    pub fn storage_proof_nodes(
        &self,
        hashed_address: B256,
        slots: impl IntoIterator<Item = B256>,
    ) -> Vec<(B256, Vec<(Nibbles, Bytes)>)> {
        self.storages
            .get(&hashed_address)
            .map(|storage_mp| {
                slots
                    .into_iter()
                    .map(|slot| {
                        let nibbles = Nibbles::unpack(slot);
                        (slot, storage_mp.subtree.matching_nodes_sorted(&nibbles))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Construct the account proof from the multiproof.
    pub fn account_proof(
        &self,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof, alloy_rlp::Error> {
        let hashed_address = keccak256(address);
        let nibbles = Nibbles::unpack(hashed_address);

        // Retrieve the account proof.
        let proof = self
            .account_proof_nodes(&nibbles)
            .into_iter()
            .map(|(_, node)| node)
            .collect::<Vec<_>>();

        // Inspect the last node in the proof. If it's a leaf node with matching suffix,
        // then the node contains the encoded trie account.
        let info = 'info: {
            if let Some(last) = proof.last() {
                if let TrieNode::Leaf(leaf) = TrieNode::decode(&mut &last[..])? {
                    if nibbles.ends_with(&leaf.key) {
                        let account = TrieAccount::decode(&mut &leaf.value[..])?;
                        break 'info Some(Account {
                            balance: account.balance,
                            nonce: account.nonce,
                            bytecode_hash: (account.code_hash != KECCAK_EMPTY)
                                .then_some(account.code_hash),
                        })
                    }
                }
            }
            None
        };

        // Retrieve proofs for requested storage slots.
        let storage_multiproof = self.storages.get(&hashed_address);
        let storage_root = storage_multiproof.map(|m| m.root).unwrap_or(EMPTY_ROOT_HASH);
        let mut storage_proofs = Vec::with_capacity(slots.len());
        for slot in slots {
            let proof = if let Some(multiproof) = &storage_multiproof {
                multiproof.storage_proof(*slot)?
            } else {
                StorageProof::new(*slot)
            };
            storage_proofs.push(proof);
        }
        Ok(AccountProof { address, info, proof, storage_root, storage_proofs })
    }

    /// Extends this multiproof with another one, merging both account and storage
    /// proofs.
    pub fn extend(&mut self, other: Self) {
        self.account_subtree.extend_from(other.account_subtree);

        self.branch_node_hash_masks.extend(other.branch_node_hash_masks);
        self.branch_node_tree_masks.extend(other.branch_node_tree_masks);

        for (hashed_address, storage) in other.storages {
            match self.storages.entry(hashed_address) {
                hash_map::Entry::Occupied(mut entry) => {
                    debug_assert_eq!(entry.get().root, storage.root);
                    let entry = entry.get_mut();
                    entry.subtree.extend_from(storage.subtree);
                    entry.branch_node_hash_masks.extend(storage.branch_node_hash_masks);
                    entry.branch_node_tree_masks.extend(storage.branch_node_tree_masks);
                }
                hash_map::Entry::Vacant(entry) => {
                    entry.insert(storage);
                }
            }
        }
    }
}

/// The merkle multiproof of storage trie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageMultiProof {
    /// Storage trie root.
    pub root: B256,
    /// Storage multiproof for requested slots.
    pub subtree: ProofNodes,
    /// The hash masks of the branch nodes in the storage proof.
    pub branch_node_hash_masks: HashMap<Nibbles, TrieMask>,
    /// The tree masks of the branch nodes in the storage proof.
    pub branch_node_tree_masks: HashMap<Nibbles, TrieMask>,
}

impl StorageMultiProof {
    /// Create new storage multiproof for empty trie.
    pub fn empty() -> Self {
        Self {
            root: EMPTY_ROOT_HASH,
            subtree: ProofNodes::from_iter([(
                Nibbles::default(),
                Bytes::from([EMPTY_STRING_CODE]),
            )]),
            branch_node_hash_masks: HashMap::default(),
            branch_node_tree_masks: HashMap::default(),
        }
    }

    /// Return storage proofs for the target storage slot (unhashed).
    pub fn storage_proof(&self, slot: B256) -> Result<StorageProof, alloy_rlp::Error> {
        let nibbles = Nibbles::unpack(keccak256(slot));

        // Retrieve the storage proof.
        let proof = self
            .subtree
            .matching_nodes_iter(&nibbles)
            .sorted_by(|a, b| a.0.cmp(b.0))
            .map(|(_, node)| node.clone())
            .collect::<Vec<_>>();

        // Inspect the last node in the proof. If it's a leaf node with matching suffix,
        // then the node contains the encoded slot value.
        let value = 'value: {
            if let Some(last) = proof.last() {
                if let TrieNode::Leaf(leaf) = TrieNode::decode(&mut &last[..])? {
                    if nibbles.ends_with(&leaf.key) {
                        break 'value U256::decode(&mut &leaf.value[..])?
                    }
                }
            }
            U256::ZERO
        };

        Ok(StorageProof { key: slot, nibbles, value, proof })
    }
}

/// The decoded merkle multiproof for a storage trie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedStorageMultiProof {
    /// Storage trie root.
    pub root: B256,
    /// Storage multiproof for requested slots.
    pub subtree: DecodedProofNodes,
    /// The hash masks of the branch nodes in the storage proof.
    pub branch_node_hash_masks: HashMap<Nibbles, TrieMask>,
    /// The tree masks of the branch nodes in the storage proof.
    pub branch_node_tree_masks: HashMap<Nibbles, TrieMask>,
}

impl DecodedStorageMultiProof {
    /// Create new storage multiproof for empty trie.
    pub fn empty() -> Self {
        Self {
            root: EMPTY_ROOT_HASH,
            subtree: DecodedProofNodes::from_iter([(Nibbles::default(), TrieNode::EmptyRoot)]),
            branch_node_hash_masks: HashMap::default(),
            branch_node_tree_masks: HashMap::default(),
        }
    }

    /// Return storage proofs for the target storage slot (unhashed).
    pub fn storage_proof(&self, slot: B256) -> Result<DecodedStorageProof, alloy_rlp::Error> {
        let nibbles = Nibbles::unpack(keccak256(slot));

        // Retrieve the storage proof.
        let proof = self
            .subtree
            .matching_nodes_iter(&nibbles)
            .sorted_by(|a, b| a.0.cmp(b.0))
            .map(|(_, node)| node.clone())
            .collect::<Vec<_>>();

        // Inspect the last node in the proof. If it's a leaf node with matching suffix,
        // then the node contains the encoded slot value.
        let value = 'value: {
            if let Some(TrieNode::Leaf(leaf)) = proof.last() {
                if nibbles.ends_with(&leaf.key) {
                    break 'value U256::decode(&mut &leaf.value[..])?
                }
            }
            U256::ZERO
        };

        Ok(DecodedStorageProof { key: slot, nibbles, value, proof })
    }
}

/// The merkle proof with the relevant account info.
#[derive(Clone, PartialEq, Eq, Debug)]
#[cfg_attr(any(test, feature = "serde"), derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(any(test, feature = "serde"), serde(rename_all = "camelCase"))]
pub struct AccountProof {
    /// The address associated with the account.
    pub address: Address,
    /// Account info.
    pub info: Option<Account>,
    /// Array of rlp-serialized merkle trie nodes which starting from the root node and
    /// following the path of the hashed address as key.
    pub proof: Vec<Bytes>,
    /// The storage trie root.
    pub storage_root: B256,
    /// Array of storage proofs as requested.
    pub storage_proofs: Vec<StorageProof>,
}

#[cfg(feature = "eip1186")]
impl AccountProof {
    /// Convert into an EIP-1186 account proof response
    pub fn into_eip1186_response(
        self,
        slots: Vec<alloy_serde::JsonStorageKey>,
    ) -> alloy_rpc_types_eth::EIP1186AccountProofResponse {
        let info = self.info.unwrap_or_default();
        alloy_rpc_types_eth::EIP1186AccountProofResponse {
            address: self.address,
            balance: info.balance,
            code_hash: info.get_bytecode_hash(),
            nonce: info.nonce,
            storage_hash: self.storage_root,
            account_proof: self.proof,
            storage_proof: self
                .storage_proofs
                .into_iter()
                .filter_map(|proof| {
                    let input_slot = slots.iter().find(|s| s.as_b256() == proof.key)?;
                    Some(proof.into_eip1186_proof(*input_slot))
                })
                .collect(),
        }
    }
}

impl Default for AccountProof {
    fn default() -> Self {
        Self::new(Address::default())
    }
}

impl AccountProof {
    /// Create new account proof entity.
    pub const fn new(address: Address) -> Self {
        Self {
            address,
            info: None,
            proof: Vec::new(),
            storage_root: EMPTY_ROOT_HASH,
            storage_proofs: Vec::new(),
        }
    }

    /// Verify the storage proofs and account proof against the provided state root.
    pub fn verify(&self, root: B256) -> Result<(), ProofVerificationError> {
        // Verify storage proofs.
        for storage_proof in &self.storage_proofs {
            storage_proof.verify(self.storage_root)?;
        }

        // Verify the account proof.
        let expected = if self.info.is_none() && self.storage_root == EMPTY_ROOT_HASH {
            None
        } else {
            Some(alloy_rlp::encode(
                self.info.unwrap_or_default().into_trie_account(self.storage_root),
            ))
        };
        let nibbles = Nibbles::unpack(keccak256(self.address));
        verify_proof(root, nibbles, expected, &self.proof)
    }
}

/// The merkle proof of the storage entry.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
#[cfg_attr(any(test, feature = "serde"), derive(serde::Serialize, serde::Deserialize))]
pub struct StorageProof {
    /// The raw storage key.
    pub key: B256,
    /// The hashed storage key nibbles.
    pub nibbles: Nibbles,
    /// The storage value.
    pub value: U256,
    /// Array of rlp-serialized merkle trie nodes which starting from the storage root node and
    /// following the path of the hashed storage slot as key.
    pub proof: Vec<Bytes>,
}

impl StorageProof {
    /// Convert into an EIP-1186 storage proof
    #[cfg(feature = "eip1186")]
    pub fn into_eip1186_proof(
        self,
        slot: alloy_serde::JsonStorageKey,
    ) -> alloy_rpc_types_eth::EIP1186StorageProof {
        alloy_rpc_types_eth::EIP1186StorageProof { key: slot, value: self.value, proof: self.proof }
    }
}

impl StorageProof {
    /// Create new storage proof from the storage slot.
    pub fn new(key: B256) -> Self {
        let nibbles = Nibbles::unpack(keccak256(key));
        Self { key, nibbles, ..Default::default() }
    }

    /// Create new storage proof from the storage slot and its pre-hashed image.
    pub fn new_with_hashed(key: B256, hashed_key: B256) -> Self {
        Self { key, nibbles: Nibbles::unpack(hashed_key), ..Default::default() }
    }

    /// Create new storage proof from the storage slot and its pre-hashed image.
    pub fn new_with_nibbles(key: B256, nibbles: Nibbles) -> Self {
        Self { key, nibbles, ..Default::default() }
    }

    /// Set proof nodes on storage proof.
    pub fn with_proof(mut self, proof: Vec<Bytes>) -> Self {
        self.proof = proof;
        self
    }

    /// Verify the proof against the provided storage root.
    pub fn verify(&self, root: B256) -> Result<(), ProofVerificationError> {
        let expected =
            if self.value.is_zero() { None } else { Some(encode_fixed_size(&self.value).to_vec()) };
        verify_proof(root, self.nibbles.clone(), expected, &self.proof)
    }
}

/// The merkle proof of the storage entry, using decoded proofs.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct DecodedStorageProof {
    /// The raw storage key.
    pub key: B256,
    /// The hashed storage key nibbles.
    pub nibbles: Nibbles,
    /// The storage value.
    pub value: U256,
    /// Array of merkle trie nodes which starting from the storage root node and following the path
    /// of the hashed storage slot as key.
    pub proof: Vec<TrieNode>,
}

impl DecodedStorageProof {
    /// Create new storage proof from the storage slot.
    pub fn new(key: B256) -> Self {
        let nibbles = Nibbles::unpack(keccak256(key));
        Self { key, nibbles, ..Default::default() }
    }

    /// Create new storage proof from the storage slot and its pre-hashed image.
    pub fn new_with_hashed(key: B256, hashed_key: B256) -> Self {
        Self { key, nibbles: Nibbles::unpack(hashed_key), ..Default::default() }
    }

    /// Create new storage proof from the storage slot and its pre-hashed image.
    pub fn new_with_nibbles(key: B256, nibbles: Nibbles) -> Self {
        Self { key, nibbles, ..Default::default() }
    }

    /// Set proof nodes on storage proof.
    pub fn with_proof(mut self, proof: Vec<TrieNode>) -> Self {
        self.proof = proof;
        self
    }
}

/// Implementation of hasher using our keccak256 hashing function
/// for compatibility with `triehash` crate.
#[cfg(any(test, feature = "test-utils"))]
pub mod triehash {
    use alloy_primitives::{keccak256, B256};
    use alloy_rlp::RlpEncodable;
    use hash_db::Hasher;
    use plain_hasher::PlainHasher;

    /// A [Hasher] that calculates a keccak256 hash of the given data.
    #[derive(Default, Debug, Clone, PartialEq, Eq, RlpEncodable)]
    #[non_exhaustive]
    pub struct KeccakHasher;

    #[cfg(any(test, feature = "test-utils"))]
    impl Hasher for KeccakHasher {
        type Out = B256;
        type StdHasher = PlainHasher;

        const LENGTH: usize = 32;

        fn hash(x: &[u8]) -> Self::Out {
            keccak256(x)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multiproof_extend_account_proofs() {
        let mut proof1 = MultiProof::default();
        let mut proof2 = MultiProof::default();

        let addr1 = B256::random();
        let addr2 = B256::random();

        proof1.account_subtree.insert(
            Nibbles::unpack(addr1),
            alloy_rlp::encode_fixed_size(&U256::from(42)).to_vec().into(),
        );
        proof2.account_subtree.insert(
            Nibbles::unpack(addr2),
            alloy_rlp::encode_fixed_size(&U256::from(43)).to_vec().into(),
        );

        proof1.extend(proof2);

        assert!(proof1.account_subtree.contains_key(&Nibbles::unpack(addr1)));
        assert!(proof1.account_subtree.contains_key(&Nibbles::unpack(addr2)));
    }

    #[test]
    fn test_multiproof_extend_storage_proofs() {
        let mut proof1 = MultiProof::default();
        let mut proof2 = MultiProof::default();

        let addr = B256::random();
        let root = B256::random();

        let mut subtree1 = ProofNodes::default();
        subtree1.insert(
            Nibbles::from_nibbles(vec![0]),
            alloy_rlp::encode_fixed_size(&U256::from(42)).to_vec().into(),
        );
        proof1.storages.insert(
            addr,
            StorageMultiProof {
                root,
                subtree: subtree1,
                branch_node_hash_masks: HashMap::default(),
                branch_node_tree_masks: HashMap::default(),
            },
        );

        let mut subtree2 = ProofNodes::default();
        subtree2.insert(
            Nibbles::from_nibbles(vec![1]),
            alloy_rlp::encode_fixed_size(&U256::from(43)).to_vec().into(),
        );
        proof2.storages.insert(
            addr,
            StorageMultiProof {
                root,
                subtree: subtree2,
                branch_node_hash_masks: HashMap::default(),
                branch_node_tree_masks: HashMap::default(),
            },
        );

        proof1.extend(proof2);

        let storage = proof1.storages.get(&addr).unwrap();
        assert_eq!(storage.root, root);
        assert!(storage.subtree.contains_key(&Nibbles::from_nibbles(vec![0])));
        assert!(storage.subtree.contains_key(&Nibbles::from_nibbles(vec![1])));
    }
}

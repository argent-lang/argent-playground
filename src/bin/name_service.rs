use std::collections::{BTreeMap, BTreeSet};

use argent::build_file;
use argent_playground::{PlaygroundResult, demo_keypair, demo_outpoint, sign_input};
use argent_runtime::{CovenantOutput, EntryCall, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::{
    Hash,
    tx::{CovenantBinding, ScriptPublicKey, UtxoEntry},
};

const SOURCE: &str = "ag/name_service/name_service.ag";
const NAME_BYTES: usize = 63;
const SMT_DEPTH: usize = 128;
const SMT_PROOF_BYTES: usize = SMT_DEPTH * 32;
const ZERO: [u8; 32] = [0; 32];
const NAME_KEY_DOMAIN: [u8; 32] = key32(b"NameKey");
const SMT_LEAF_DOMAIN: [u8; 32] = key32(b"SmtLeaf");
const SMT_NODE_DOMAIN: [u8; 32] = key32(b"SmtNode");
const REGISTRY_VALUE: u64 = 10_000;
const NAME_VALUE: u64 = 2_000;

const fn key32(key: &[u8]) -> [u8; 32] {
    assert!(key.len() <= 32);
    let mut padded = [0; 32];
    let mut index = 0;
    while index < key.len() {
        padded[index] = key[index];
        index += 1;
    }
    padded
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CanonicalName {
    padded: [u8; NAME_BYTES],
    len: usize,
}

impl CanonicalName {
    fn parse(label: &str) -> Result<Self, &'static str> {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > NAME_BYTES {
            return Err("a name must contain between 1 and 63 bytes");
        }
        if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
            return Err("a name cannot start or end with a hyphen");
        }
        if !bytes.iter().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-') {
            return Err("a name may only contain lowercase ASCII letters, digits, and hyphens");
        }

        let mut padded = [0; NAME_BYTES];
        padded[..bytes.len()].copy_from_slice(bytes);
        Ok(Self { padded, len: bytes.len() })
    }

    fn key(&self, registry_id: Hash) -> [u8; 32] {
        let mut preimage = Vec::with_capacity(32 + 8 + self.len);
        preimage.extend_from_slice(&registry_id.as_bytes());
        preimage.extend_from_slice(&(self.len as i64).to_le_bytes());
        preimage.extend_from_slice(&self.padded[..self.len]);
        *blake3::keyed_hash(&NAME_KEY_DOMAIN, &preimage).as_bytes()
    }
}

#[derive(Default)]
struct NameTree {
    leaves: BTreeMap<[u8; 32], [u8; 32]>,
}

impl NameTree {
    fn root(&self) -> [u8; 32] {
        let empty_hashes = Self::empty_hashes();
        self.levels(&empty_hashes)[0].get(&ZERO).copied().unwrap_or(empty_hashes[0])
    }

    fn proof_payload(&self, key: [u8; 32]) -> Vec<u8> {
        let empty_hashes = Self::empty_hashes();
        let levels = self.levels(&empty_hashes);
        let path = Self::tree_path(key);
        let mut payload = Vec::with_capacity(SMT_PROOF_BYTES);

        for depth in (0..SMT_DEPTH).rev() {
            let mut sibling_prefix = Self::prefix(path, depth + 1);
            let (path_byte, path_bit) = Self::path_bit_location(depth);
            sibling_prefix[path_byte] ^= path_bit;
            let sibling = levels[depth + 1].get(&sibling_prefix).copied().unwrap_or(empty_hashes[depth + 1]);
            payload.extend_from_slice(&sibling);
        }

        payload
    }

    fn verified_insert_root(old_root: [u8; 32], key: [u8; 32], payload: &[u8]) -> Option<[u8; 32]> {
        if payload.len() != SMT_PROOF_BYTES {
            return None;
        }

        let mut old_current = ZERO;
        let mut new_current = Self::leaf_hash(key);
        let path = Self::tree_path(key);
        for (proof_index, depth) in (0..SMT_DEPTH).rev().enumerate() {
            let sibling = payload[proof_index * 32..(proof_index + 1) * 32].try_into().ok()?;

            if Self::key_goes_right(path, depth) {
                old_current = Self::node_hash(sibling, old_current);
                new_current = Self::node_hash(sibling, new_current);
            } else {
                old_current = Self::node_hash(old_current, sibling);
                new_current = Self::node_hash(new_current, sibling);
            }
        }

        (old_current == old_root).then_some(new_current)
    }

    fn insert(&mut self, key: [u8; 32]) -> bool {
        self.leaves.insert(Self::prefix(Self::tree_path(key), SMT_DEPTH), key).is_none()
    }

    fn levels(&self, empty_hashes: &[[u8; 32]]) -> Vec<BTreeMap<[u8; 32], [u8; 32]>> {
        let mut levels = vec![BTreeMap::new(); SMT_DEPTH + 1];
        for (path, key) in &self.leaves {
            levels[SMT_DEPTH].insert(*path, Self::leaf_hash(*key));
        }

        for depth in (0..SMT_DEPTH).rev() {
            let parents = levels[depth + 1].keys().map(|key| Self::prefix(*key, depth)).collect::<BTreeSet<_>>();
            let mut parent_level = BTreeMap::new();
            for parent in parents {
                let left = levels[depth + 1].get(&parent).copied().unwrap_or(empty_hashes[depth + 1]);
                let mut right_prefix = parent;
                let (path_byte, path_bit) = Self::path_bit_location(depth);
                right_prefix[path_byte] |= path_bit;
                let right = levels[depth + 1].get(&right_prefix).copied().unwrap_or(empty_hashes[depth + 1]);
                let node = Self::node_hash(left, right);
                if node != empty_hashes[depth] {
                    parent_level.insert(parent, node);
                }
            }
            levels[depth] = parent_level;
        }
        levels
    }

    fn empty_hashes() -> Vec<[u8; 32]> {
        let mut hashes = vec![ZERO; SMT_DEPTH + 1];
        for depth in (0..SMT_DEPTH).rev() {
            hashes[depth] = Self::node_hash(hashes[depth + 1], hashes[depth + 1]);
        }
        hashes
    }

    // covenant folds the proof leaf-to-root while consuming key bytes in
    // ascending order, so the tree path reverses the key's first 16 bytes.
    fn tree_path(mut key: [u8; 32]) -> [u8; 32] {
        key[..SMT_DEPTH / 8].reverse();
        key
    }

    fn prefix(mut key: [u8; 32], depth: usize) -> [u8; 32] {
        let whole_bytes = depth / 8;
        let retained_bits = depth % 8;
        if retained_bits == 0 {
            key[whole_bytes..].fill(0);
        } else {
            let retained_prefix_mask = u8::MAX << (8 - retained_bits);
            key[whole_bytes] &= retained_prefix_mask;
            key[whole_bytes + 1..].fill(0);
        }
        key
    }

    fn path_bit_location(depth: usize) -> (usize, u8) {
        (depth / 8, 1 << (7 - depth % 8))
    }

    fn key_goes_right(key: [u8; 32], depth: usize) -> bool {
        let (path_byte, path_bit) = Self::path_bit_location(depth);
        key[path_byte] & path_bit != 0
    }

    fn leaf_hash(key: [u8; 32]) -> [u8; 32] {
        *blake3::keyed_hash(&SMT_LEAF_DOMAIN, &key).as_bytes()
    }

    fn node_hash(left: [u8; 32], right: [u8; 32]) -> [u8; 32] {
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(&left);
        preimage[32..].copy_from_slice(&right);
        *blake3::keyed_hash(&SMT_NODE_DOMAIN, &preimage).as_bytes()
    }
}

fn main() -> PlaygroundResult<()> {
    let artifact = build_file(SOURCE, "build/name_service")?;
    let builder = TxBuilder::new(&artifact)?;
    let alice = demo_keypair(0x21);
    let bob = demo_keypair(0x22);
    let alice_owner = alice.x_only_public_key().0.serialize().to_vec();
    let bob_owner = bob.x_only_public_key().0.serialize().to_vec();
    let mut tree = NameTree::default();

    // Initiate the shared registry (the genesis)
    let registry_initial = state! { root: tree.root().to_vec() };
    let registry_funding = UtxoEntry::new(REGISTRY_VALUE, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let registry_genesis_context = TxContext::new()
        .input(demo_outpoint(0x90, 0), registry_funding, Vec::new(), 0)
        .actor_genesis_output(0, "launch::registry", "Registry", registry_initial.clone(), REGISTRY_VALUE);
    let registry_genesis_tx = builder.build(&registry_genesis_context)?;
    let registry_empty = CovenantOutput::from_tx(&registry_genesis_tx, 0)?;

    // Proofs contain all 128 sibling hashes in leaf-to-root order
    // Mint alice from fresh root
    let alice_name = CanonicalName::parse("alice")?;
    let alice_key = alice_name.key(registry_empty.covenant_id);
    let alice_proof = tree.proof_payload(alice_key);
    let alice_proof_len = alice_proof.len();
    let alice_root =
        NameTree::verified_insert_root(tree.root(), alice_key, &alice_proof).ok_or("the locally generated alice proof must verify")?;
    if !tree.insert(alice_key) || tree.root() != alice_root {
        return Err("alice insertion did not produce the verified root".into());
    }

    let registry_alice = state! { root: alice_root.to_vec() };
    let alice_name_state = state! {
        name_key: alice_key.to_vec(),
        label: alice_name.padded.to_vec(),
        label_len: alice_name.len as i64,
        owner: alice_owner.clone(),
    };
    let alice_funding = UtxoEntry::new(NAME_VALUE, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let mint_alice_context = TxContext::new()
        .actor_input(
            "Registry",
            registry_initial,
            EntryCall::new("register").args_with(|tx, input_idx| {
                args![alice_name.padded.to_vec(), alice_name.len as i64, alice_owner.clone(), sign_input(tx, input_idx, &alice)]
            }),
            registry_empty.outpoint,
            registry_empty.utxo,
            0,
        )
        .input(demo_outpoint(0x91, 0), alice_funding, Vec::new(), 0)
        .actor_output("Registry", registry_alice.clone(), CovenantBinding::new(0, registry_empty.covenant_id), REGISTRY_VALUE)
        .actor_output("Name", alice_name_state.clone(), CovenantBinding::new(0, registry_empty.covenant_id), NAME_VALUE)
        .payload(alice_proof);
    let mint_alice_tx = builder.build(&mint_alice_context)?;
    let registry_once = CovenantOutput::from_tx(&mint_alice_tx, 0)?;
    let alice_token = CovenantOutput::from_tx(&mint_alice_tx, 1)?;

    // Mint bob from the updated tree.
    let bob_name = CanonicalName::parse("bob")?;
    let bob_key = bob_name.key(registry_once.covenant_id);
    let bob_proof = tree.proof_payload(bob_key);
    let bob_proof_len = bob_proof.len();
    let bob_root =
        NameTree::verified_insert_root(tree.root(), bob_key, &bob_proof).ok_or("the locally generated bob proof must verify")?;
    if !tree.insert(bob_key) || tree.root() != bob_root {
        return Err("bob insertion did not produce the verified root".into());
    }

    let registry_bob = state! { root: bob_root.to_vec() };
    let bob_name_state = state! {
        name_key: bob_key.to_vec(),
        label: bob_name.padded.to_vec(),
        label_len: bob_name.len as i64,
        owner: bob_owner.clone(),
    };
    let bob_funding = UtxoEntry::new(NAME_VALUE, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let mint_bob_context = TxContext::new()
        .actor_input(
            "Registry",
            registry_alice,
            EntryCall::new("register").args_with(|tx, input_idx| {
                args![bob_name.padded.to_vec(), bob_name.len as i64, bob_owner.clone(), sign_input(tx, input_idx, &bob)]
            }),
            registry_once.outpoint,
            registry_once.utxo,
            0,
        )
        .input(demo_outpoint(0x92, 0), bob_funding, Vec::new(), 0)
        .actor_output("Registry", registry_bob.clone(), CovenantBinding::new(0, registry_once.covenant_id), REGISTRY_VALUE)
        .actor_output("Name", bob_name_state, CovenantBinding::new(0, registry_once.covenant_id), NAME_VALUE)
        .payload(bob_proof);
    let mint_bob_tx = builder.build(&mint_bob_context)?;
    let registry_twice = CovenantOutput::from_tx(&mint_bob_tx, 0)?;

    // Reusing alice's membership path as a non-membership proof must fail in
    // the actual Kaspa script engine, proving duplicate mint rejection.
    let duplicate_proof = tree.proof_payload(alice_key);
    if NameTree::verified_insert_root(tree.root(), alice_key, &duplicate_proof).is_some() {
        return Err("duplicate alice unexpectedly passed the local proof verifier".into());
    }
    let duplicate_funding = UtxoEntry::new(NAME_VALUE, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let duplicate_context = TxContext::new()
        .actor_input(
            "Registry",
            registry_bob,
            EntryCall::new("register").args_with(|tx, input_idx| {
                args![alice_name.padded.to_vec(), alice_name.len as i64, alice_owner.clone(), sign_input(tx, input_idx, &alice)]
            }),
            registry_twice.outpoint,
            registry_twice.utxo,
            0,
        )
        .input(demo_outpoint(0x93, 0), duplicate_funding, Vec::new(), 0)
        .actor_output(
            "Registry",
            state! { root: bob_root.to_vec() },
            CovenantBinding::new(0, registry_twice.covenant_id),
            REGISTRY_VALUE,
        )
        .actor_output("Name", alice_name_state.clone(), CovenantBinding::new(0, registry_twice.covenant_id), NAME_VALUE)
        .payload(duplicate_proof);
    let duplicate_error = match builder.build(&duplicate_context) {
        Ok(_) => return Err("duplicate alice unexpectedly passed covenant execution".into()),
        Err(error) => error,
    };

    // Transfer alice's Name output
    let alice_transferred = state! {
        name_key: alice_key.to_vec(),
        label: alice_name.padded.to_vec(),
        label_len: alice_name.len as i64,
        owner: bob_owner.clone(),
    };
    let transfer_context = TxContext::new()
        .actor_input(
            "Name",
            alice_name_state,
            EntryCall::new("transfer").args_with(|tx, input_idx| args![bob_owner.clone(), sign_input(tx, input_idx, &alice)]),
            alice_token.outpoint,
            alice_token.utxo,
            0,
        )
        .actor_output("Name", alice_transferred, CovenantBinding::new(0, alice_token.covenant_id), NAME_VALUE);
    let transfer_tx = builder.build(&transfer_context)?;

    if registry_twice.covenant_id != alice_token.covenant_id {
        return Err("registry and name outputs must share one covenant family".into());
    }

    println!("deployed Registry {}", registry_twice.covenant_id);
    println!("minted alice ({alice_proof_len}-byte proof) and bob ({bob_proof_len}-byte proof)");
    println!("duplicate alice rejected: {duplicate_error}");
    println!("transferred alice to bob without a Registry input");
    println!("transfer tx: {} ({} input, {} output)", transfer_tx.id(), transfer_tx.inputs.len(), transfer_tx.outputs.len());
    println!("artifact: build/name_service/artifact.json");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_names_reject_ambiguous_labels() {
        assert!(CanonicalName::parse("alice-7").is_ok());
        for invalid in ["", "Alice", "-alice", "alice-", "alice.eth", "alice\u{e9}"] {
            assert!(CanonicalName::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn domain_keys_are_zero_padded_ascii() {
        for (domain, key) in
            [(&NAME_KEY_DOMAIN[..], &b"NameKey"[..]), (&SMT_LEAF_DOMAIN[..], &b"SmtLeaf"[..]), (&SMT_NODE_DOMAIN[..], &b"SmtNode"[..])]
        {
            assert_eq!(&domain[..key.len()], key);
            assert!(domain[key.len()..].iter().all(|byte| *byte == 0));
        }
    }
}

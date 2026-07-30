/** WARNING: Do not treat this implementation as production ready */
mod smt;

use argent::build_file;
use argent_playground::{PlaygroundResult, demo_keypair, demo_outpoint, sign_input};
use argent_runtime::{CovenantOutput, EntryCall, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::{
    Hash,
    tx::{CovenantBinding, ScriptPublicKey, UtxoEntry},
};
use smt::CompactSparseMerkleSet;

const SOURCE: &str = "ag/name_service/name_service.ag";
const NAME_BYTES: usize = 63;
const STORE_VALUE: u64 = 10_000;
const REGISTRY_VALUE: u64 = 10_000;
const NAME_VALUE: u64 = 2_000;
const SMT_LEAF_DOMAIN: [u8; 32] =
    [0x53, 0x6d, 0x74, 0x4c, 0x65, 0x61, 0x66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

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
        *blake3::keyed_hash(&SMT_LEAF_DOMAIN, &preimage).as_bytes()
    }
}

fn main() -> PlaygroundResult<()> {
    let artifact = build_file(SOURCE, "build/name_service")?;
    let builder = TxBuilder::new(&artifact)?;
    let alice = demo_keypair(0x21);
    let bob = demo_keypair(0x22);
    let alice_owner = alice.x_only_public_key().0.serialize().to_vec();
    let bob_owner = bob.x_only_public_key().0.serialize().to_vec();
    let mut tree = CompactSparseMerkleSet::new();

    // Launch the registry
    let store_initial = state! { root: tree.root_hash().to_vec() };
    let fast_initial = state! {};
    let registry_funding = UtxoEntry::new(STORE_VALUE + REGISTRY_VALUE, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let registry_genesis_context = TxContext::new()
        .input(demo_outpoint(0x90, 0), registry_funding, Vec::new(), 0)
        .actor_genesis_output(0, "launch::registry", "RegistryStore", store_initial.clone(), STORE_VALUE)
        .actor_genesis_output(0, "launch::registry", "RegistryFast", fast_initial.clone(), REGISTRY_VALUE);
    let registry_genesis_tx = builder.build(&registry_genesis_context)?;
    let store_empty = CovenantOutput::from_tx(&registry_genesis_tx, 0)?;
    let fast_empty = CovenantOutput::from_tx(&registry_genesis_tx, 1)?;

    // Mint alice
    let alice_name = CanonicalName::parse("alice")?;
    let alice_key = alice_name.key(store_empty.covenant_id);
    let alice_proof = tree.prove(&alice_key).encode();
    tree.insert(alice_key, alice_name.padded[..alice_name.len].to_vec());
    let store_alice = state! { root: tree.root_hash().to_vec() };
    let alice_name_state = state! {
        name_key: alice_key.to_vec(),
        label: alice_name.padded.to_vec(),
        owner: alice_owner.clone(),
    };
    let alice_funding = UtxoEntry::new(NAME_VALUE, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let mint_alice_context = TxContext::new()
        .actor_input(
            "RegistryFast",
            fast_initial,
            EntryCall::new("register").args_with(|tx, input_idx| {
                args![
                    alice_name.padded[..alice_name.len].to_vec(),
                    alice_proof.clone(),
                    alice_name.padded[..alice_name.len].to_vec(),
                    alice_owner.clone(),
                    sign_input(tx, input_idx, &alice)
                ]
            }),
            fast_empty.outpoint,
            fast_empty.utxo,
            0,
        )
        .actor_input("RegistryStore", store_initial, "update_fast", store_empty.outpoint, store_empty.utxo, 0)
        .input(demo_outpoint(0x91, 0), alice_funding, Vec::new(), 0)
        .actor_output("RegistryFast", state! {}, CovenantBinding::new(0, fast_empty.covenant_id), REGISTRY_VALUE)
        .actor_output("RegistryStore", store_alice.clone(), CovenantBinding::new(0, fast_empty.covenant_id), STORE_VALUE)
        .actor_output("Name", alice_name_state.clone(), CovenantBinding::new(0, fast_empty.covenant_id), NAME_VALUE);
    let mint_alice_tx = builder.build(&mint_alice_context)?;
    let fast_alice = CovenantOutput::from_tx(&mint_alice_tx, 0)?;
    let store_alice_output = CovenantOutput::from_tx(&mint_alice_tx, 1)?;
    let alice_token = CovenantOutput::from_tx(&mint_alice_tx, 2)?;

    // Mint bob using the existing leaf as the non-membership witness
    let bob_name = CanonicalName::parse("bob")?;
    let bob_key = bob_name.key(fast_alice.covenant_id);
    let bob_compact_proof = tree.prove(&bob_key);
    let (_bob_witness_key, bob_witness_label) = bob_compact_proof.witness().ok_or("bob's non-empty tree proof must have a witness")?;

    let bob_witness_label = bob_witness_label.to_vec();
    let bob_proof = bob_compact_proof.encode();
    tree.insert(bob_key, bob_name.padded[..bob_name.len].to_vec());
    let store_bob = state! { root: tree.root_hash().to_vec() };
    let bob_name_state = state! {
        name_key: bob_key.to_vec(),
        label: bob_name.padded.to_vec(),
        owner: bob_owner.clone(),
    };
    let bob_funding = UtxoEntry::new(NAME_VALUE, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let mint_bob_context = TxContext::new()
        .actor_input(
            "RegistryFast",
            state! {},
            EntryCall::new("register").args_with(|tx, input_idx| {
                args![
                    bob_name.padded[..bob_name.len].to_vec(),
                    bob_proof.clone(),
                    bob_witness_label.clone(),
                    bob_owner.clone(),
                    sign_input(tx, input_idx, &bob)
                ]
            }),
            fast_alice.outpoint,
            fast_alice.utxo,
            0,
        )
        .actor_input("RegistryStore", store_alice, "update_fast", store_alice_output.outpoint, store_alice_output.utxo, 0)
        .input(demo_outpoint(0x92, 0), bob_funding, Vec::new(), 0)
        .actor_output("RegistryFast", state! {}, CovenantBinding::new(0, fast_alice.covenant_id), REGISTRY_VALUE)
        .actor_output("RegistryStore", store_bob, CovenantBinding::new(0, fast_alice.covenant_id), STORE_VALUE)
        .actor_output("Name", bob_name_state, CovenantBinding::new(0, fast_alice.covenant_id), NAME_VALUE);
    let mint_bob_tx = builder.build(&mint_bob_context)?;

    // Transfer alice to Bob
    let alice_transferred = state! {
        name_key: alice_key.to_vec(),
        label: alice_name.padded.to_vec(),
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

    println!("minted alice: {}", mint_alice_tx.id());
    println!("minted bob with alice as witness: {}", mint_bob_tx.id());
    println!("transfer tx: {} ({} input, {} output)", transfer_tx.id(), transfer_tx.inputs.len(), transfer_tx.outputs.len());
    println!("artifact: build/name_service/artifact.json");
    Ok(())
}

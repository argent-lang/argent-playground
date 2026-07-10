use std::collections::BTreeMap;
use std::path::Path;

use argent_runtime::{ArtifactValue, ObservedCovenantContext, state};
use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::Hash;
use kaspa_consensus_core::hashing::sighash::{SigHashReusedValuesUnsync, calc_schnorr_signature_hash};
use kaspa_consensus_core::hashing::sighash_type::SIG_HASH_ALL;
use kaspa_consensus_core::tx::{
    MutableTransaction, Transaction, TransactionId, TransactionInput, TransactionOutpoint, UtxoEntry,
};
use kaspa_txscript::script_builder::ScriptBuilder;
use secp256k1::{Keypair, Secp256k1, SecretKey};

pub type PlaygroundResult<T> = Result<T, Box<dyn std::error::Error>>;

pub fn demo_outpoint(byte: u8, index: u32) -> TransactionOutpoint {
    TransactionOutpoint { transaction_id: TransactionId::from_bytes([byte; 32]), index }
}

// ---- tn10 helpers ----------------------------------------------------------

/// Borsh wRPC endpoint of a covenant-enabled tn10 node.
/// Override with KASPA_NODE_URL, e.g. `KASPA_NODE_URL=ws://myhost:17210`.
pub fn node_url() -> String {
    std::env::var("KASPA_NODE_URL").unwrap_or_else(|_| "ws://127.0.0.1:17210".to_string())
}
pub const EXPLORER: &str = "https://tn10.kaspa.stream/transactions";

// Toccata v1 inputs commit a compute budget (1 unit = 10,000 script units).
// A schnorr sig verification costs 100,000 units => budget 10; covenant
// P2SH scripts carry no sigops, so a modest budget covers their opcodes.
pub const P2PK_BUDGET: u16 = 12;
pub const COVENANT_BUDGET: u16 = 400;

pub fn input_with_budget(outpoint: TransactionOutpoint, sigscript: Vec<u8>, budget: u16) -> TransactionInput {
    TransactionInput::new_with_compute_budget(outpoint, sigscript, 0, budget)
}

pub fn load_or_create_keypair(key_file: &str) -> PlaygroundResult<Keypair> {
    let secp = Secp256k1::new();
    if Path::new(key_file).exists() {
        let hex = std::fs::read_to_string(key_file)?;
        let bytes: Vec<u8> = (0..hex.trim().len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex.trim()[i..i + 2], 16))
            .collect::<Result<_, _>>()?;
        let secret = SecretKey::from_slice(&bytes)?;
        Ok(Keypair::from_secret_key(&secp, &secret))
    } else {
        let (secret, _) = secp.generate_keypair(&mut secp256k1::rand::thread_rng());
        let hex: String = secret.secret_bytes().iter().map(|b| format!("{b:02x}")).collect();
        std::fs::write(key_file, &hex)?;
        println!("generated new key, saved to {key_file} (gitignored — do not commit)");
        Ok(Keypair::from_secret_key(&secp, &secret))
    }
}

pub fn funding_address(keypair: &Keypair) -> Address {
    let (xonly, _) = keypair.x_only_public_key();
    Address::new(Prefix::Testnet, Version::PubKey, &xonly.serialize())
}

pub fn sign_p2pk_input(
    tx: &Transaction,
    entries: &[UtxoEntry],
    input_idx: usize,
    keypair: &Keypair,
) -> PlaygroundResult<Vec<u8>> {
    let mutable = MutableTransaction::with_entries(tx.clone(), entries.to_vec());
    let reused = SigHashReusedValuesUnsync::new();
    let sig_hash = calc_schnorr_signature_hash(&mutable.as_verifiable(), input_idx, SIG_HASH_ALL, &reused);
    let msg = secp256k1::Message::from_digest_slice(sig_hash.as_bytes().as_slice())?;
    let sig = keypair.sign_schnorr(msg);
    let mut signature = sig.as_ref().to_vec();
    signature.push(SIG_HASH_ALL.to_u8());
    Ok(ScriptBuilder::new().add_data(&signature)?.drain())
}

pub fn hex_bytes(hex: &str) -> PlaygroundResult<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return Err("odd-length hex".into());
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(Into::into)).collect()
}

// ---- Open Lattice state helpers --------------------------------------------

pub const WORLD_ID: [u8; 32] = [0x11; 32];
pub const CAPS_DIGEST: [u8; 32] = [0xAA; 32];
pub const EMPTY_SENTINEL: [u8; 32] = [0x00; 32];

pub fn lattice_capsule(
    agent_id: u8,
    controller_id: Hash,
    x: i64,
    y: i64,
    energy: i64,
) -> BTreeMap<String, ArtifactValue> {
    state! {
        world_id: WORLD_ID,
        agent_id: [agent_id; 32],
        species_id: [0x02; 32],
        controller_id: controller_id,
        capabilities_digest: CAPS_DIGEST,
        strategy: [0x44; 32],
        x: x,
        y: y,
        energy: energy,
        generation: 1,
    }
}

pub fn lattice_cell(
    x: i64,
    y: i64,
    food: i64,
    occupant_covid: Hash,
    occupant_type: Vec<u8>,
    occupant_caps: [u8; 32],
) -> BTreeMap<String, ArtifactValue> {
    state! {
        world_id: WORLD_ID,
        x: x,
        y: y,
        food: food,
        occupant_agent_covid: occupant_covid,
        occupant_agent_type: occupant_type,
        occupant_caps_digest: occupant_caps,
    }
}

// ---- Persistent world state (continue a live tn10 world across sessions) ----

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CellView {
    pub x: i64,
    pub y: i64,
    pub food: i64,
    pub occupant: Option<usize>,
    pub outpoint: TransactionOutpoint,
    pub value: u64,
    pub spk: kaspa_consensus_core::tx::ScriptPublicKey,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct AgentView {
    pub covid: Hash,
    pub id: u8,
    pub x: i64,
    pub y: i64,
    pub energy: i64,
    pub outpoint: TransactionOutpoint,
    pub value: u64,
    pub spk: kaspa_consensus_core::tx::ScriptPublicKey,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct WorldState {
    pub world_covid: Hash,
    pub size: i64,
    pub cells: Vec<CellView>,
    pub agents: Vec<AgentView>,
}

impl WorldState {
    pub fn save(&self, path: &str) -> PlaygroundResult<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load(path: &str) -> PlaygroundResult<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }
}

pub fn lattice_observe_ctx(
    observe: &str,
    utxo: UtxoEntry,
    input_state: BTreeMap<String, ArtifactValue>,
    output_state: BTreeMap<String, ArtifactValue>,
) -> BTreeMap<String, ObservedCovenantContext> {
    BTreeMap::from([(
        observe.to_string(),
        ObservedCovenantContext::from_app("open_lattice_agent")
            .input("agent", "Agent", utxo, input_state)
            .output("agent", "Agent", output_state),
    )])
}

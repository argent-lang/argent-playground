// Open Lattice Season 0 — live on testnet-10.
//
// Spends a faucet UTXO into real covenant genesis transactions, then submits
// the attach and harvest game actions as chained transactions. The physics
// executed here are identical to the local `open_lattice` demo; this binary
// only adds funding, signing, fees, and RPC submission.
//
// Usage: fund the printed address from the tn10 faucet, then re-run.

use std::collections::BTreeMap;
use std::path::Path;

use argent::build_file;
use argent_playground::PlaygroundResult;
use argent_runtime::{ArtifactBundle, ObservedCovenantContext, TxBuilder, args, execute_input_with_covenants, state};
use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::Hash;
use kaspa_consensus_core::hashing::sighash::{SigHashReusedValuesUnsync, calc_schnorr_signature_hash};
use kaspa_consensus_core::hashing::sighash_type::SIG_HASH_ALL;
use kaspa_consensus_core::tx::{
    GenesisCovenantGroup, MutableTransaction, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput, UtxoEntry,
};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::pay_to_address_script;
use kaspa_txscript::script_builder::ScriptBuilder;
use kaspa_wrpc_client::{KaspaRpcClient, WrpcEncoding, prelude::ConnectOptions};
use secp256k1::{Keypair, Secp256k1, SecretKey};

const NODE_URL: &str = "ws://10.0.3.26:17210";
const KEY_FILE: &str = ".tn10-key";
const EXPLORER: &str = "https://explorer-tn10.kaspa.org/txs";

const CORE_SOURCE: &str = "ag/open_lattice/core.ag";
const AGENT_SOURCE: &str = "ag/open_lattice/agent.ag";

const WORLD_ID: [u8; 32] = [0x11; 32];
const CAPS_DIGEST: [u8; 32] = [0xAA; 32];
const EMPTY_SENTINEL: [u8; 32] = [0x00; 32];

// KIP-9 storage mass punishes small outputs; keep every output >= 1 TKAS.
const COVENANT_VALUE: u64 = 100_000_000; // 1 TKAS
const FEE: u64 = 15_000_000; // 0.15 TKAS — compute mass is priced at 100 sompi/gram
const MIN_FUNDING: u64 = 2 * COVENANT_VALUE + 2 * COVENANT_VALUE + 4 * FEE;

// Toccata v1 inputs commit a compute budget (1 unit = 10,000 script units).
// A schnorr sig verification costs 100,000 units => budget 10; covenant
// P2SH scripts carry no sigops, so a modest budget covers their opcodes.
const P2PK_BUDGET: u16 = 12;
const COVENANT_BUDGET: u16 = 400;

fn input_with_budget(outpoint: TransactionOutpoint, sigscript: Vec<u8>, budget: u16) -> TransactionInput {
    TransactionInput::new_with_compute_budget(outpoint, sigscript, 0, budget)
}

fn load_or_create_keypair() -> PlaygroundResult<Keypair> {
    let secp = Secp256k1::new();
    if Path::new(KEY_FILE).exists() {
        let hex = std::fs::read_to_string(KEY_FILE)?;
        let bytes: Vec<u8> = (0..hex.trim().len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex.trim()[i..i + 2], 16))
            .collect::<Result<_, _>>()?;
        let secret = SecretKey::from_slice(&bytes)?;
        Ok(Keypair::from_secret_key(&secp, &secret))
    } else {
        let (secret, _) = secp.generate_keypair(&mut secp256k1::rand::thread_rng());
        let hex: String = secret.secret_bytes().iter().map(|b| format!("{b:02x}")).collect();
        std::fs::write(KEY_FILE, &hex)?;
        println!("generated new key, saved to {KEY_FILE} (gitignored — do not commit)");
        Ok(Keypair::from_secret_key(&secp, &secret))
    }
}

fn funding_address(keypair: &Keypair) -> Address {
    let (xonly, _) = keypair.x_only_public_key();
    Address::new(Prefix::Testnet, Version::PubKey, &xonly.serialize())
}

fn sign_p2pk_input(tx: &Transaction, entries: &[UtxoEntry], input_idx: usize, keypair: &Keypair) -> PlaygroundResult<Vec<u8>> {
    let mutable = MutableTransaction::with_entries(tx.clone(), entries.to_vec());
    let reused = SigHashReusedValuesUnsync::new();
    let sig_hash = calc_schnorr_signature_hash(&mutable.as_verifiable(), input_idx, SIG_HASH_ALL, &reused);
    let msg = secp256k1::Message::from_digest_slice(sig_hash.as_bytes().as_slice())?;
    let sig = keypair.sign_schnorr(msg);
    let mut signature = sig.as_ref().to_vec();
    signature.push(SIG_HASH_ALL.to_u8());
    Ok(ScriptBuilder::new().add_data(&signature)?.drain())
}

fn capsule(
    agent_id: u8,
    controller_id: Hash,
    x: i64,
    y: i64,
    energy: i64,
) -> BTreeMap<String, argent_runtime::ArtifactValue> {
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

fn cell_state(
    food: i64,
    occupant_covid: Hash,
    occupant_type: Vec<u8>,
    occupant_caps: [u8; 32],
) -> BTreeMap<String, argent_runtime::ArtifactValue> {
    state! {
        world_id: WORLD_ID,
        x: 0,
        y: 0,
        food: food,
        occupant_agent_covid: occupant_covid,
        occupant_agent_type: occupant_type,
        occupant_caps_digest: occupant_caps,
    }
}

fn observe_ctx(
    observe: &str,
    utxo: UtxoEntry,
    input_state: BTreeMap<String, argent_runtime::ArtifactValue>,
    output_state: BTreeMap<String, argent_runtime::ArtifactValue>,
) -> BTreeMap<String, ObservedCovenantContext> {
    BTreeMap::from([(
        observe.to_string(),
        ObservedCovenantContext::from_app("open_lattice_agent")
            .input("agent", "Agent", utxo, input_state)
            .output("agent", "Agent", output_state),
    )])
}

async fn submit(client: &KaspaRpcClient, label: &str, tx: &Transaction) -> PlaygroundResult<()> {
    let txid = client.submit_transaction(tx.into(), false).await?;
    println!("{label}: {EXPLORER}/{txid}");
    Ok(())
}

#[tokio::main]
async fn main() -> PlaygroundResult<()> {
    let keypair = load_or_create_keypair()?;
    let address = funding_address(&keypair);
    let change_spk = pay_to_address_script(&address);
    println!("funding address: {address}");

    let client = KaspaRpcClient::new(WrpcEncoding::Borsh, Some(NODE_URL), None, None, None)?;
    client.connect(Some(ConnectOptions::blocking_fallback())).await?;
    let info = client.get_server_info().await?;
    println!("connected: {} v{} synced={}", info.network_id, info.server_version, info.is_synced);

    let utxos = client.get_utxos_by_addresses(vec![address.clone()]).await?;
    let total: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
    println!("balance: {} TKAS across {} utxos", total as f64 / 1e8, utxos.len());
    if total < MIN_FUNDING {
        println!();
        println!("need at least {} TKAS — fund via the tn10 faucet, then re-run:", MIN_FUNDING as f64 / 1e8);
        println!("  {address}");
        return Ok(());
    }

    // Largest faucet UTXO funds everything.
    let funding = utxos.iter().max_by_key(|u| u.utxo_entry.amount).expect("nonempty");
    let funding_outpoint = TransactionOutpoint::new(funding.outpoint.transaction_id, funding.outpoint.index);
    let funding_entry = UtxoEntry {
        amount: funding.utxo_entry.amount,
        script_public_key: funding.utxo_entry.script_public_key.clone(),
        block_daa_score: funding.utxo_entry.block_daa_score,
        is_coinbase: funding.utxo_entry.is_coinbase,
        covenant_id: funding.utxo_entry.covenant_id,
    };

    // Compile + bundle, same as the local demo.
    let core_artifact = build_file(CORE_SOURCE, "build/open_lattice/core")?;
    let agent_artifact = build_file(AGENT_SOURCE, "build/open_lattice/agent")?;
    let bundle = ArtifactBundle::new(&core_artifact)?.with_app("open_lattice_agent", &agent_artifact)?;
    let builder = TxBuilder::from_bundle(&bundle)?;
    let agent_template = {
        let hex = &agent_artifact.sil_abi.contract("Agent").ok_or("Agent ABI exists")?.compiled.template.hash_hex;
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect::<Result<Vec<u8>, _>>()?
    };
    let empty_covid = Hash::from_bytes(EMPTY_SENTINEL);

    // --- Tx 1: world cell genesis + change ------------------------------------
    let cell_initial = cell_state(40, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
    let change_1 = funding_entry.amount - COVENANT_VALUE - FEE;
    let mut world_tx = TxBuilder::transaction(
        vec![input_with_budget(funding_outpoint, Vec::new(), P2PK_BUDGET)],
        vec![
            builder.genesis_output("Cell", cell_initial.clone(), COVENANT_VALUE)?,
            TransactionOutput::new(change_1, change_spk.clone()),
        ],
    );
    let world_genesis = TxBuilder::populate_genesis_covenants(&mut world_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let cell_root = world_genesis.output(0)?;
    world_tx.inputs[0].signature_script = sign_p2pk_input(&world_tx, &[funding_entry.clone()], 0, &keypair)?;
    world_tx.finalize();
    submit(&client, "world genesis (Cell covenant)", &world_tx).await?;
    println!("  world covenant id: {}", cell_root.covenant_id);

    // --- Tx 2: agent genesis + change (spends tx1 change) ---------------------
    let agent_initial = capsule(0x01, cell_root.covenant_id, 0, 0, 5);
    let change_1_outpoint = TransactionOutpoint::new(world_tx.id(), 1);
    let change_1_entry = UtxoEntry {
        amount: change_1,
        script_public_key: change_spk.clone(),
        block_daa_score: 0,
        is_coinbase: false,
        covenant_id: None,
    };
    let change_2 = change_1 - COVENANT_VALUE - FEE;
    let mut agent_tx = TxBuilder::transaction(
        vec![input_with_budget(change_1_outpoint, Vec::new(), P2PK_BUDGET)],
        vec![
            builder.genesis_output_in_app("open_lattice_agent", "Agent", agent_initial.clone(), COVENANT_VALUE)?,
            TransactionOutput::new(change_2, change_spk.clone()),
        ],
    );
    let agent_genesis = TxBuilder::populate_genesis_covenants(&mut agent_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let agent_root = agent_genesis.output(0)?;
    agent_tx.inputs[0].signature_script = sign_p2pk_input(&agent_tx, &[change_1_entry], 0, &keypair)?;
    agent_tx.finalize();
    submit(&client, "agent genesis (Agent covenant)", &agent_tx).await?;
    println!("  agent covenant id: {}", agent_root.covenant_id);

    // --- Tx 3: attach — the agent joins the world -----------------------------
    let cell_attached = cell_state(40, agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let observed = observe_ctx("joining", agent_root.utxo.clone(), agent_initial.clone(), agent_initial.clone());
    let cell_out_value = COVENANT_VALUE - FEE;
    let mut attach_outputs = vec![builder.covenant_output("Cell", cell_attached.clone(), cell_out_value, 0, cell_root.covenant_id)?];
    attach_outputs.extend(builder.observed_outputs(
        "Cell",
        "attach",
        "joining",
        observed.get("joining").expect("ctx"),
        BTreeMap::from([("agent".to_string(), COVENANT_VALUE)]),
        1,
        agent_root.covenant_id,
    )?);
    let cell_sig = builder.p2sh_signature_script_with_observed_covenants(
        "Cell",
        "attach",
        cell_initial,
        args![agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST],
        &observed,
    )?;
    let agent_sig = builder.p2sh_signature_script_in_app(
        "open_lattice_agent",
        "Agent",
        "step",
        agent_initial.clone(),
        args![agent_initial.clone()],
    )?;
    let mut attach_tx = TxBuilder::transaction(
        vec![
            input_with_budget(cell_root.outpoint, cell_sig, COVENANT_BUDGET),
            input_with_budget(agent_root.outpoint, agent_sig, COVENANT_BUDGET),
        ],
        attach_outputs,
    );
    attach_tx.finalize();
    // Pre-validate locally with the same consensus code before spending real UTXOs.
    let attach_entries = vec![cell_root.utxo.clone(), agent_root.utxo.clone()];
    execute_input_with_covenants(&attach_tx, attach_entries.clone(), 0)?;
    execute_input_with_covenants(&attach_tx, attach_entries, 1)?;
    submit(&client, "attach (agent joins world)", &attach_tx).await?;

    // --- Tx 4: harvest — a real game action under the quota -------------------
    let amount = 8_i64;
    let cell_prev = cell_attached;
    let agent_prev = agent_initial;
    let cell_next = cell_state(40 - amount, agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let agent_next = capsule(0x01, cell_root.covenant_id, 0, 0, 5 + amount);

    let cell_utxo = UtxoEntry {
        amount: cell_out_value,
        script_public_key: attach_tx.outputs[0].script_public_key.clone(),
        block_daa_score: 0,
        is_coinbase: false,
        covenant_id: Some(cell_root.covenant_id),
    };
    let agent_utxo = UtxoEntry {
        amount: COVENANT_VALUE,
        script_public_key: attach_tx.outputs[1].script_public_key.clone(),
        block_daa_score: 0,
        is_coinbase: false,
        covenant_id: Some(agent_root.covenant_id),
    };

    let observed = observe_ctx("remote", agent_utxo.clone(), agent_prev.clone(), agent_next.clone());
    let harvest_out_value = cell_out_value - FEE;
    let mut harvest_outputs =
        vec![builder.covenant_output("Cell", cell_next, harvest_out_value, 0, cell_root.covenant_id)?];
    harvest_outputs.extend(builder.observed_outputs(
        "Cell",
        "harvest",
        "remote",
        observed.get("remote").expect("ctx"),
        BTreeMap::from([("agent".to_string(), COVENANT_VALUE)]),
        1,
        agent_root.covenant_id,
    )?);
    let cell_sig =
        builder.p2sh_signature_script_with_observed_covenants("Cell", "harvest", cell_prev, args![amount], &observed)?;
    let agent_sig =
        builder.p2sh_signature_script_in_app("open_lattice_agent", "Agent", "step", agent_prev, args![agent_next])?;
    let mut harvest_tx = TxBuilder::transaction(
        vec![
            input_with_budget(TransactionOutpoint::new(attach_tx.id(), 0), cell_sig, COVENANT_BUDGET),
            input_with_budget(TransactionOutpoint::new(attach_tx.id(), 1), agent_sig, COVENANT_BUDGET),
        ],
        harvest_outputs,
    );
    harvest_tx.finalize();
    let harvest_entries = vec![cell_utxo, agent_utxo];
    execute_input_with_covenants(&harvest_tx, harvest_entries.clone(), 0)?;
    execute_input_with_covenants(&harvest_tx, harvest_entries, 1)?;
    submit(&client, "harvest 8 (game action)", &harvest_tx).await?;

    println!();
    println!("OPEN LATTICE IS LIVE ON TESTNET-10");
    println!("  world covenant: {}", cell_root.covenant_id);
    println!("  agent covenant: {}", agent_root.covenant_id);
    Ok(())
}

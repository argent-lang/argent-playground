use std::collections::BTreeMap;

use argent::build_file;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{ArtifactBundle, ObservedCovenantContext, TxBuilder, args, execute_input_with_covenants, state};
use kaspa_consensus_core::Hash;
use kaspa_consensus_core::tx::GenesisCovenantGroup;

const CORE_SOURCE: &str = "ag/open_lattice/core.ag";
const AGENT_SOURCE: &str = "ag/open_lattice/agent.ag";

const WORLD_ID: [u8; 32] = [0x11; 32];
const CAPS_DIGEST: [u8; 32] = [0xAA; 32];
const EMPTY_SENTINEL: [u8; 32] = [0x00; 32];

const CELL_VALUE: u64 = 10_000;
const AGENT_VALUE: u64 = 5_000;

// Season 0 physics constants (must match core.ag).
const QUOTA_FLOOR: i64 = 20;
const DEATH_MULCH: i64 = 12;

fn capsule(
    agent_id: u8,
    controller_id: Hash,
    caps: [u8; 32],
    x: i64,
    y: i64,
    energy: i64,
) -> BTreeMap<String, argent_runtime::ArtifactValue> {
    state! {
        world_id: WORLD_ID,
        agent_id: [agent_id; 32],
        species_id: [0x02; 32],
        controller_id: controller_id,
        capabilities_digest: caps,
        strategy: [0x44; 32],
        x: x,
        y: y,
        energy: energy,
        generation: 1,
    }
}

fn cell(
    x: i64,
    y: i64,
    food: i64,
    occupant_covid: Hash,
    occupant_type: Vec<u8>,
    occupant_caps: [u8; 32],
) -> BTreeMap<String, argent_runtime::ArtifactValue> {
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

fn main() -> PlaygroundResult<()> {
    let core_artifact = build_file(CORE_SOURCE, "build/open_lattice/core")?;
    let agent_artifact = build_file(AGENT_SOURCE, "build/open_lattice/agent")?;
    let bundle = ArtifactBundle::new(&core_artifact)?.with_app("open_lattice_agent", &agent_artifact)?;
    let builder = TxBuilder::from_bundle(&bundle)?;

    let agent_template = hex_bytes(
        &agent_artifact.sil_abi.contract("Agent").ok_or("Agent ABI exists")?.compiled.template.hash_hex,
    )?;
    let empty_covid = Hash::from_bytes(EMPTY_SENTINEL);

    // --- Genesis: one empty world cell ---------------------------------------
    let cell_initial = cell(0, 0, 40, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
    let mut cell_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x81, 0), Vec::new())],
        vec![builder.genesis_output("Cell", cell_initial.clone(), CELL_VALUE)?],
    );
    let cell_genesis = TxBuilder::populate_genesis_covenants(&mut cell_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let cell_root = cell_genesis.output(0)?;
    println!("cell covenant launched: {}", cell_root.covenant_id);

    // --- Genesis: one unbound agent, controller = the cell covenant ----------
    let agent_initial = capsule(0x01, cell_root.covenant_id, CAPS_DIGEST, 0, 0, 5);
    let mut agent_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x82, 0), Vec::new())],
        vec![builder.genesis_output_in_app("open_lattice_agent", "Agent", agent_initial.clone(), AGENT_VALUE)?],
    );
    let agent_genesis = TxBuilder::populate_genesis_covenants(&mut agent_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let agent_root = agent_genesis.output(0)?;
    println!("agent covenant launched: {}", agent_root.covenant_id);

    // --- Attach: the lobby transition binding agent into the cell lock -------
    let cell_attached = cell(0, 0, 40, agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let observed = observe_ctx("joining", "Agent", agent_root.utxo.clone(), agent_initial.clone(), Some(agent_initial.clone()));
    let mut outputs = vec![builder.covenant_output("Cell", cell_attached.clone(), CELL_VALUE, 0, cell_root.covenant_id)?];
    outputs.extend(builder.observed_outputs(
        "Cell",
        "attach",
        "joining",
        ctx(&observed, "joining"),
        BTreeMap::from([("agent".to_string(), AGENT_VALUE)]),
        1,
        agent_root.covenant_id,
    )?);
    let cell_sig = builder.p2sh_signature_script_with_observed_covenants(
        "Cell",
        "attach",
        cell_initial.clone(),
        args![agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST],
        &observed,
    )?;
    let agent_sig =
        builder.p2sh_signature_script_in_app("open_lattice_agent", "Agent", "step", agent_initial.clone(), args![agent_initial.clone()])?;
    let attach_tx = TxBuilder::transaction(
        vec![
            TxBuilder::transaction_input(cell_root.outpoint, cell_sig),
            TxBuilder::transaction_input(agent_root.outpoint, agent_sig),
        ],
        outputs,
    );
    let entries = vec![cell_root.utxo.clone(), agent_root.utxo.clone()];
    execute_input_with_covenants(&attach_tx, entries.clone(), 0)?;
    execute_input_with_covenants(&attach_tx, entries, 1)?;
    println!("attach: agent joined the world at (0,0)");

    // --- Harvest under the quota: two legal skims ----------------------------
    let mut food = 40;
    let mut energy = 5;
    let mut harvest_sig_sizes = (0usize, 0usize);
    for amount in [8_i64, 8] {
        let (cell_prev, agent_prev) = (
            cell(0, 0, food, agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST),
            capsule(0x01, cell_root.covenant_id, CAPS_DIGEST, 0, 0, energy),
        );
        food -= amount;
        energy += amount;
        let (cell_next, agent_next) = (
            cell(0, 0, food, agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST),
            capsule(0x01, cell_root.covenant_id, CAPS_DIGEST, 0, 0, energy),
        );
        harvest_sig_sizes = harvest_tx(&builder, &agent_template, cell_prev, cell_next, agent_prev, agent_next, amount, true)?;
        println!("harvest {amount}: cell food -> {food}, agent energy -> {energy}");
    }

    // --- Over-harvest: legal for the agent, refused by cell physics ----------
    let amount = 10;
    let cell_prev = cell(0, 0, food, agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let agent_prev = capsule(0x01, cell_root.covenant_id, CAPS_DIGEST, 0, 0, energy);
    let cell_next = cell(0, 0, food - amount, agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let agent_next = capsule(0x01, cell_root.covenant_id, CAPS_DIGEST, 0, 0, energy + amount);
    assert!(food - amount < QUOTA_FLOOR, "test setup: this harvest must violate the quota");
    harvest_tx(&builder, &agent_template, cell_prev, cell_next, agent_prev, agent_next, amount, false)?;
    println!("harvest {amount}: REJECTED by cell covenant (would leave {} < quota floor {QUOTA_FLOOR})", food - amount);

    // --- Reap: a starved agent dies and mulches into the cell ----------------
    let dead_initial = capsule(0x03, cell_root.covenant_id, CAPS_DIGEST, 0, 0, 0);
    let mut dead_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x83, 0), Vec::new())],
        vec![builder.genesis_output_in_app("open_lattice_agent", "Agent", dead_initial.clone(), AGENT_VALUE)?],
    );
    let dead_genesis = TxBuilder::populate_genesis_covenants(&mut dead_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let dead_root = dead_genesis.output(0)?;

    let cell_prev = cell(0, 0, food, dead_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let cell_utxo = builder.covenant_utxo("Cell", cell_prev.clone(), CELL_VALUE, 0, false, Some(cell_root.covenant_id))?;
    let cell_next = cell(0, 0, food + DEATH_MULCH, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
    let observed = observe_ctx("remote", "Agent", dead_root.utxo.clone(), dead_initial.clone(), None);
    let reap_outputs = vec![builder.covenant_output("Cell", cell_next, CELL_VALUE + AGENT_VALUE, 0, cell_root.covenant_id)?];
    let cell_sig = builder.p2sh_signature_script_with_observed_covenants(
        "Cell",
        "reap",
        cell_prev,
        args![empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL],
        &observed,
    )?;
    let perish_sig = builder.p2sh_signature_script_in_app("open_lattice_agent", "Agent", "perish", dead_initial, args![])?;
    let reap_tx = TxBuilder::transaction(
        vec![
            TxBuilder::transaction_input(demo_outpoint(0x84, 0), cell_sig),
            TxBuilder::transaction_input(dead_root.outpoint, perish_sig),
        ],
        reap_outputs,
    );
    let entries = vec![cell_utxo, dead_root.utxo.clone()];
    execute_input_with_covenants(&reap_tx, entries.clone(), 0)?;
    execute_input_with_covenants(&reap_tx, entries, 1)?;
    println!("reap: starved agent perished, cell food -> {} (+{DEATH_MULCH} mulch), occupant cleared", food + DEATH_MULCH);

    println!();
    println!("season 0 physics verified: attach, harvest x2, quota rejection, reap");
    println!();
    println!("on-chain cost of one game action (harvest = 2 inputs, 2 outputs):");
    println!("  cell sigscript:  {} bytes", harvest_sig_sizes.0);
    println!("  agent sigscript: {} bytes", harvest_sig_sizes.1);
    for (name, path) in [("Cell.sil", "build/open_lattice/core/sil/Cell.sil"), ("Agent.sil", "build/open_lattice/agent/sil/Agent.sil")] {
        if let Ok(meta) = std::fs::metadata(path) {
            println!("  {name} (generated source): {} bytes", meta.len());
        }
    }
    println!("artifacts: build/open_lattice/{{core,agent}}/artifact.json");
    Ok(())
}

fn observe_ctx(
    observe: &str,
    actor: &str,
    utxo: kaspa_consensus_core::tx::UtxoEntry,
    input_state: BTreeMap<String, argent_runtime::ArtifactValue>,
    output_state: Option<BTreeMap<String, argent_runtime::ArtifactValue>>,
) -> BTreeMap<String, ObservedCovenantContext> {
    let mut context = ObservedCovenantContext::from_app("open_lattice_agent").input("agent", actor, utxo, input_state);
    if let Some(next) = output_state {
        context = context.output("agent", actor, next);
    }
    BTreeMap::from([(observe.to_string(), context)])
}

fn ctx<'a>(observed: &'a BTreeMap<String, ObservedCovenantContext>, name: &str) -> &'a ObservedCovenantContext {
    observed.get(name).expect("observed context exists")
}

#[allow(clippy::too_many_arguments)]
fn harvest_tx(
    builder: &TxBuilder<'_>,
    agent_template: &[u8],
    cell_prev: BTreeMap<String, argent_runtime::ArtifactValue>,
    cell_next: BTreeMap<String, argent_runtime::ArtifactValue>,
    agent_prev: BTreeMap<String, argent_runtime::ArtifactValue>,
    agent_next: BTreeMap<String, argent_runtime::ArtifactValue>,
    amount: i64,
    expect_pass: bool,
) -> PlaygroundResult<(usize, usize)> {
    let _ = agent_template;
    let cell_covid = extract_covid(&agent_prev, "controller_id")?;
    let agent_covid = extract_covid(&cell_prev, "occupant_agent_covid")?;

    let cell_utxo = builder.covenant_utxo("Cell", cell_prev.clone(), CELL_VALUE, 0, false, Some(cell_covid))?;
    let agent_utxo = builder.covenant_utxo_in_app("open_lattice_agent", "Agent", agent_prev.clone(), AGENT_VALUE, 0, false, Some(agent_covid))?;

    let observed = observe_ctx("remote", "Agent", agent_utxo.clone(), agent_prev.clone(), Some(agent_next.clone()));
    let mut outputs = vec![builder.covenant_output("Cell", cell_next, CELL_VALUE, 0, cell_covid)?];
    outputs.extend(builder.observed_outputs(
        "Cell",
        "harvest",
        "remote",
        ctx(&observed, "remote"),
        BTreeMap::from([("agent".to_string(), AGENT_VALUE)]),
        1,
        agent_covid,
    )?);

    let cell_sig =
        builder.p2sh_signature_script_with_observed_covenants("Cell", "harvest", cell_prev, args![amount], &observed)?;
    let agent_sig = builder.p2sh_signature_script_in_app("open_lattice_agent", "Agent", "step", agent_prev, args![agent_next])?;
    let sig_sizes = (cell_sig.len(), agent_sig.len());
    let tx = TxBuilder::transaction(
        vec![
            TxBuilder::transaction_input(demo_outpoint(0x90, 0), cell_sig),
            TxBuilder::transaction_input(demo_outpoint(0x91, 0), agent_sig),
        ],
        outputs,
    );
    let entries = vec![cell_utxo, agent_utxo];
    let cell_result = execute_input_with_covenants(&tx, entries.clone(), 0);
    if expect_pass {
        cell_result?;
        execute_input_with_covenants(&tx, entries, 1)?;
    } else {
        assert!(cell_result.is_err(), "cell covenant must reject harvest {amount} below the quota floor");
        execute_input_with_covenants(&tx, entries, 1)?; // agent side alone still authorizes
    }
    Ok(sig_sizes)
}

fn extract_covid(state: &BTreeMap<String, argent_runtime::ArtifactValue>, field: &str) -> PlaygroundResult<Hash> {
    match state.get(field) {
        Some(argent_runtime::ArtifactValue::Bytes(bytes)) if bytes.len() == 32 => {
            let mut raw = [0u8; 32];
            raw.copy_from_slice(bytes);
            Ok(Hash::from_bytes(raw))
        }
        other => Err(format!("state field {field} is not a 32-byte value: {other:?}").into()),
    }
}

fn hex_bytes(hex: &str) -> PlaygroundResult<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return Err("odd-length hex".into());
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(Into::into)).collect()
}

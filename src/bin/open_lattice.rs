use std::collections::BTreeMap;

use argent::build_file;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{
    ArtifactBundle, ObservedCovenantContext, TerminalPathOutputRequest, TxBuilder, args, execute_input_with_covenants, state,
};
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
const REPRO_COST: i64 = 24;
const CHILD_ENERGY: i64 = 18;

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

    // --- Share: giver funds an adjacent agent at 120% (two observed covenants)
    let recv_initial = capsule(0x05, cell_root.covenant_id, CAPS_DIGEST, 0, 1, 3);
    let mut recv_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x85, 0), Vec::new())],
        vec![builder.genesis_output_in_app("open_lattice_agent", "Agent", recv_initial.clone(), AGENT_VALUE)?],
    );
    let recv_genesis = TxBuilder::populate_genesis_covenants(&mut recv_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let recv_root = recv_genesis.output(0)?;

    let (share_amount, receiver_gain) = (10_i64, 12_i64);
    let giver_prev = capsule(0x01, cell_root.covenant_id, CAPS_DIGEST, 0, 0, energy);
    let giver_next = capsule(0x01, cell_root.covenant_id, CAPS_DIGEST, 0, 0, energy - share_amount);
    let recv_next = capsule(0x05, cell_root.covenant_id, CAPS_DIGEST, 0, 1, 3 + receiver_gain);
    let cell_prev = cell(0, 0, food, agent_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let cell_utxo = builder.covenant_utxo("Cell", cell_prev.clone(), CELL_VALUE, 0, false, Some(cell_root.covenant_id))?;
    let giver_utxo = builder.covenant_utxo_in_app(
        "open_lattice_agent",
        "Agent",
        giver_prev.clone(),
        AGENT_VALUE,
        0,
        false,
        Some(agent_root.covenant_id),
    )?;

    let mut observed = observe_ctx("giving", "Agent", giver_utxo.clone(), giver_prev.clone(), Some(giver_next.clone()));
    observed.extend(observe_ctx("receiving", "Agent", recv_root.utxo.clone(), recv_initial.clone(), Some(recv_next.clone())));

    let mut share_outputs = vec![builder.covenant_output("Cell", cell_prev.clone(), CELL_VALUE, 0, cell_root.covenant_id)?];
    share_outputs.extend(builder.observed_outputs(
        "Cell",
        "share",
        "giving",
        ctx(&observed, "giving"),
        BTreeMap::from([("agent".to_string(), AGENT_VALUE)]),
        1,
        agent_root.covenant_id,
    )?);
    share_outputs.extend(builder.observed_outputs(
        "Cell",
        "share",
        "receiving",
        ctx(&observed, "receiving"),
        BTreeMap::from([("agent".to_string(), AGENT_VALUE)]),
        2,
        recv_root.covenant_id,
    )?);

    let cell_sig = builder.p2sh_signature_script_with_observed_covenants(
        "Cell",
        "share",
        cell_prev,
        args![share_amount, receiver_gain, recv_root.covenant_id, agent_template.clone(), CAPS_DIGEST],
        &observed,
    )?;
    let giver_sig =
        builder.p2sh_signature_script_in_app("open_lattice_agent", "Agent", "step", giver_prev, args![giver_next])?;
    let recv_sig =
        builder.p2sh_signature_script_in_app("open_lattice_agent", "Agent", "step", recv_initial, args![recv_next])?;
    let share_tx = TxBuilder::transaction(
        vec![
            TxBuilder::transaction_input(demo_outpoint(0x92, 0), cell_sig),
            TxBuilder::transaction_input(demo_outpoint(0x93, 0), giver_sig),
            TxBuilder::transaction_input(recv_root.outpoint, recv_sig),
        ],
        share_outputs,
    );
    let entries = vec![cell_utxo, giver_utxo, recv_root.utxo.clone()];
    for input_idx in 0..3 {
        execute_input_with_covenants(&share_tx, entries.clone(), input_idx)?;
    }
    energy -= share_amount;
    println!("share {share_amount}: giver energy -> {energy}, receiver energy -> {} (+20% bonus, two observed covenants)", 3 + receiver_gain);

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

    // --- Move: two cells launched under ONE world covenant id ----------------
    // Consumed peers must share the leader's covenant id, so a world's cells
    // are one covenant lineage. The target cell input self-validates through
    // the host_move delegate.
    let cell_a_initial = cell(0, 0, 40, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
    let cell_b_initial = cell(1, 0, 25, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
    let mut world_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x86, 0), Vec::new())],
        vec![
            builder.genesis_output("Cell", cell_a_initial.clone(), CELL_VALUE)?,
            builder.genesis_output("Cell", cell_b_initial.clone(), CELL_VALUE)?,
        ],
    );
    let world_genesis = TxBuilder::populate_genesis_covenants(&mut world_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0, 1])])?;
    let (cell_a_root, cell_b_root) = (world_genesis.output(0)?, world_genesis.output(1)?);
    let world_covid = cell_a_root.covenant_id;
    assert_eq!(world_covid, cell_b_root.covenant_id, "one genesis group => one world covenant id");
    println!("world covenant launched (2 cells, one covid): {world_covid}");

    let walker_initial = capsule(0x07, world_covid, CAPS_DIGEST, 0, 0, 40);
    let mut walker_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x87, 0), Vec::new())],
        vec![builder.genesis_output_in_app("open_lattice_agent", "Agent", walker_initial.clone(), AGENT_VALUE)?],
    );
    let walker_genesis = TxBuilder::populate_genesis_covenants(&mut walker_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let walker_root = walker_genesis.output(0)?;

    // Attach the walker to cell A.
    let cell_a_attached = cell(0, 0, 40, walker_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let observed = observe_ctx("joining", "Agent", walker_root.utxo.clone(), walker_initial.clone(), Some(walker_initial.clone()));
    let mut outputs = vec![builder.covenant_output("Cell", cell_a_attached.clone(), CELL_VALUE, 0, world_covid)?];
    outputs.extend(builder.observed_outputs(
        "Cell",
        "attach",
        "joining",
        ctx(&observed, "joining"),
        BTreeMap::from([("agent".to_string(), AGENT_VALUE)]),
        1,
        walker_root.covenant_id,
    )?);
    let cell_sig = builder.p2sh_signature_script_with_observed_covenants(
        "Cell",
        "attach",
        cell_a_initial,
        args![walker_root.covenant_id, agent_template.clone(), CAPS_DIGEST],
        &observed,
    )?;
    let walker_sig = builder
        .p2sh_signature_script_in_app("open_lattice_agent", "Agent", "step", walker_initial.clone(), args![walker_initial.clone()])?;
    let attach_tx = TxBuilder::transaction(
        vec![
            TxBuilder::transaction_input(cell_a_root.outpoint, cell_sig),
            TxBuilder::transaction_input(walker_root.outpoint, walker_sig),
        ],
        outputs,
    );
    let entries = vec![cell_a_root.utxo.clone(), walker_root.utxo.clone()];
    execute_input_with_covenants(&attach_tx, entries.clone(), 0)?;
    execute_input_with_covenants(&attach_tx, entries, 1)?;

    // The move itself: source cell leads, target cell delegates, agent observes.
    let walker_next = capsule(0x07, world_covid, CAPS_DIGEST, 1, 0, 39);
    let cell_a_next = cell(0, 0, 40, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
    let cell_b_next = cell(1, 0, 25, walker_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let cell_a_utxo = builder.covenant_utxo("Cell", cell_a_attached.clone(), CELL_VALUE, 0, false, Some(world_covid))?;
    let cell_b_utxo = builder.covenant_utxo("Cell", cell_b_initial.clone(), CELL_VALUE, 0, false, Some(world_covid))?;
    let walker_utxo = builder.covenant_utxo_in_app(
        "open_lattice_agent",
        "Agent",
        walker_initial.clone(),
        AGENT_VALUE,
        0,
        false,
        Some(walker_root.covenant_id),
    )?;

    let observed = observe_ctx("remote", "Agent", walker_utxo.clone(), walker_initial.clone(), Some(walker_next.clone()));
    let mut move_outputs = builder.terminal_path_outputs(TerminalPathOutputRequest {
        actor_name: "Cell",
        entry_name: "move",
        path_index: 0,
        output_states: BTreeMap::from([("source_cell".to_string(), cell_a_next), ("target_cell".to_string(), cell_b_next)]),
        output_values: BTreeMap::from([("source_cell".to_string(), CELL_VALUE), ("target_cell".to_string(), CELL_VALUE)]),
        authorizing_input: 0,
        covenant_id: world_covid,
    })?;
    move_outputs.extend(builder.observed_outputs(
        "Cell",
        "move",
        "remote",
        ctx(&observed, "remote"),
        BTreeMap::from([("agent".to_string(), AGENT_VALUE)]),
        2,
        walker_root.covenant_id,
    )?);

    let move_sig = builder.p2sh_signature_script_with_observed_covenants(
        "Cell",
        "move",
        cell_a_attached,
        args![1, 0, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL],
        &observed,
    )?;
    let host_sig = builder.p2sh_signature_script("Cell", "host_move", cell_b_initial, args![])?;
    let step_sig =
        builder.p2sh_signature_script_in_app("open_lattice_agent", "Agent", "step", walker_initial, args![walker_next.clone()])?;
    let move_tx = TxBuilder::transaction(
        vec![
            TxBuilder::transaction_input(demo_outpoint(0x94, 0), move_sig),
            TxBuilder::transaction_input(demo_outpoint(0x95, 0), host_sig),
            TxBuilder::transaction_input(demo_outpoint(0x96, 0), step_sig),
        ],
        move_outputs,
    );
    let entries = vec![cell_a_utxo, cell_b_utxo, walker_utxo];
    for input_idx in 0..3 {
        execute_input_with_covenants(&move_tx, entries.clone(), input_idx)?;
    }
    println!("move: agent walked (0,0) -> (1,0); source cleared, target bound, energy 40 -> 39");

    // --- Reproduce: parent births a pre-launched child onto the vacated cell -
    // Lobby step: player funds the child agent's genesis, unbound, gen 2.
    let child_initial = {
        let mut c = capsule(0x08, world_covid, CAPS_DIGEST, 0, 0, CHILD_ENERGY);
        c.insert("generation".to_string(), argent_runtime::ArtifactValue::Int(2));
        c
    };
    let mut child_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x88, 0), Vec::new())],
        vec![builder.genesis_output_in_app("open_lattice_agent", "Agent", child_initial.clone(), AGENT_VALUE)?],
    );
    let child_genesis = TxBuilder::populate_genesis_covenants(&mut child_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let child_root = child_genesis.output(0)?;

    // World step: cell A (now empty) recognizes the birth. Parent sits on cell
    // B at (1,0), adjacent; both observe lanes bind through the SAME actor
    // handle, so the child must carry the parent's exact template.
    let parent_prev = walker_next.clone();
    let parent_next = capsule(0x07, world_covid, CAPS_DIGEST, 1, 0, 39 - REPRO_COST);
    let cell_a_empty = cell(0, 0, 40, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
    let cell_a_born = cell(0, 0, 40, child_root.covenant_id, agent_template.clone(), CAPS_DIGEST);
    let cell_a_utxo = builder.covenant_utxo("Cell", cell_a_empty.clone(), CELL_VALUE, 0, false, Some(world_covid))?;
    let parent_utxo = builder.covenant_utxo_in_app(
        "open_lattice_agent",
        "Agent",
        parent_prev.clone(),
        AGENT_VALUE,
        0,
        false,
        Some(walker_root.covenant_id),
    )?;

    let mut observed = observe_ctx("parent_lane", "Agent", parent_utxo.clone(), parent_prev.clone(), Some(parent_next.clone()));
    observed.extend(observe_ctx("child_lane", "Agent", child_root.utxo.clone(), child_initial.clone(), Some(child_initial.clone())));

    let mut repro_outputs = vec![builder.covenant_output("Cell", cell_a_born, CELL_VALUE, 0, world_covid)?];
    repro_outputs.extend(builder.observed_outputs(
        "Cell",
        "reproduce",
        "parent_lane",
        ctx(&observed, "parent_lane"),
        BTreeMap::from([("agent".to_string(), AGENT_VALUE)]),
        1,
        walker_root.covenant_id,
    )?);
    repro_outputs.extend(builder.observed_outputs(
        "Cell",
        "reproduce",
        "child_lane",
        ctx(&observed, "child_lane"),
        BTreeMap::from([("agent".to_string(), AGENT_VALUE)]),
        2,
        child_root.covenant_id,
    )?);

    let repro_sig = builder.p2sh_signature_script_with_observed_covenants(
        "Cell",
        "reproduce",
        cell_a_empty,
        args![walker_root.covenant_id, agent_template.clone(), CAPS_DIGEST, child_root.covenant_id, CAPS_DIGEST],
        &observed,
    )?;
    let parent_sig =
        builder.p2sh_signature_script_in_app("open_lattice_agent", "Agent", "step", parent_prev, args![parent_next])?;
    let child_sig = builder
        .p2sh_signature_script_in_app("open_lattice_agent", "Agent", "step", child_initial.clone(), args![child_initial.clone()])?;
    let repro_tx = TxBuilder::transaction(
        vec![
            TxBuilder::transaction_input(demo_outpoint(0x97, 0), repro_sig),
            TxBuilder::transaction_input(demo_outpoint(0x98, 0), parent_sig),
            TxBuilder::transaction_input(child_root.outpoint, child_sig),
        ],
        repro_outputs,
    );
    let entries = vec![cell_a_utxo, parent_utxo, child_root.utxo.clone()];
    for input_idx in 0..3 {
        execute_input_with_covenants(&repro_tx, entries.clone(), input_idx)?;
    }
    println!(
        "reproduce: gen-2 child born at (0,0), parent energy 39 -> {}, breed-true enforced by shared actor handle",
        39 - REPRO_COST
    );

    println!();
    println!("season 0 physics verified: attach, harvest, quota rejection, share, reap, move, reproduce");
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

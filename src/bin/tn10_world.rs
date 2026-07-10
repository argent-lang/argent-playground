// Open Lattice — an 8x8 world live on testnet-10 with parallel agents.
//
// One genesis transaction mints all 64 cells under a single world covenant
// id. Six agents launch, attach, and then play rounds of harvest/move — each
// round's actions touch disjoint neighborhoods, so they are submitted
// CONCURRENTLY and confirm independently. That parallelism is the point:
// there is no global state object to contend on.
//
// Emits build/world-log.json for a future map viewer.

use std::collections::BTreeMap;
use std::time::Instant;

use argent::build_file;
use argent_playground::{
    AgentView, CellView, WorldState,
    CAPS_DIGEST, COVENANT_BUDGET, EMPTY_SENTINEL, EXPLORER, node_url, P2PK_BUDGET, PlaygroundResult, funding_address,
    hex_bytes, input_with_budget, lattice_capsule, lattice_cell, lattice_observe_ctx, load_or_create_keypair,
    sign_p2pk_input,
};
use argent_runtime::{ArtifactBundle, ArtifactValue, TerminalPathOutputRequest, TxBuilder, args, execute_input_with_covenants};
use kaspa_consensus_core::Hash;
use kaspa_consensus_core::tx::{GenesisCovenantGroup, Transaction, TransactionOutpoint, TransactionOutput, UtxoEntry};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::pay_to_address_script;
use kaspa_wrpc_client::{KaspaRpcClient, WrpcEncoding, prelude::ConnectOptions};
use serde_json::json;

const KEY_FILE: &str = ".tn10-key";
const SIZE: i64 = 8;
const N_AGENTS: usize = 6;
const ROUNDS: usize = 8;

const CELL_VALUE: u64 = 1_000_000_000; // 10 TKAS — keeps 64-output genesis under storage-mass limits
const AGENT_VALUE: u64 = 100_000_000; // 1 TKAS
const FEE: u64 = 15_000_000; // per action tx (compute mass at 100 sompi/gram)
const GENESIS_FEE: u64 = 20_000_000;

const QUOTA_FLOOR: i64 = 20;
const HARVEST: i64 = 8;

fn cell_idx(x: i64, y: i64) -> usize {
    (y * SIZE + x) as usize
}

fn initial_food(x: i64, y: i64) -> i64 {
    30 + ((x * 7 + y * 13) % 50)
}

fn cell_state_of(view: &CellView, world: &[AgentView], agent_template: &[u8]) -> BTreeMap<String, ArtifactValue> {
    match view.occupant {
        Some(a) => lattice_cell(view.x, view.y, view.food, world[a].covid, agent_template.to_vec(), CAPS_DIGEST),
        None => lattice_cell(view.x, view.y, view.food, Hash::from_bytes(EMPTY_SENTINEL), EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL),
    }
}

fn utxo_of(spk: &kaspa_consensus_core::tx::ScriptPublicKey, value: u64, covid: Hash) -> UtxoEntry {
    UtxoEntry { amount: value, script_public_key: spk.clone(), block_daa_score: 0, is_coinbase: false, covenant_id: Some(covid) }
}

#[tokio::main]
async fn main() -> PlaygroundResult<()> {
    let keypair = load_or_create_keypair(KEY_FILE)?;
    let address = funding_address(&keypair);
    let change_spk = pay_to_address_script(&address);

    let client = KaspaRpcClient::new(WrpcEncoding::Borsh, Some(&node_url()), None, None, None)?;
    client.connect(Some(ConnectOptions::blocking_fallback())).await?;
    let info = client.get_server_info().await?;
    println!("connected: {} v{} synced={}", info.network_id, info.server_version, info.is_synced);

    let utxos = client.get_utxos_by_addresses(vec![address.clone()]).await?;
    let total: u64 = utxos.iter().map(|u| u.utxo_entry.amount).sum();
    let need = 64 * CELL_VALUE + N_AGENTS as u64 * AGENT_VALUE + 60 * FEE + 8 * GENESIS_FEE;
    println!("balance: {} TKAS", total as f64 / 1e8);
    if total < need {
        return Err(format!("need {} TKAS, have {}", need as f64 / 1e8, total as f64 / 1e8).into());
    }

    let core_artifact = build_file("ag/open_lattice/core.ag", "build/open_lattice/core")?;
    let agent_artifact = build_file("ag/open_lattice/agent.ag", "build/open_lattice/agent")?;
    let bundle = ArtifactBundle::new(&core_artifact)?.with_app("open_lattice_agent", &agent_artifact)?;
    let builder = TxBuilder::from_bundle(&bundle)?;
    let agent_template =
        hex_bytes(&agent_artifact.sil_abi.contract("Agent").ok_or("Agent ABI")?.compiled.template.hash_hex)?;

    let funding = utxos.iter().max_by_key(|u| u.utxo_entry.amount).expect("nonempty");
    let mut spend_outpoint = TransactionOutpoint::new(funding.outpoint.transaction_id, funding.outpoint.index);
    let mut spend_entry = UtxoEntry {
        amount: funding.utxo_entry.amount,
        script_public_key: funding.utxo_entry.script_public_key.clone(),
        block_daa_score: funding.utxo_entry.block_daa_score,
        is_coinbase: funding.utxo_entry.is_coinbase,
        covenant_id: funding.utxo_entry.covenant_id,
    };

    let mut log_events = Vec::new();

    // --- World genesis: 64 cells, ONE covenant id ------------------------------
    let mut genesis_outputs = Vec::with_capacity(65);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let state = lattice_cell(x, y, initial_food(x, y), Hash::from_bytes(EMPTY_SENTINEL), EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
            genesis_outputs.push(builder.genesis_output("Cell", state, CELL_VALUE)?);
        }
    }
    let change_value = spend_entry.amount - 64 * CELL_VALUE - GENESIS_FEE;
    genesis_outputs.push(TransactionOutput::new(change_value, change_spk.clone()));
    let mut world_tx = TxBuilder::transaction(
        vec![input_with_budget(spend_outpoint, Vec::new(), P2PK_BUDGET)],
        genesis_outputs,
    );
    let group: Vec<u32> = (0..64).collect();
    let world_genesis = TxBuilder::populate_genesis_covenants(&mut world_tx, &[GenesisCovenantGroup::new(0, group)])?;
    let world_covid = world_genesis.output(0)?.covenant_id;
    world_tx.inputs[0].signature_script = sign_p2pk_input(&world_tx, &[spend_entry.clone()], 0, &keypair)?;
    world_tx.finalize();
    let t0 = Instant::now();
    let txid = client.submit_transaction((&world_tx).into(), false).await?;
    println!("WORLD GENESIS — 64 cells, one covid, one tx ({}ms): {EXPLORER}/{txid}", t0.elapsed().as_millis());
    println!("  world covenant id: {world_covid}");

    let mut cells: Vec<CellView> = Vec::with_capacity(64);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let out = world_genesis.output(cell_idx(x, y) as u32)?;
            cells.push(CellView {
                x,
                y,
                food: initial_food(x, y),
                occupant: None,
                outpoint: out.outpoint,
                value: CELL_VALUE,
                spk: out.utxo.script_public_key.clone(),
            });
        }
    }
    spend_outpoint = TransactionOutpoint::new(world_tx.id(), 64);
    spend_entry = UtxoEntry {
        amount: change_value,
        script_public_key: change_spk.clone(),
        block_daa_score: 0,
        is_coinbase: false,
        covenant_id: None,
    };

    // --- Agent geneses (chained on change) -------------------------------------
    let spots: [(i64, i64); N_AGENTS] = [(1, 1), (6, 1), (1, 6), (6, 6), (3, 4), (4, 2)];
    let mut agents: Vec<AgentView> = Vec::with_capacity(N_AGENTS);
    for (i, (x, y)) in spots.iter().enumerate() {
        let agent_id = (i + 1) as u8;
        let capsule = lattice_capsule(agent_id, world_covid, *x, *y, 30);
        let change = spend_entry.amount - AGENT_VALUE - GENESIS_FEE;
        let mut tx = TxBuilder::transaction(
            vec![input_with_budget(spend_outpoint, Vec::new(), P2PK_BUDGET)],
            vec![
                builder.genesis_output_in_app("open_lattice_agent", "Agent", capsule, AGENT_VALUE)?,
                TransactionOutput::new(change, change_spk.clone()),
            ],
        );
        let genesis = TxBuilder::populate_genesis_covenants(&mut tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
        let root = genesis.output(0)?;
        tx.inputs[0].signature_script = sign_p2pk_input(&tx, &[spend_entry.clone()], 0, &keypair)?;
        tx.finalize();
        client.submit_transaction((&tx).into(), false).await?;
        agents.push(AgentView {
            covid: root.covenant_id,
            id: agent_id,
            x: *x,
            y: *y,
            energy: 30,
            outpoint: root.outpoint,
            value: AGENT_VALUE,
            spk: root.utxo.script_public_key.clone(),
        });
        spend_outpoint = TransactionOutpoint::new(tx.id(), 1);
        spend_entry = UtxoEntry {
            amount: change,
            script_public_key: change_spk.clone(),
            block_daa_score: 0,
            is_coinbase: false,
            covenant_id: None,
        };
    }
    println!("{} agent covenants launched", agents.len());

    // --- Attach all agents (submitted concurrently) ----------------------------
    let mut attach_txs = Vec::new();
    for (i, agent) in agents.iter().enumerate() {
        let ci = cell_idx(agent.x, agent.y);
        let cell_prev = cell_state_of(&cells[ci], &agents, &agent_template);
        let agent_state = lattice_capsule(agent.id, world_covid, agent.x, agent.y, agent.energy);
        let cell_next = lattice_cell(agent.x, agent.y, cells[ci].food, agent.covid, agent_template.clone(), CAPS_DIGEST);
        let observed = lattice_observe_ctx("joining", utxo_of(&agent.spk, agent.value, agent.covid), agent_state.clone(), agent_state.clone());
        let cell_out_value = cells[ci].value - FEE;
        let mut outputs = vec![builder.covenant_output("Cell", cell_next, cell_out_value, 0, world_covid)?];
        outputs.extend(builder.observed_outputs(
            "Cell",
            "attach",
            "joining",
            observed.get("joining").expect("ctx"),
            BTreeMap::from([("agent".to_string(), agent.value)]),
            1,
            agent.covid,
        )?);
        let cell_sig = builder.p2sh_signature_script_with_observed_covenants(
            "Cell",
            "attach",
            cell_prev,
            args![agent.covid, agent_template.clone(), CAPS_DIGEST],
            &observed,
        )?;
        let agent_sig = builder.p2sh_signature_script_in_app(
            "open_lattice_agent",
            "Agent",
            "step",
            agent_state.clone(),
            args![agent_state.clone()],
        )?;
        let mut tx = TxBuilder::transaction(
            vec![
                input_with_budget(cells[ci].outpoint, cell_sig, COVENANT_BUDGET),
                input_with_budget(agent.outpoint, agent_sig, COVENANT_BUDGET),
            ],
            outputs,
        );
        tx.finalize();
        let entries = vec![utxo_of(&cells[ci].spk, cells[ci].value, world_covid), utxo_of(&agent.spk, agent.value, agent.covid)];
        execute_input_with_covenants(&tx, entries.clone(), 0)?;
        execute_input_with_covenants(&tx, entries, 1)?;
        attach_txs.push((i, ci, cell_out_value, tx));
    }
    let t0 = Instant::now();
    let submits = attach_txs.iter().map(|(_, _, _, tx)| client.submit_transaction(tx.into(), false));
    let results = futures::future::join_all(submits).await;
    for r in &results {
        r.as_ref().map_err(|e| format!("attach submit failed: {e}"))?;
    }
    println!("{} attach txs submitted concurrently in {}ms — all agents joined", results.len(), t0.elapsed().as_millis());
    for (i, ci, cell_out_value, tx) in &attach_txs {
        cells[*ci].occupant = Some(*i);
        cells[*ci].outpoint = TransactionOutpoint::new(tx.id(), 0);
        cells[*ci].value = *cell_out_value;
        cells[*ci].spk = tx.outputs[0].script_public_key.clone();
        agents[*i].outpoint = TransactionOutpoint::new(tx.id(), 1);
        agents[*i].spk = tx.outputs[1].script_public_key.clone();
    }

    // --- Rounds: each agent acts; all actions in a round submit concurrently ---
    let mut latencies = Vec::new();
    for round in 1..=ROUNDS {
        let mut round_txs: Vec<(usize, String, Transaction, Vec<(usize, TxUpdate)>)> = Vec::new();
        let mut reserved: Vec<usize> = Vec::new();

        for i in 0..agents.len() {
            let (ax, ay) = (agents[i].x, agents[i].y);
            let ci = cell_idx(ax, ay);
            if reserved.contains(&ci) {
                continue;
            }
            if cells[ci].food - HARVEST >= QUOTA_FLOOR {
                // Harvest.
                let cell_prev = cell_state_of(&cells[ci], &agents, &agent_template);
                let agent_prev = lattice_capsule(agents[i].id, world_covid, ax, ay, agents[i].energy);
                let next_food = cells[ci].food - HARVEST;
                let next_energy = agents[i].energy + HARVEST;
                let cell_next = lattice_cell(ax, ay, next_food, agents[i].covid, agent_template.clone(), CAPS_DIGEST);
                let agent_next = lattice_capsule(agents[i].id, world_covid, ax, ay, next_energy);
                let observed =
                    lattice_observe_ctx("remote", utxo_of(&agents[i].spk, agents[i].value, agents[i].covid), agent_prev.clone(), agent_next);
                let cell_out_value = cells[ci].value - FEE;
                let mut outputs = vec![builder.covenant_output("Cell", cell_next, cell_out_value, 0, world_covid)?];
                outputs.extend(builder.observed_outputs(
                    "Cell",
                    "harvest",
                    "remote",
                    observed.get("remote").expect("ctx"),
                    BTreeMap::from([("agent".to_string(), agents[i].value)]),
                    1,
                    agents[i].covid,
                )?);
                let cell_sig = builder.p2sh_signature_script_with_observed_covenants(
                    "Cell",
                    "harvest",
                    cell_prev,
                    args![HARVEST],
                    &observed,
                )?;
                let agent_next2 = lattice_capsule(agents[i].id, world_covid, ax, ay, next_energy);
                let agent_sig = builder.p2sh_signature_script_in_app(
                    "open_lattice_agent",
                    "Agent",
                    "step",
                    lattice_capsule(agents[i].id, world_covid, ax, ay, agents[i].energy),
                    args![agent_next2],
                )?;
                let mut tx = TxBuilder::transaction(
                    vec![
                        input_with_budget(cells[ci].outpoint, cell_sig, COVENANT_BUDGET),
                        input_with_budget(agents[i].outpoint, agent_sig, COVENANT_BUDGET),
                    ],
                    outputs,
                );
                tx.finalize();
                reserved.push(ci);
                let updates = vec![(
                    i,
                    TxUpdate {
                        cell: ci,
                        cell_out: 0,
                        cell_value: cell_out_value,
                        cell_food: next_food,
                        agent_out: 1,
                        agent_energy: next_energy,
                        move_to: None,
                    },
                )];
                round_txs.push((i, format!("harvest {HARVEST} at ({ax},{ay})"), tx, updates));
            } else {
                // Move to the adjacent empty cell with the most food.
                let mut best: Option<(i64, i64, usize)> = None;
                for (dx, dy) in [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)] {
                    let (tx_, ty_) = (ax + dx, ay + dy);
                    if tx_ < 0 || ty_ < 0 || tx_ >= SIZE || ty_ >= SIZE {
                        continue;
                    }
                    let ti = cell_idx(tx_, ty_);
                    if cells[ti].occupant.is_some() || reserved.contains(&ti) {
                        continue;
                    }
                    if best.is_none() || cells[ti].food > cells[best.unwrap().2].food {
                        best = Some((tx_, ty_, ti));
                    }
                }
                let Some((nx, ny, ti)) = best else { continue };
                if agents[i].energy < 2 {
                    continue;
                }
                let cell_prev = cell_state_of(&cells[ci], &agents, &agent_template);
                let target_prev = cell_state_of(&cells[ti], &agents, &agent_template);
                let agent_prev = lattice_capsule(agents[i].id, world_covid, ax, ay, agents[i].energy);
                let next_energy = agents[i].energy - 1;
                let agent_next = lattice_capsule(agents[i].id, world_covid, nx, ny, next_energy);
                let source_next = lattice_cell(ax, ay, cells[ci].food, Hash::from_bytes(EMPTY_SENTINEL), EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
                let target_next = lattice_cell(nx, ny, cells[ti].food, agents[i].covid, agent_template.clone(), CAPS_DIGEST);
                let observed =
                    lattice_observe_ctx("remote", utxo_of(&agents[i].spk, agents[i].value, agents[i].covid), agent_prev.clone(), agent_next);
                let source_out_value = cells[ci].value - FEE;
                let mut outputs = builder.terminal_path_outputs(TerminalPathOutputRequest {
                    actor_name: "Cell",
                    entry_name: "move",
                    path_index: 0,
                    output_states: BTreeMap::from([
                        ("source_cell".to_string(), source_next),
                        ("target_cell".to_string(), target_next),
                    ]),
                    output_values: BTreeMap::from([
                        ("source_cell".to_string(), source_out_value),
                        ("target_cell".to_string(), cells[ti].value),
                    ]),
                    authorizing_input: 0,
                    covenant_id: world_covid,
                })?;
                outputs.extend(builder.observed_outputs(
                    "Cell",
                    "move",
                    "remote",
                    observed.get("remote").expect("ctx"),
                    BTreeMap::from([("agent".to_string(), agents[i].value)]),
                    2,
                    agents[i].covid,
                )?);
                let move_sig = builder.p2sh_signature_script_with_observed_covenants(
                    "Cell",
                    "move",
                    cell_prev,
                    args![nx - ax, ny - ay, Hash::from_bytes(EMPTY_SENTINEL), EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL],
                    &observed,
                )?;
                let host_sig = builder.p2sh_signature_script("Cell", "host_move", target_prev, args![])?;
                let step_sig = builder.p2sh_signature_script_in_app(
                    "open_lattice_agent",
                    "Agent",
                    "step",
                    agent_prev,
                    args![lattice_capsule(agents[i].id, world_covid, nx, ny, next_energy)],
                )?;
                let mut tx = TxBuilder::transaction(
                    vec![
                        input_with_budget(cells[ci].outpoint, move_sig, COVENANT_BUDGET),
                        input_with_budget(cells[ti].outpoint, host_sig, COVENANT_BUDGET),
                        input_with_budget(agents[i].outpoint, step_sig, COVENANT_BUDGET),
                    ],
                    outputs,
                );
                tx.finalize();
                reserved.push(ci);
                reserved.push(ti);
                let updates = vec![(
                    i,
                    TxUpdate {
                        cell: ci,
                        cell_out: 0,
                        cell_value: source_out_value,
                        cell_food: cells[ci].food,
                        agent_out: 2,
                        agent_energy: next_energy,
                        move_to: Some((ti, 1)),
                    },
                )];
                round_txs.push((i, format!("move ({ax},{ay}) -> ({nx},{ny})"), tx, updates));
            }
        }

        // Submit the whole round concurrently.
        let t0 = Instant::now();
        let submits = round_txs.iter().map(|(_, _, tx, _)| client.submit_transaction(tx.into(), false));
        let results = futures::future::join_all(submits).await;
        let round_ms = t0.elapsed().as_millis();
        for (k, r) in results.iter().enumerate() {
            let (i, desc, tx, updates) = &round_txs[k];
            match r {
                Ok(txid) => {
                    println!("round {round}: agent {} {desc} — {txid}", agents[*i].id);
                    log_events.push(json!({
                        "round": round,
                        "agent": agents[*i].id,
                        "action": desc,
                        "txid": txid.to_string(),
                    }));
                    for (ai, up) in updates {
                        cells[up.cell].food = up.cell_food;
                        cells[up.cell].value = up.cell_value;
                        cells[up.cell].outpoint = TransactionOutpoint::new(tx.id(), up.cell_out);
                        cells[up.cell].spk = tx.outputs[up.cell_out as usize].script_public_key.clone();
                        if let Some((ti, tout)) = up.move_to {
                            cells[up.cell].occupant = None;
                            cells[ti].occupant = Some(*ai);
                            cells[ti].outpoint = TransactionOutpoint::new(tx.id(), tout);
                            cells[ti].spk = tx.outputs[tout as usize].script_public_key.clone();
                            agents[*ai].x = cells[ti].x;
                            agents[*ai].y = cells[ti].y;
                        }
                        agents[*ai].energy = up.agent_energy;
                        agents[*ai].outpoint = TransactionOutpoint::new(tx.id(), up.agent_out);
                        agents[*ai].spk = tx.outputs[up.agent_out as usize].script_public_key.clone();
                    }
                }
                Err(e) => println!("round {round}: agent {} {desc} — REJECTED: {e}", agents[*i].id),
            }
        }
        latencies.push((round_txs.len(), round_ms));
        println!("  round {round}: {} game actions accepted concurrently in {round_ms}ms", round_txs.len());
    }

    // --- Summary + world log ----------------------------------------------------
    let total_actions: usize = latencies.iter().map(|(n, _)| n).sum();
    println!();
    println!("WORLD RUN COMPLETE — {} cells, {} agents, {} game actions over {} rounds", 64, N_AGENTS, total_actions, ROUNDS);
    for (r, (n, ms)) in latencies.iter().enumerate() {
        println!("  round {}: {} concurrent txs accepted in {}ms", r + 1, n, ms);
    }
    let log = json!({
        "network": "testnet-10",
        "world_covid": world_covid.to_string(),
        "size": SIZE,
        "agents": agents.iter().map(|a| json!({"id": a.id, "covid": a.covid.to_string(), "x": a.x, "y": a.y, "energy": a.energy})).collect::<Vec<_>>(),
        "cells": cells.iter().map(|c| json!({"x": c.x, "y": c.y, "food": c.food, "occupant": c.occupant.map(|i| agents[i].id)})).collect::<Vec<_>>(),
        "events": log_events,
    });
    std::fs::write("build/world-log.json", serde_json::to_string_pretty(&log)?)?;
    let world_state = WorldState { world_covid, size: SIZE, cells: cells.clone(), agents: agents.clone() };
    world_state.save("build/world-state.json")?;
    println!("world log: build/world-log.json");
    println!("continuation state: build/world-state.json (use `cargo run --bin play`)");
    println!("world covenant: {world_covid}");
    Ok(())
}

struct TxUpdate {
    cell: usize,
    cell_out: u32,
    cell_value: u64,
    cell_food: i64,
    agent_out: u32,
    agent_energy: i64,
    move_to: Option<(usize, u32)>,
}

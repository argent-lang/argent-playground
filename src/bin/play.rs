// Play Open Lattice on testnet-10.
//
//   cargo run --bin play            # you are agent 1
//   cargo run --bin play -- 3       # you are agent 3
//
// Commands: h = harvest 8, m n|s|e|w = move, i = info, q = quit (state saves
// after every action). Every command you enter is built, locally validated,
// and submitted to tn10 as a real covenant transaction.
//
// Continues the world in build/world-state.json (created by tn10_world).

use std::collections::BTreeMap;
use std::io::Write;

use argent::build_file;
use argent_playground::{
    CAPS_DIGEST, COVENANT_BUDGET, EMPTY_SENTINEL, EXPLORER, node_url, PlaygroundResult, WorldState, hex_bytes,
    input_with_budget, lattice_capsule, lattice_cell, lattice_observe_ctx,
};
use argent_runtime::{ArtifactBundle, TerminalPathOutputRequest, TxBuilder, args, execute_input_with_covenants};
use kaspa_consensus_core::Hash;
use kaspa_consensus_core::tx::{TransactionOutpoint, UtxoEntry};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_wrpc_client::{KaspaRpcClient, WrpcEncoding, prelude::ConnectOptions};

const STATE_FILE: &str = "build/world-state.json";
const FEE: u64 = 15_000_000;
const QUOTA_FLOOR: i64 = 20;
const HARVEST: i64 = 8;

fn utxo_of(spk: &kaspa_consensus_core::tx::ScriptPublicKey, value: u64, covid: Hash) -> UtxoEntry {
    UtxoEntry { amount: value, script_public_key: spk.clone(), block_daa_score: 0, is_coinbase: false, covenant_id: Some(covid) }
}

fn render(world: &WorldState, me: usize) {
    let size = world.size;
    println!();
    print!("     ");
    for x in 0..size {
        print!(" x={x} ");
    }
    println!();
    for y in 0..size {
        print!("y={y}  ");
        for x in 0..size {
            let c = &world.cells[(y * size + x) as usize];
            match c.occupant {
                Some(a) if a == me => print!("[*{}]", world.agents[a].id),
                Some(a) => print!("[ {}]", world.agents[a].id),
                None => print!(" {:2} ", c.food),
            }
        }
        println!();
    }
    let a = &world.agents[me];
    let cell = &world.cells[(a.y * size + a.x) as usize];
    println!();
    println!(
        "you are agent {} at ({},{}) — energy {} | cell food {} (quota floor {QUOTA_FLOOR})",
        a.id, a.x, a.y, a.energy, cell.food
    );
    println!("numbers = food on empty cells; [n] = agent n; [*n] = you");
}

#[tokio::main]
async fn main() -> PlaygroundResult<()> {
    let me_id: u8 = std::env::args().nth(1).map(|s| s.parse()).transpose()?.unwrap_or(1);
    let mut world = WorldState::load(STATE_FILE)
        .map_err(|e| format!("no live world ({e}) — run `cargo run --bin tn10_world` first"))?;
    let me = world.agents.iter().position(|a| a.id == me_id).ok_or("no such agent id")?;

    let client = KaspaRpcClient::new(WrpcEncoding::Borsh, Some(&node_url()), None, None, None)?;
    client.connect(Some(ConnectOptions::blocking_fallback())).await?;
    let info = client.get_server_info().await?;
    println!("connected: {} v{} synced={}", info.network_id, info.server_version, info.is_synced);
    println!("world covenant: {}", world.world_covid);

    let core_artifact = build_file("ag/open_lattice/core.ag", "build/open_lattice/core")?;
    let agent_artifact = build_file("ag/open_lattice/agent.ag", "build/open_lattice/agent")?;
    let bundle = ArtifactBundle::new(&core_artifact)?.with_app("open_lattice_agent", &agent_artifact)?;
    let builder = TxBuilder::from_bundle(&bundle)?;
    let agent_template =
        hex_bytes(&agent_artifact.sil_abi.contract("Agent").ok_or("Agent ABI")?.compiled.template.hash_hex)?;

    render(&world, me);
    loop {
        print!("\n[h]arvest  [m]ove n/s/e/w  [i]nfo  [q]uit > ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        match parts.as_slice() {
            ["q"] => break,
            ["i"] => render(&world, me),
            ["h"] => match harvest(&client, &builder, &agent_template, &mut world, me).await {
                Ok(txid) => {
                    println!("harvested {HARVEST} — {EXPLORER}/{txid}");
                    world.save(STATE_FILE)?;
                    render(&world, me);
                }
                Err(e) => println!("harvest refused: {e}"),
            },
            ["m", dir] => {
                let (dx, dy) = match *dir {
                    "n" => (0, -1),
                    "s" => (0, 1),
                    "e" => (1, 0),
                    "w" => (-1, 0),
                    _ => {
                        println!("direction must be n/s/e/w");
                        continue;
                    }
                };
                match do_move(&client, &builder, &agent_template, &mut world, me, dx, dy).await {
                    Ok(txid) => {
                        println!("moved — {EXPLORER}/{txid}");
                        world.save(STATE_FILE)?;
                        render(&world, me);
                    }
                    Err(e) => println!("move refused: {e}"),
                }
            }
            [] => {}
            _ => println!("commands: h | m n|s|e|w | i | q"),
        }
    }
    world.save(STATE_FILE)?;
    println!("state saved — the world persists on tn10");
    Ok(())
}

async fn harvest(
    client: &KaspaRpcClient,
    builder: &TxBuilder<'_>,
    agent_template: &[u8],
    world: &mut WorldState,
    me: usize,
) -> PlaygroundResult<String> {
    let a = world.agents[me].clone();
    let ci = (a.y * world.size + a.x) as usize;
    let cell = world.cells[ci].clone();
    if cell.food - HARVEST < QUOTA_FLOOR {
        return Err(format!("cell food {} — the quota floor ({QUOTA_FLOOR}) binds; move to a greener cell", cell.food).into());
    }

    let cell_prev = lattice_cell(a.x, a.y, cell.food, a.covid, agent_template.to_vec(), CAPS_DIGEST);
    let agent_prev = lattice_capsule(a.id, world.world_covid, a.x, a.y, a.energy);
    let cell_next = lattice_cell(a.x, a.y, cell.food - HARVEST, a.covid, agent_template.to_vec(), CAPS_DIGEST);
    let agent_next = lattice_capsule(a.id, world.world_covid, a.x, a.y, a.energy + HARVEST);

    let observed = lattice_observe_ctx("remote", utxo_of(&a.spk, a.value, a.covid), agent_prev.clone(), agent_next);
    let cell_out_value = cell.value - FEE;
    let mut outputs = vec![builder.covenant_output("Cell", cell_next, cell_out_value, 0, world.world_covid)?];
    outputs.extend(builder.observed_outputs(
        "Cell",
        "harvest",
        "remote",
        observed.get("remote").expect("ctx"),
        BTreeMap::from([("agent".to_string(), a.value)]),
        1,
        a.covid,
    )?);
    let cell_sig =
        builder.p2sh_signature_script_with_observed_covenants("Cell", "harvest", cell_prev, args![HARVEST], &observed)?;
    let agent_sig = builder.p2sh_signature_script_in_app(
        "open_lattice_agent",
        "Agent",
        "step",
        agent_prev,
        args![lattice_capsule(a.id, world.world_covid, a.x, a.y, a.energy + HARVEST)],
    )?;
    let mut tx = TxBuilder::transaction(
        vec![
            input_with_budget(cell.outpoint, cell_sig, COVENANT_BUDGET),
            input_with_budget(a.outpoint, agent_sig, COVENANT_BUDGET),
        ],
        outputs,
    );
    tx.finalize();
    let entries = vec![utxo_of(&cell.spk, cell.value, world.world_covid), utxo_of(&a.spk, a.value, a.covid)];
    execute_input_with_covenants(&tx, entries.clone(), 0)?;
    execute_input_with_covenants(&tx, entries, 1)?;
    let txid = client.submit_transaction((&tx).into(), false).await?;

    world.cells[ci].food -= HARVEST;
    world.cells[ci].value = cell_out_value;
    world.cells[ci].outpoint = TransactionOutpoint::new(tx.id(), 0);
    world.cells[ci].spk = tx.outputs[0].script_public_key.clone();
    world.agents[me].energy += HARVEST;
    world.agents[me].outpoint = TransactionOutpoint::new(tx.id(), 1);
    world.agents[me].spk = tx.outputs[1].script_public_key.clone();
    Ok(txid.to_string())
}

async fn do_move(
    client: &KaspaRpcClient,
    builder: &TxBuilder<'_>,
    agent_template: &[u8],
    world: &mut WorldState,
    me: usize,
    dx: i64,
    dy: i64,
) -> PlaygroundResult<String> {
    let a = world.agents[me].clone();
    let (nx, ny) = (a.x + dx, a.y + dy);
    if nx < 0 || ny < 0 || nx >= world.size || ny >= world.size {
        return Err("edge of the world".into());
    }
    let ci = (a.y * world.size + a.x) as usize;
    let ti = (ny * world.size + nx) as usize;
    if world.cells[ti].occupant.is_some() {
        return Err(format!("({nx},{ny}) is occupied by agent {}", world.agents[world.cells[ti].occupant.unwrap()].id).into());
    }
    if a.energy < 2 {
        return Err("not enough energy".into());
    }
    let source = world.cells[ci].clone();
    let target = world.cells[ti].clone();

    let empty_covid = Hash::from_bytes(EMPTY_SENTINEL);
    let cell_prev = lattice_cell(a.x, a.y, source.food, a.covid, agent_template.to_vec(), CAPS_DIGEST);
    let target_prev = lattice_cell(nx, ny, target.food, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
    let agent_prev = lattice_capsule(a.id, world.world_covid, a.x, a.y, a.energy);
    let agent_next = lattice_capsule(a.id, world.world_covid, nx, ny, a.energy - 1);
    let source_next = lattice_cell(a.x, a.y, source.food, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL);
    let target_next = lattice_cell(nx, ny, target.food, a.covid, agent_template.to_vec(), CAPS_DIGEST);

    let observed = lattice_observe_ctx("remote", utxo_of(&a.spk, a.value, a.covid), agent_prev.clone(), agent_next);
    let source_out_value = source.value - FEE;
    let mut outputs = builder.terminal_path_outputs(TerminalPathOutputRequest {
        actor_name: "Cell",
        entry_name: "move",
        path_index: 0,
        output_states: BTreeMap::from([("source_cell".to_string(), source_next), ("target_cell".to_string(), target_next)]),
        output_values: BTreeMap::from([("source_cell".to_string(), source_out_value), ("target_cell".to_string(), target.value)]),
        authorizing_input: 0,
        covenant_id: world.world_covid,
    })?;
    outputs.extend(builder.observed_outputs(
        "Cell",
        "move",
        "remote",
        observed.get("remote").expect("ctx"),
        BTreeMap::from([("agent".to_string(), a.value)]),
        2,
        a.covid,
    )?);
    let move_sig = builder.p2sh_signature_script_with_observed_covenants(
        "Cell",
        "move",
        cell_prev,
        args![dx, dy, empty_covid, EMPTY_SENTINEL.to_vec(), EMPTY_SENTINEL],
        &observed,
    )?;
    let host_sig = builder.p2sh_signature_script("Cell", "host_move", target_prev, args![])?;
    let step_sig = builder.p2sh_signature_script_in_app(
        "open_lattice_agent",
        "Agent",
        "step",
        agent_prev,
        args![lattice_capsule(a.id, world.world_covid, nx, ny, a.energy - 1)],
    )?;
    let mut tx = TxBuilder::transaction(
        vec![
            input_with_budget(source.outpoint, move_sig, COVENANT_BUDGET),
            input_with_budget(target.outpoint, host_sig, COVENANT_BUDGET),
            input_with_budget(a.outpoint, step_sig, COVENANT_BUDGET),
        ],
        outputs,
    );
    tx.finalize();
    let entries = vec![
        utxo_of(&source.spk, source.value, world.world_covid),
        utxo_of(&target.spk, target.value, world.world_covid),
        utxo_of(&a.spk, a.value, a.covid),
    ];
    for k in 0..3 {
        execute_input_with_covenants(&tx, entries.clone(), k)?;
    }
    let txid = client.submit_transaction((&tx).into(), false).await?;

    world.cells[ci].occupant = None;
    world.cells[ci].value = source_out_value;
    world.cells[ci].outpoint = TransactionOutpoint::new(tx.id(), 0);
    world.cells[ci].spk = tx.outputs[0].script_public_key.clone();
    world.cells[ti].occupant = Some(me);
    world.cells[ti].outpoint = TransactionOutpoint::new(tx.id(), 1);
    world.cells[ti].spk = tx.outputs[1].script_public_key.clone();
    world.agents[me].x = nx;
    world.agents[me].y = ny;
    world.agents[me].energy -= 1;
    world.agents[me].outpoint = TransactionOutpoint::new(tx.id(), 2);
    world.agents[me].spk = tx.outputs[2].script_public_key.clone();
    Ok(txid.to_string())
}

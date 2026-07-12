use std::collections::BTreeMap;

use argent::build_file;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{ArtifactBundle, ObservedCovenantContext, TxBuilder, args, execute_input_with_covenants, state};
use kaspa_consensus_core::tx::GenesisCovenantGroup;

const CORE_SOURCE: &str = "ag/open_icc_agent/core.ag";
const AGENT_SOURCE: &str = "ag/open_icc_agent/forager.ag";

fn main() -> PlaygroundResult<()> {
    let core_artifact = build_file(CORE_SOURCE, "build/open_icc_agent/core")?;
    let agent_artifact = build_file(AGENT_SOURCE, "build/open_icc_agent/agent")?;
    let bundle = ArtifactBundle::new(&core_artifact)?.with_app("open_agents", &agent_artifact)?;
    let builder = TxBuilder::from_bundle(&bundle)?;

    let cell_value = 4_000;
    let agent_value = 2_000;

    // Launch the concrete agent first so its covenant id can be stored by Cell.
    let agent_initial = state! { energy: 5 };
    let mut agent_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x81, 0), Vec::new())],
        vec![builder.genesis_output_in_app("open_agents", "Forager", agent_initial.clone(), agent_value)?],
    );
    let agent_genesis = TxBuilder::populate_genesis_covenants(&mut agent_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let agent_root = agent_genesis.output(0)?;

    let cell_initial = state! {
        agent_covid: agent_root.covenant_id,
        ticks: 0,
    };
    let mut cell_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x82, 0), Vec::new())],
        vec![builder.genesis_output("Cell", cell_initial.clone(), cell_value)?],
    );
    let cell_genesis = TxBuilder::populate_genesis_covenants(&mut cell_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let cell_root = cell_genesis.output(0)?;

    let agent_next = state! { energy: 4 };
    let cell_next = state! {
        agent_covid: agent_root.covenant_id,
        ticks: 1,
    };

    let outputs = vec![
        builder.covenant_output("Cell", cell_next, cell_value, 0, cell_root.covenant_id)?,
        builder.covenant_output_in_app("open_agents", "Forager", agent_next.clone(), agent_value, 1, agent_root.covenant_id)?,
    ];

    // `observed_agent` is runtime-bound, so the context identifies its concrete
    // app, actor, input UTXO, and before/after states.
    let observed = BTreeMap::from([(
        "remote".to_string(),
        ObservedCovenantContext::from_app("open_agents")
            .input("agent", "Forager", agent_root.utxo.clone(), agent_initial.clone())
            .output("agent", "Forager", agent_next),
    )]);

    let cell_sigscript = builder.p2sh_signature_script_with_observed_covenants("Cell", "advance", cell_initial, args![], &observed)?;
    let agent_sigscript = builder.p2sh_signature_script_in_app("open_agents", "Forager", "step", agent_initial, args![4])?;
    let tx = TxBuilder::transaction(
        vec![
            TxBuilder::transaction_input(cell_root.outpoint, cell_sigscript),
            TxBuilder::transaction_input(agent_root.outpoint, agent_sigscript),
        ],
        outputs,
    );
    let entries = vec![cell_root.utxo.clone(), agent_root.utxo.clone()];

    execute_input_with_covenants(&tx, entries.clone(), 0)?;
    execute_input_with_covenants(&tx, entries, 1)?;

    println!("built Cell::advance + Forager::step open ICC co-spend");
    println!("artifacts: build/open_icc_agent/{{core,agent}}/artifact.json");
    Ok(())
}

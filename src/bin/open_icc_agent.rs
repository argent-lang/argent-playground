use argent::build_file;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{ArtifactBundle, CovenantOutput, EntryCall, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::tx::{CovenantBinding, ScriptPublicKey, UtxoEntry};

const CORE_SOURCE: &str = "ag/open_icc_agent/core.ag";
const AGENT_SOURCE: &str = "ag/open_icc_agent/forager.ag";

fn main() -> PlaygroundResult<()> {
    let core_artifact = build_file(CORE_SOURCE, "build/open_icc_agent/core")?;
    let agent_artifact = build_file(AGENT_SOURCE, "build/open_icc_agent/agent")?;
    let bundle = ArtifactBundle::named("core", &core_artifact)?.with_app("agents", &agent_artifact)?;
    let builder = TxBuilder::from_bundle(&bundle)?;

    let cell_value = 4_000;
    let agent_value = 2_000;

    // Launch the concrete agent first so its covenant id can be stored by Cell.
    let agent_initial = state! { energy: 5 };
    let agent_funding = UtxoEntry::new(agent_value, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let agent_genesis_context = TxContext::new().input(demo_outpoint(0x81, 0), agent_funding, Vec::new(), 0).argent_genesis_output(
        0,
        "launch::forager",
        "agents::Forager",
        agent_initial.clone(),
        agent_value,
    );
    let agent_genesis_tx = builder.build(&agent_genesis_context)?;
    let agent_root = CovenantOutput::from_tx(&agent_genesis_tx, 0)?;

    let cell_initial = state! {
        agent_covid: agent_root.covenant_id,
        ticks: 0,
    };
    let cell_funding = UtxoEntry::new(cell_value, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let cell_genesis_context = TxContext::new().input(demo_outpoint(0x82, 0), cell_funding, Vec::new(), 0).argent_genesis_output(
        0,
        "launch::cell",
        "core::Cell",
        cell_initial.clone(),
        cell_value,
    );
    let cell_genesis_tx = builder.build(&cell_genesis_context)?;
    let cell_root = CovenantOutput::from_tx(&cell_genesis_tx, 0)?;

    let agent_next = state! { energy: 4 };
    let cell_next = state! {
        agent_covid: agent_root.covenant_id,
        ticks: 1,
    };

    // The complete typed transaction identifies the concrete actor behind the
    // open `actor_type<AgentCapsule>` handle.
    let context = TxContext::new()
        .argent_input("core::Cell", cell_initial, "advance", cell_root.outpoint, cell_root.utxo.clone(), 0)
        .argent_input(
            "agents::Forager",
            agent_initial,
            EntryCall::new("step").args(args![4]),
            agent_root.outpoint,
            agent_root.utxo.clone(),
            0,
        )
        .argent_output("core::Cell", cell_next, CovenantBinding::new(0, cell_root.covenant_id), cell_value)
        .argent_output("agents::Forager", agent_next, CovenantBinding::new(1, agent_root.covenant_id), agent_value);
    let tx = builder.build(&context)?;

    println!("built Cell::advance + Forager::step open ICC co-spend");
    println!("inputs: {}", tx.inputs.len());
    println!("outputs: {}", tx.outputs.len());
    println!("artifacts: build/open_icc_agent/{{core,agent}}/artifact.json");
    Ok(())
}

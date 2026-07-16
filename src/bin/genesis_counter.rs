use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{EntryCall, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::tx::{CovenantBinding, GenesisCovenantGroup};

// Same Counter app as `basic_counter`, but this demo also creates the genesis
// covenant transaction so the first spend uses the real covenant id and UTXO.
const COUNTER_APP: &str = r#"
state CounterState {
    int count;
}

actor Counter owns CounterState {
    entry bump(delta: int) emits one Counter {
        CounterState next = {
            count: count + delta,
        };

        become Counter(next);
    }
}

app CounterApp {
    actor Counter;
}
"#;

fn main() -> PlaygroundResult<()> {
    let artifact = build_inline("counter.ag", COUNTER_APP, "build/counter")?;
    let builder = TxBuilder::new(&artifact)?;

    let initial = state! { count: 2 };
    let next = state! { count: 5 };
    let input_value = 1_000;

    let mut genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x10, 0), Vec::new())],
        vec![builder.genesis_output("Counter", initial.clone(), input_value)?],
    );
    let genesis = TxBuilder::populate_genesis_covenants(&mut genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let counter_genesis = genesis.output(0)?;

    let context = TxContext::new()
        .argent_input(
            "Counter",
            initial,
            EntryCall::new("bump").args(args![3]),
            counter_genesis.outpoint,
            counter_genesis.utxo.clone(),
        )
        .argent_output("Counter", next, CovenantBinding::new(0, counter_genesis.covenant_id), input_value);
    let tx = builder.build(&context)?;

    println!("launched Counter covenant");
    println!("genesis tx: {}", genesis_tx.id());
    println!("covenant id: {}", counter_genesis.covenant_id);
    println!("built Counter::bump tx");
    println!("inputs: {}", tx.inputs.len());
    println!("outputs: {}", tx.outputs.len());
    println!("artifact: build/counter/artifact.json");
    Ok(())
}

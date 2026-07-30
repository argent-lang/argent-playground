use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{EntryCall, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::{Hash, tx::CovenantBinding};

const COUNTER_APP: &str = r#"
state CounterState {
    int count;
}

actor Counter owns CounterState {
    entry bump(int delta) emits next: Counter {
        CounterState next = {
            count: count + delta,
        };

        unrestricted(next.value);
        become next <- Counter(next);
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
    let covenant_id = Hash::from_bytes([0x42; 32]);

    let input_utxo = builder.covenant_utxo("Counter", initial.clone(), input_value, 0, false, Some(covenant_id))?;
    let context = TxContext::new()
        .actor_input("Counter", initial, EntryCall::new("bump").args(args![3]), demo_outpoint(0x11, 0), input_utxo, 0)
        .actor_output("Counter", next, CovenantBinding::new(0, covenant_id), input_value);
    let tx = builder.build(&context)?;

    println!("built Counter::bump tx");
    println!("inputs: {}", tx.inputs.len());
    println!("outputs: {}", tx.outputs.len());
    println!("artifact: build/counter/artifact.json");
    Ok(())
}

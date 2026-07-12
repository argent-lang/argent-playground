use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{TxBuilder, args, state};
use kaspa_consensus_core::Hash;

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
    let covenant_id = Hash::from_bytes([0x42; 32]);

    let input_utxo = builder.covenant_utxo("Counter", initial.clone(), input_value, 0, false, Some(covenant_id))?;
    let built = builder
        .transition("Counter", "bump", args![3])
        .input(demo_outpoint(0x11, 0), input_utxo, initial)
        .expect(next)
        .preserve_value()
        .build()?;
    let tx = built.transaction;

    println!("built Counter::bump tx");
    println!("inputs: {}", tx.inputs.len());
    println!("outputs: {}", tx.outputs.len());
    println!("artifact: build/counter/artifact.json");
    Ok(())
}

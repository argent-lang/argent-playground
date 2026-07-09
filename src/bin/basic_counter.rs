use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{TxBuilder, args, execute_input_with_covenants, state};
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
    let output = builder.covenant_output("Counter", next, input_value, 0, covenant_id)?;

    let sigscript = builder.p2sh_signature_script("Counter", "bump", initial, args![3])?;
    let tx = TxBuilder::transaction(vec![TxBuilder::transaction_input(demo_outpoint(0x11, 0), sigscript)], vec![output]);

    execute_input_with_covenants(&tx, vec![input_utxo], 0)?;

    println!("built Counter::bump tx");
    println!("inputs: {}", tx.inputs.len());
    println!("outputs: {}", tx.outputs.len());
    println!("artifact: build/counter/artifact.json");
    Ok(())
}

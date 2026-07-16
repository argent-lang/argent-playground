use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_keypair, demo_outpoint, sign_input};
use argent_runtime::{EntryCall, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::{Hash, tx::CovenantBinding};

const COUNTER_APP: &str = r#"
state CounterState {
    pubkey owner;
    int count;
}

actor Counter owns CounterState {
    entry bump(owner_sig: sig, delta: int) emits one Counter {
        require(checkSig(owner_sig, owner));

        CounterState next = {
            owner: owner,
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
    let artifact = build_inline("signed_counter.ag", COUNTER_APP, "build/signed_counter")?;
    let builder = TxBuilder::new(&artifact)?;

    let owner = demo_keypair(1);
    let owner_public_key = owner.x_only_public_key().0.serialize().to_vec();

    let initial = state! { owner: owner_public_key.clone(), count: 2 };
    let next = state! { owner: owner_public_key, count: 5 };
    let input_value = 1_000;
    let covenant_id = Hash::from_bytes([0x42; 32]);

    let input_utxo = builder.covenant_utxo("Counter", initial.clone(), input_value, 0, false, Some(covenant_id))?;
    let context = TxContext::new()
        .argent_input(
            "Counter",
            initial,
            // The signature argument is built after the transaction outputs exist.
            EntryCall::new("bump").args_with(|tx, input_idx| args![sign_input(tx, input_idx, &owner), 3]),
            demo_outpoint(0x11, 0),
            input_utxo,
        )
        .argent_output("Counter", next, CovenantBinding::new(0, covenant_id), input_value);
    let tx = builder.build(&context)?;

    println!("built signed Counter::bump tx");
    println!("inputs: {}", tx.inputs.len());
    println!("outputs: {}", tx.outputs.len());
    println!("artifact: build/signed_counter/artifact.json");
    Ok(())
}

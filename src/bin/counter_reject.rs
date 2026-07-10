// Negative-path hardening demo.
//
// Every other playground demo proves the happy path: a well-formed transition
// passes offline covenant execution. This one proves the *other* half of the
// safety claim -- that the compiled Silverscript actually rejects a transition
// that violates the state rule. `Counter::bump` enforces `next.count ==
// count + delta`; here we build an output whose count does not match and assert
// that `execute_input_with_covenants` returns an error.

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
    let artifact = build_inline("counter.ag", COUNTER_APP, "build/counter_reject")?;
    let builder = TxBuilder::new(&artifact)?;

    let initial = state! { count: 2 };
    let input_value = 1_000;
    let covenant_id = Hash::from_bytes([0x42; 32]);

    let input_utxo = builder.covenant_utxo("Counter", initial.clone(), input_value, 0, false, Some(covenant_id))?;

    // Sanity: the honest output (2 + 3 == 5) must pass, so we know the rejection
    // below is caused by the tampering and not by unrelated setup problems.
    let honest = builder.covenant_output("Counter", state! { count: 5 }, input_value, 0, covenant_id)?;
    let honest_sig = builder.p2sh_signature_script("Counter", "bump", initial.clone(), args![3])?;
    let honest_tx = TxBuilder::transaction(vec![TxBuilder::transaction_input(demo_outpoint(0x11, 0), honest_sig)], vec![honest]);
    execute_input_with_covenants(&honest_tx, vec![input_utxo.clone()], 0)?;
    println!("honest Counter::bump (2 + 3 == 5) accepted");

    // Tampered: claim delta == 3 in the sigscript but write count == 6 in the
    // output state. The covenant recomputes count + delta and must reject.
    let tampered = builder.covenant_output("Counter", state! { count: 6 }, input_value, 0, covenant_id)?;
    let tampered_sig = builder.p2sh_signature_script("Counter", "bump", initial, args![3])?;
    let tampered_tx = TxBuilder::transaction(vec![TxBuilder::transaction_input(demo_outpoint(0x11, 0), tampered_sig)], vec![tampered]);

    match execute_input_with_covenants(&tampered_tx, vec![input_utxo], 0) {
        Ok(()) => {
            return Err("SECURITY REGRESSION: tampered Counter::bump (claimed 5, wrote 6) was accepted".into());
        }
        Err(err) => {
            println!("tampered Counter::bump (claimed 5, wrote 6) correctly rejected: {err}");
        }
    }

    Ok(())
}

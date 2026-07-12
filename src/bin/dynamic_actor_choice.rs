use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{TxBuilder, actor, args, execute_input_with_covenants, state};
use kaspa_consensus_core::Hash;

// Router can become either actor in the enum. The builder names the chosen
// actor, and the runtime lowers it through the artifact.
const ROUTER_APP: &str = r#"
state RouteState {
    int nonce;
    int hops;
}

actor Alpha owns RouteState {
    entry done() emits none {
        require(hops >= 1);
    }
}

actor Beta owns RouteState {
    entry done() emits none {
        require(hops >= 1);
    }
}

actor enum Target {
    Alpha;
    Beta;
}

actor Router owns RouteState {
    entry choose(target: Target) emits one Target {
        if (target == Target::Beta) {
            require(nonce >= 0);
        }

        RouteState next = {
            nonce: nonce,
            hops: hops + 1,
        };

        become target(next);
    }
}

app RouterApp {
    actor Router;
    actor Alpha;
    actor Beta;
}
"#;

fn main() -> PlaygroundResult<()> {
    // Compile the inline Argent app and build the runtime surface from its artifact.
    let artifact = build_inline("dynamic_actor_choice.ag", ROUTER_APP, "build/dynamic_actor_choice")?;
    let builder = TxBuilder::new(&artifact)?;

    let value = 3_000;
    let covenant_id = Hash::from_bytes([0x66; 32]);

    // Router owns the current state. Alpha/Beta will own the successor state.
    let router_state = state! { nonce: 7, hops: 0 };
    let routed_state = state! { nonce: 7, hops: 1, };

    // Choose Alpha.
    let router_utxo = builder.covenant_utxo("Router", router_state.clone(), value, 0, false, Some(covenant_id))?;
    let alpha_output = builder.covenant_output("Alpha", routed_state.clone(), value, 0, covenant_id)?;
    // `choose` takes `target: Target` in the Argent source, so the user arg is
    // the actor variant name from that enum.
    let alpha_sigscript = builder.p2sh_signature_script("Router", "choose", router_state.clone(), args![actor("Alpha")])?;
    let alpha_tx =
        TxBuilder::transaction(vec![TxBuilder::transaction_input(demo_outpoint(0x31, 0), alpha_sigscript)], vec![alpha_output]);
    execute_input_with_covenants(&alpha_tx, vec![router_utxo], 0)?;

    // Same entry, same state transition, different actor choice.
    let router_utxo = builder.covenant_utxo("Router", router_state.clone(), value, 0, false, Some(covenant_id))?;
    let beta_output = builder.covenant_output("Beta", routed_state, value, 0, covenant_id)?;
    let beta_sigscript = builder.p2sh_signature_script("Router", "choose", router_state, args![actor("Beta")])?;
    let beta_tx =
        TxBuilder::transaction(vec![TxBuilder::transaction_input(demo_outpoint(0x32, 0), beta_sigscript)], vec![beta_output]);
    execute_input_with_covenants(&beta_tx, vec![router_utxo], 0)?;

    println!("built Router::choose tx: Router -> Alpha");
    println!("built Router::choose tx: Router -> Beta");
    println!("artifact: build/dynamic_actor_choice/artifact.json");
    Ok(())
}

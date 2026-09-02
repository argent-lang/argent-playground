use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{EntryCall, TxBuilder, TxContext, actor, args, state};
use kaspa_consensus_core::{Hash, tx::CovenantBinding};

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
        require(nonce >= 0);
    }
}

actor enum Target {
    Alpha;
    Beta;
}

actor Router owns RouteState {
    entry choose(Target target) emits next: Target {
        if (target == Target::Beta) {
            require(nonce >= 0);
        }

        RouteState next_state = {
            nonce: nonce,
            hops: hops + 1,
        };

        unrestricted(next.value);
        become next <- target(next_state);
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
    // `choose` takes `Target target` in the Argent source, so the user arg is
    // the actor variant name from that enum.
    let alpha_context = TxContext::new()
        .actor_input(
            "Router",
            router_state.clone(),
            EntryCall::new("choose").args(args![actor("Alpha")]),
            demo_outpoint(0x31, 0),
            router_utxo,
            0,
        )
        .actor_output("Alpha", routed_state.clone(), CovenantBinding::new(0, covenant_id), value);
    builder.build(&alpha_context)?;

    // Same entry, same state transition, different actor choice.
    let router_utxo = builder.covenant_utxo("Router", router_state.clone(), value, 0, false, Some(covenant_id))?;
    let beta_context = TxContext::new()
        .actor_input(
            "Router",
            router_state,
            EntryCall::new("choose").args(args![actor("Beta")]),
            demo_outpoint(0x32, 0),
            router_utxo,
            0,
        )
        .actor_output("Beta", routed_state, CovenantBinding::new(0, covenant_id), value);
    builder.build(&beta_context)?;

    println!("built Router::choose tx: Router -> Alpha");
    println!("built Router::choose tx: Router -> Beta");
    println!("artifact: build/dynamic_actor_choice/artifact.json");
    Ok(())
}

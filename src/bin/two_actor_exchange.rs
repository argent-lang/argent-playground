use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{CovenantOutput, TxBuilder, TxContext, state};
use kaspa_consensus_core::{Hash, tx::CovenantBinding};

// The Argent source only names the actor transitions. The route/template
// plumbing is generated and kept out of the Rust builder calls too.
const PING_PONG_APP: &str = r#"
state Turn {
    int cycles;
}

actor Ping owns Turn {
    entry send() emits one Pong {
        Turn next = {
            cycles: cycles + 1,
        };

        become Pong(next);
    }
}

actor Pong owns Turn {
    entry reply() emits one Ping {
        Turn next = {
            cycles: cycles + 1,
        };

        become Ping(next);
    }
}

app PingPongApp {
    actor Ping;
    actor Pong;
}
"#;

fn main() -> PlaygroundResult<()> {
    // Compile the inline app and load its artifact into the runtime builder.
    let artifact = build_inline("ping_pong.ag", PING_PONG_APP, "build/ping_pong")?;
    let builder = TxBuilder::new(&artifact)?;

    let value = 2_000;
    let covenant_id = Hash::from_bytes([0x55; 32]);

    // All actors share the same state type; the active actor changes by route.
    let ping_0 = state! { cycles: 0 };
    let pong_1 = state! { cycles: 1 };
    let ping_2 = state! { cycles: 2 };

    // Spend Ping and require the next output to become Pong.
    let ping_utxo = builder.covenant_utxo("Ping", ping_0.clone(), value, 0, false, Some(covenant_id))?;
    let open_context = TxContext::new().actor_input("Ping", ping_0, "send", demo_outpoint(0x21, 0), ping_utxo, 0).actor_output(
        "Pong",
        pong_1.clone(),
        CovenantBinding::new(0, covenant_id),
        value,
    );
    let open_tx = builder.build(&open_context)?;
    let pong = CovenantOutput::from_tx(&open_tx, 0)?;

    // Spend Pong back into Ping using the same covenant id.
    let close_context = TxContext::new().actor_input("Pong", pong_1, "reply", pong.outpoint, pong.utxo, 0).actor_output(
        "Ping",
        ping_2,
        CovenantBinding::new(0, covenant_id),
        value,
    );
    builder.build(&close_context)?;

    println!("built Ping::send tx: Ping -> Pong");
    println!("built Pong::reply tx: Pong -> Ping");
    println!("artifact: build/ping_pong/artifact.json");
    Ok(())
}

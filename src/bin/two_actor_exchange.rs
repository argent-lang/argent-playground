use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{TxBuilder, args, execute_input_with_covenants, state};
use kaspa_consensus_core::Hash;

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
    let pong_output = builder.covenant_output("Pong", pong_1.clone(), value, 0, covenant_id)?;
    let send_sigscript = builder.p2sh_signature_script("Ping", "send", ping_0, args![])?;
    let open_tx =
        TxBuilder::transaction(vec![TxBuilder::transaction_input(demo_outpoint(0x21, 0), send_sigscript)], vec![pong_output]);

    execute_input_with_covenants(&open_tx, vec![ping_utxo], 0)?;

    // Spend Pong back into Ping using the same covenant id.
    let pong_utxo = builder.covenant_utxo("Pong", pong_1.clone(), value, 0, false, Some(covenant_id))?;
    let ping_output = builder.covenant_output("Ping", ping_2, value, 0, covenant_id)?;
    let reply_sigscript = builder.p2sh_signature_script("Pong", "reply", pong_1, args![])?;
    let close_tx =
        TxBuilder::transaction(vec![TxBuilder::transaction_input(demo_outpoint(0x22, 0), reply_sigscript)], vec![ping_output]);

    execute_input_with_covenants(&close_tx, vec![pong_utxo], 0)?;

    println!("built Ping::send tx: {} -> {}", "Ping", "Pong");
    println!("built Pong::reply tx: {} -> {}", "Pong", "Ping");
    println!("artifact: build/ping_pong/artifact.json");
    Ok(())
}

use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{EntryCall, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::{Hash, tx::CovenantBinding};

const PAIR_APP: &str = r#"
state BoxState {
    int units;
}

actor Left owns BoxState {
    entry shift(amount: int) consumes {
        peer: Right;
    } emits {
        left_out: Left;
        peer_out: Right;
    } {
        require(amount > 0);
        require(units >= amount);

        BoxState next_left = {
            units: units - amount,
        };

        BoxState next_peer = {
            units: peer.units + amount,
        };

        require(left_out.value == self.value);
        require(peer_out.value == peer.value);

        become {
            left_out <- Left(next_left);
            peer_out <- Right(next_peer);
        };
    }
}

actor Right owns BoxState {
    delegate accept_shift() consumes {
        leader: Left;
    } {
        require(leader.units >= 0);
    }
}

app PairApp {
    actor Left;
    actor Right;
}
"#;

fn main() -> PlaygroundResult<()> {
    let artifact = build_inline("paired_transfer.ag", PAIR_APP, "build/paired_transfer")?;
    let builder = TxBuilder::new(&artifact)?;

    let covenant_id = Hash::from_bytes([0x66; 32]);
    let left_value = 3_000;
    let right_value = 2_000;

    let left_initial = state! { units: 10 };
    let right_initial = state! { units: 1 };
    let left_next = state! { units: 7 };
    let right_next = state! { units: 4 };

    let left_outpoint = demo_outpoint(0x61, 0);
    let right_outpoint = demo_outpoint(0x62, 0);
    let left_utxo = builder.covenant_utxo("Left", left_initial.clone(), left_value, 0, false, Some(covenant_id))?;
    let right_utxo = builder.covenant_utxo("Right", right_initial.clone(), right_value, 0, false, Some(covenant_id))?;

    let context = TxContext::new()
        .argent_input("Left", left_initial, EntryCall::new("shift").args(args![3]), left_outpoint, left_utxo, 0)
        .argent_input("Right", right_initial, "accept_shift", right_outpoint, right_utxo, 0)
        .argent_output("Left", left_next, CovenantBinding::new(0, covenant_id), left_value)
        .argent_output("Right", right_next, CovenantBinding::new(0, covenant_id), right_value);
    let tx = builder.build(&context)?;

    println!("built Left::shift + Right::accept_shift 2:2 tx");
    println!("inputs: {}", tx.inputs.len());
    println!("outputs: {}", tx.outputs.len());
    println!("artifact: build/paired_transfer/artifact.json");
    Ok(())
}

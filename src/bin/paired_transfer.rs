use argent::build_inline;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{TxBuilder, args, execute_input_with_covenants, state};
use kaspa_consensus_core::Hash;

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
    let entries = vec![
        builder.covenant_utxo("Left", left_initial.clone(), left_value, 0, false, Some(covenant_id))?,
        builder.covenant_utxo("Right", right_initial.clone(), right_value, 0, false, Some(covenant_id))?,
    ];

    // Left::shift authorizes both recreated actors, so both outputs bind to
    // input 0. Their order matches the transaction this demo chooses to build.
    let outputs = vec![
        builder.covenant_output("Left", left_next, left_value, 0, covenant_id)?,
        builder.covenant_output("Right", right_next, right_value, 0, covenant_id)?,
    ];

    let leader_sigscript = builder.p2sh_signature_script("Left", "shift", left_initial, args![3])?;
    let delegate_sigscript = builder.p2sh_signature_script("Right", "accept_shift", right_initial, args![])?;
    let tx = TxBuilder::transaction(
        vec![
            TxBuilder::transaction_input(left_outpoint, leader_sigscript),
            TxBuilder::transaction_input(right_outpoint, delegate_sigscript),
        ],
        outputs,
    );

    execute_input_with_covenants(&tx, entries.clone(), 0)?;
    execute_input_with_covenants(&tx, entries, 1)?;

    println!("built Left::shift + Right::accept_shift 2:2 tx");
    println!("artifact: build/paired_transfer/artifact.json");
    Ok(())
}

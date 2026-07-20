use argent::build_file;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{ArtifactBundle, CovenantOutput, EntryCall, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::tx::{CovenantBinding, ScriptPublicKey, UtxoEntry};

const ASSET_SOURCE: &str = "ag/multiapp_badge/badge_asset.ag";
const CONTROLLER_SOURCE: &str = "ag/multiapp_badge/badge_controller.ag";

// This demo is WIP as we perfect Argent's devx.
fn main() -> PlaygroundResult<()> {
    // Compile both apps independently, then bundle the controller with the
    // observed asset app.
    let asset_artifact = build_file(ASSET_SOURCE, "build/multiapp_badge/asset")?;
    let controller_artifact = build_file(CONTROLLER_SOURCE, "build/multiapp_badge/controller")?;
    let bundle = ArtifactBundle::named("badge_controller", &controller_artifact)?.with_app("badge_asset", &asset_artifact)?;
    let builder = TxBuilder::from_bundle(&bundle)?;

    let controller_value = 4_000;
    let badge_value = 2_000;

    // Launch the controller first. Its covenant id will become the authority
    // stored inside the badge covenant state.
    let controller_initial = state! { minted: 0 };
    let controller_funding = UtxoEntry::new(controller_value, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let controller_genesis_context = TxContext::new()
        .input(demo_outpoint(0x70, 0), controller_funding, Vec::new(), 0)
        .actor_genesis_output(0, "launch::controller", "badge_controller::Controller", controller_initial.clone(), controller_value);
    let controller_genesis_tx = builder.build(&controller_genesis_context)?;
    let controller_root = CovenantOutput::from_tx(&controller_genesis_tx, 0)?;

    // Launch the observed asset app second. The Badge state now commits to the
    // controller covenant id, so Badge::apply can require controller co-spend.
    let badge_initial = state! {
        controller_id: controller_root.covenant_id,
        balance: 10,
    };
    let badge_funding = UtxoEntry::new(badge_value, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let badge_genesis_context = TxContext::new().input(demo_outpoint(0x71, 0), badge_funding, Vec::new(), 0).actor_genesis_output(
        0,
        "launch::badge",
        "badge_asset::Badge",
        badge_initial.clone(),
        badge_value,
    );
    let badge_genesis_tx = builder.build(&badge_genesis_context)?;
    let badge_root = CovenantOutput::from_tx(&badge_genesis_tx, 0)?;

    let amount = 7;
    let controller_next = state! { minted: amount };
    let badge_next = state! {
        controller_id: controller_root.covenant_id,
        balance: 10 + amount,
    };

    let context = TxContext::new()
        .actor_input(
            "badge_controller::Controller",
            controller_initial,
            EntryCall::new("mint").args(args![badge_root.covenant_id, amount]),
            controller_root.outpoint,
            controller_root.utxo.clone(),
            0,
        )
        .actor_input(
            "badge_asset::Badge",
            badge_initial,
            EntryCall::new("apply").args(args![10 + amount]),
            badge_root.outpoint,
            badge_root.utxo.clone(),
            0,
        )
        .actor_output(
            "badge_controller::Controller",
            controller_next,
            CovenantBinding::new(0, controller_root.covenant_id),
            controller_value,
        )
        .actor_output("badge_asset::Badge", badge_next, CovenantBinding::new(1, badge_root.covenant_id), badge_value);
    let tx = builder.build(&context)?;

    println!("controller genesis tx: {}", controller_genesis_tx.id());
    println!("controller covenant id: {}", controller_root.covenant_id);
    println!("badge genesis tx: {}", badge_genesis_tx.id());
    println!("badge covenant id: {}", badge_root.covenant_id);
    println!("built Controller::mint + Badge::apply co-spend");
    println!("inputs: {}", tx.inputs.len());
    println!("outputs: {}", tx.outputs.len());
    println!("artifacts: build/multiapp_badge/{{controller,asset}}/artifact.json");
    Ok(())
}

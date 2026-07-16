use argent::build_file;
use argent_playground::{PlaygroundResult, demo_outpoint};
use argent_runtime::{ArtifactBundle, EntryCall, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::tx::{CovenantBinding, GenesisCovenantGroup};

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
    let mut controller_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x70, 0), Vec::new())],
        vec![builder.genesis_output("badge_controller::Controller", controller_initial.clone(), controller_value)?],
    );
    let controller_genesis =
        TxBuilder::populate_genesis_covenants(&mut controller_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let controller_root = controller_genesis.output(0)?;

    // Launch the observed asset app second. The Badge state now commits to the
    // controller covenant id, so Badge::apply can require controller co-spend.
    let badge_initial = state! {
        controller_id: controller_root.covenant_id,
        balance: 10,
    };
    let mut badge_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x71, 0), Vec::new())],
        vec![builder.genesis_output("badge_asset::Badge", badge_initial.clone(), badge_value)?],
    );
    let badge_genesis = TxBuilder::populate_genesis_covenants(&mut badge_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let badge_root = badge_genesis.output(0)?;

    let amount = 7;
    let controller_next = state! { minted: amount };
    let badge_next = state! {
        controller_id: controller_root.covenant_id,
        balance: 10 + amount,
    };

    let context = TxContext::new()
        .argent_input(
            "badge_controller::Controller",
            controller_initial,
            EntryCall::new("mint").args(args![badge_root.covenant_id, amount]),
            controller_root.outpoint,
            controller_root.utxo.clone(),
        )
        .argent_input(
            "badge_asset::Badge",
            badge_initial,
            EntryCall::new("apply").args(args![10 + amount]),
            badge_root.outpoint,
            badge_root.utxo.clone(),
        )
        .argent_output(
            "badge_controller::Controller",
            controller_next,
            CovenantBinding::new(0, controller_root.covenant_id),
            controller_value,
        )
        .argent_output("badge_asset::Badge", badge_next, CovenantBinding::new(1, badge_root.covenant_id), badge_value);
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

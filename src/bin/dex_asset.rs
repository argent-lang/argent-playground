use argent::build_file;
use argent_playground::{PlaygroundResult, demo_keypair, demo_outpoint};
use argent_runtime::{ArtifactBundle, ArtifactValue, ObservedCovenantContext, TxBuilder, args, state};
use kaspa_consensus_core::{
    Hash,
    tx::{GenesisCovenantGroup, TransactionId, TransactionOutpoint},
};
use std::collections::BTreeMap;

const ASSET_SOURCE: &str = "ag/dex_asset/asset.ag";
const DEX_SOURCE: &str = "ag/dex_asset/dex.ag";
const OWNER_RESERVE: i64 = 1;

fn main() -> PlaygroundResult<()> {
    let asset_artifact = build_file(ASSET_SOURCE, "build/dex_asset/asset")?;
    let dex_artifact = build_file(DEX_SOURCE, "build/dex_asset/dex")?;
    let bundle = ArtifactBundle::new(&dex_artifact)?.with_app("asset", &asset_artifact)?;
    let builder = TxBuilder::from_bundle(&bundle)?;

    let dex_value = 4_000;
    let asset_value = 2_000;
    let empty_covid = Hash::from_bytes([0; 32]);
    let empty_actor_type = vec![0; 32];

    // The DEX starts unbound. Its covenant id becomes the reserve owner used
    // when the concrete asset is launched next.
    let dex_initial = state! {
        reserve_covid: empty_covid,
        reserve_type: empty_actor_type,
        traded: 0,
    };
    let mut dex_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x91, 0), Vec::new())],
        vec![builder.genesis_output("Dex", dex_initial.clone(), dex_value)?],
    );
    let dex_genesis = TxBuilder::populate_genesis_covenants(&mut dex_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let dex_root = dex_genesis.output(0)?;

    // ReserveAsset has another entry that routes to WalletAsset. Its external
    // capsule handle therefore differs from its canonical in-app template.
    let reserve_type = builder.actor_type_handle_in_app("asset", "ReserveAsset", "AssetCapsule")?;
    let owner_key = demo_keypair(0x19).x_only_public_key().0.serialize().to_vec();
    let asset_id = vec![0xa5; 32];
    let reserve_initial = state! {
        owner_kind: OWNER_RESERVE,
        owner_key: owner_key.clone(),
        reserve_owner: dex_root.covenant_id,
        policy: state! {
            asset_id: asset_id.clone(),
            sequence: 0,
        },
        units: 100,
    };
    let mut asset_genesis_tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(0x92, 0), Vec::new())],
        vec![builder.genesis_output_in_app("asset", "ReserveAsset", reserve_initial.clone(), asset_value)?],
    );
    let asset_genesis = TxBuilder::populate_genesis_covenants(&mut asset_genesis_tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let reserve_root = asset_genesis.output(0)?;

    // Bind the DEX to this reserve through a real open-ICC 2:2 transaction.
    // The reserve stays 1:1 and advances its private policy sequence.
    let dex_bound = state! {
        reserve_covid: reserve_root.covenant_id,
        reserve_type: reserve_type.clone(),
        traded: 0,
    };
    let reserve_bound = reserve_state(&owner_key, dex_root.covenant_id, &asset_id, 1, 100);
    let bind_observed = ObservedCovenantContext::from_app("asset")
        .input("asset", "ReserveAsset", reserve_root.utxo.clone(), reserve_initial.clone())
        .output("asset", "ReserveAsset", reserve_bound.clone());
    let bind = builder
        .transition("Dex", "bind")
        .args(args![reserve_root.covenant_id, reserve_type.clone()])
        .input(dex_root.outpoint, dex_root.utxo.clone(), dex_initial)
        .observe("reserve", bind_observed)
        .expect(dex_bound.clone())
        .preserve_value()
        .co_spend_observed("reserve", "asset", "settle", reserve_root.outpoint, args![100], asset_value)
        .build()?;

    let bind_txid = bind.transaction.id();
    let bound_dex_utxo = builder.covenant_utxo("Dex", dex_bound.clone(), dex_value, 0, false, Some(dex_root.covenant_id))?;
    let bound_reserve_utxo = builder.covenant_utxo_in_app(
        "asset",
        "ReserveAsset",
        reserve_bound.clone(),
        asset_value,
        0,
        false,
        Some(reserve_root.covenant_id),
    )?;

    // A trade is another 2:2 transition. The DEX constrains the reserve delta;
    // ReserveAsset::settle enforces that its covenant owner is co-spent.
    let amount = 7;
    let dex_next = state! {
        reserve_covid: reserve_root.covenant_id,
        reserve_type: reserve_type.clone(),
        traded: amount,
    };
    let reserve_next = reserve_state(&owner_key, dex_root.covenant_id, &asset_id, 2, 100 - amount);
    let trade_observed = ObservedCovenantContext::from_app("asset")
        .input("asset", "ReserveAsset", bound_reserve_utxo, reserve_bound)
        .output("asset", "ReserveAsset", reserve_next);
    let trade = builder
        .transition("Dex", "trade")
        .args(args![amount])
        .input(demo_outpoint_from_tx(bind_txid, 0), bound_dex_utxo, dex_bound)
        .observe("reserve", trade_observed)
        .expect(dex_next)
        .preserve_value()
        .co_spend_observed("reserve", "asset", "settle", demo_outpoint_from_tx(bind_txid, 1), args![100 - amount], asset_value)
        .build()?;

    println!("DEX covenant id: {}", dex_root.covenant_id);
    println!("reserve covenant id: {}", reserve_root.covenant_id);
    println!("external reserve handle: {} bytes", reserve_type.len());
    println!("bind tx: {}", bind_txid);
    println!("trade tx: {}", trade.transaction.id());
    println!("artifacts: build/dex_asset/{{dex,asset}}/artifact.json");
    Ok(())
}

fn reserve_state(
    owner_key: &[u8],
    reserve_owner: Hash,
    asset_id: &[u8],
    sequence: i64,
    units: i64,
) -> BTreeMap<String, ArtifactValue> {
    state! {
        owner_kind: OWNER_RESERVE,
        owner_key: owner_key.to_vec(),
        reserve_owner: reserve_owner,
        policy: state! {
            asset_id: asset_id.to_vec(),
            sequence: sequence,
        },
        units: units,
    }
}

fn demo_outpoint_from_tx(transaction_id: TransactionId, index: u32) -> TransactionOutpoint {
    TransactionOutpoint { transaction_id, index }
}

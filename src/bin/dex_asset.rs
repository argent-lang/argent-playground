use std::collections::BTreeMap;

use argent::build_file;
use argent_playground::{PlaygroundResult, demo_keypair, demo_outpoint, sign_input};
use argent_runtime::{ArtifactBundle, ArtifactValue, ObservedCovenantContext, TxBuilder, args, state};
use kaspa_consensus_core::{
    Hash,
    tx::{GenesisCovenantGroup, Transaction, TransactionOutpoint, UtxoEntry},
};

const CORE_SOURCE: &str = "ag/dex/dex_core.ag";
const PAIR_SOURCE: &str = "ag/dex/dex_pair.ag";
const QUOTE_SOURCE: &str = "ag/dex/mintable_asset.ag";
const BASE_SOURCE: &str = "ag/dex/kas_asset.ag";

const OWNER_KEY: u8 = 0;
const OWNER_COVID: u8 = 1;
const CORE_VALUE: u64 = 4_000;
const PAIR_VALUE: u64 = 3_000;
const MINTER_VALUE: u64 = 2_000;
const QUOTE_VALUE: u64 = 2_000;
const QUOTE_AMOUNT: i64 = 20_000;
const BASE_AMOUNT: i64 = 10_000;

struct LaunchedActor {
    tx: Transaction,
    outpoint: TransactionOutpoint,
    utxo: UtxoEntry,
    covenant_id: Hash,
}

#[derive(Clone)]
struct PairConfig {
    initializer: Vec<u8>,
    core_id: Hash,
    core_type: Vec<u8>,
    quote_id: Hash,
    quote_type: Vec<u8>,
    base_id: Hash,
    base_type: Vec<u8>,
}

fn main() -> PlaygroundResult<()> {
    let core_artifact = build_file(CORE_SOURCE, "build/dex/core")?;
    let pair_artifact = build_file(PAIR_SOURCE, "build/dex/pair")?;
    let quote_artifact = build_file(QUOTE_SOURCE, "build/dex/mintable_asset")?;
    let base_artifact = build_file(BASE_SOURCE, "build/dex/kas_asset")?;

    let core_bundle = ArtifactBundle::new(&core_artifact)?.with_app("dex_pair_app", &pair_artifact)?;
    let core_builder = TxBuilder::from_bundle(&core_bundle)?;
    let pair_bundle =
        ArtifactBundle::new(&pair_artifact)?.with_app("mintable_asset", &quote_artifact)?.with_app("kas_asset", &base_artifact)?;
    let pair_builder = TxBuilder::from_bundle(&pair_bundle)?;
    let quote_builder = TxBuilder::new(&quote_artifact)?;
    let base_builder = TxBuilder::new(&base_artifact)?;

    let governor = demo_keypair(0x31);
    let pair_initializer = demo_keypair(0x32);
    let issuer = demo_keypair(0x33);
    let trader = demo_keypair(0x34);
    let governor_key = governor.x_only_public_key().0.serialize().to_vec();
    let pair_initializer_key = pair_initializer.x_only_public_key().0.serialize().to_vec();
    let issuer_key = issuer.x_only_public_key().0.serialize().to_vec();
    let trader_key = trader.x_only_public_key().0.serialize().to_vec();

    let core_type = core_builder.actor_type_handle("DexCore", "DexCoreCapsule")?;
    let pair_type = core_builder.actor_type_handle_in_app("dex_pair_app", "DexPair", "DexPairState")?;
    let quote_type = pair_builder.actor_type_handle_in_app("mintable_asset", "MintableToken", "AssetCapsule")?;
    let base_type = pair_builder.actor_type_handle_in_app("kas_asset", "KasToken", "AssetCapsule")?;

    // Core starts with an empty four-slot registry. Its virtual `pairs` field
    // stores the digest while Rust supplies the typed PairRegistry preimage.
    let core_initial = core_state(&governor_key, &core_type, &pair_type, vec![0; 384], 0);
    let core_root = launch_actor(&core_builder, "DexCore", core_initial.clone(), CORE_VALUE, 0xa0)?;

    // MintableToken is the quote asset. Its Minter route gives the token's
    // external AssetCapsule handle real route-template context.
    let asset_id = vec![0x51; 32];
    let minter_initial = mintable_state(&issuer_key, OWNER_KEY, &asset_id, QUOTE_AMOUNT, QUOTE_AMOUNT);
    let minter_root = launch_actor(&quote_builder, "Minter", minter_initial.clone(), MINTER_VALUE, 0xa1)?;
    let minter_next = mintable_state(&issuer_key, OWNER_KEY, &asset_id, QUOTE_AMOUNT, 0);
    let quote_payment = mintable_state(&trader_key, OWNER_KEY, &asset_id, QUOTE_AMOUNT, QUOTE_AMOUNT);
    let mint = quote_builder
        .transition("Minter", "mint")
        .input(minter_root.outpoint, minter_root.utxo.clone(), minter_initial)
        .output("minter", minter_next, MINTER_VALUE)
        .output("token", quote_payment.clone(), QUOTE_VALUE)
        .args_with(|tx, input_idx| args![trader_key.clone(), OWNER_KEY, QUOTE_AMOUNT, sign_input(tx, input_idx, &issuer)])
        .build()?;
    let quote_payment_outpoint = output_outpoint(&mint.transaction, 1);
    let quote_payment_utxo =
        quote_builder.covenant_utxo("MintableToken", quote_payment.clone(), QUOTE_VALUE, 0, false, Some(minter_root.covenant_id))?;

    // The base asset is canonical wrapped KAS: state amount and native output
    // value are equal. It starts under the trader's key and is funded into the
    // Pair after that Pair receives its covenant id.
    let base_initial = kas_state(&trader_key, OWNER_KEY, BASE_AMOUNT);
    let base_root = launch_actor(&base_builder, "KasToken", base_initial.clone(), BASE_AMOUNT as u64, 0xa2)?;

    let pair_config = PairConfig {
        initializer: pair_initializer_key,
        core_id: core_root.covenant_id,
        core_type: core_type.clone(),
        quote_id: minter_root.covenant_id,
        quote_type,
        base_id: base_root.covenant_id,
        base_type,
    };
    let pair_initial = pair_state(&pair_config, false, 0, 0);
    let pair_root = launch_actor(&pair_builder, "DexPair", pair_initial.clone(), PAIR_VALUE, 0xa3)?;

    // Registration is a 2:2 ICC transition. Core verifies the full Pair state
    // through pair_type, while DexPair::activate independently signs the same
    // transaction. The launch group is assumed to have been audited as noted
    // in the Argent source.
    let pair_active = pair_state(&pair_config, true, 0, 0);
    let mut registry_records = vec![0; 384];
    registry_records[0..32].copy_from_slice(&pair_root.covenant_id.as_bytes());
    registry_records[32..64].copy_from_slice(&minter_root.covenant_id.as_bytes());
    registry_records[64..96].copy_from_slice(&base_root.covenant_id.as_bytes());
    let core_registered = core_state(&governor_key, &core_type, &pair_type, registry_records, 1);
    let pair_context = ObservedCovenantContext::from_app("dex_pair_app")
        .input("pair", "DexPair", pair_root.utxo.clone(), pair_initial.clone())
        .output("pair", "DexPair", pair_active.clone());
    let register = core_builder
        .transition("DexCore", "register_pair")
        .input(core_root.outpoint, core_root.utxo.clone(), core_initial)
        .observe("candidate", pair_context)
        .expect(core_registered)
        .preserve_value()
        .args_with(|tx, input_idx| args![pair_root.covenant_id, sign_input(tx, input_idx, &governor)])
        .co_spend_observed_with(
            "candidate",
            "pair",
            "activate",
            pair_root.outpoint,
            |tx, input_idx| args![sign_input(tx, input_idx, &pair_initializer)],
            PAIR_VALUE,
        )
        .build()?;
    let pair_active_outpoint = output_outpoint(&register.transaction, 1);
    let pair_active_utxo =
        pair_builder.covenant_utxo("DexPair", pair_active.clone(), PAIR_VALUE, 0, false, Some(pair_root.covenant_id))?;

    // Fund the Pair's KAS reserve through the normal asset transfer entry.
    let pair_owner = pair_root.covenant_id.as_bytes().to_vec();
    let base_reserve = kas_state(&pair_owner, OWNER_COVID, BASE_AMOUNT);
    let fund_reserve = base_builder
        .transition("KasToken", "transfer")
        .input(base_root.outpoint, base_root.utxo.clone(), base_initial)
        .expect(base_reserve.clone())
        .value(BASE_AMOUNT as u64)
        .args_with(|tx, input_idx| args![pair_owner.clone(), OWNER_COVID, BASE_AMOUNT, sign_input(tx, input_idx, &trader)])
        .build()?;
    let base_reserve_outpoint = output_outpoint(&fund_reserve.transaction, 0);
    let base_reserve_utxo =
        base_builder.covenant_utxo("KasToken", base_reserve.clone(), BASE_AMOUNT as u64, 0, false, Some(base_root.covenant_id))?;

    // The actual swap is Pair + quote payment + base reserve -> Pair + quote
    // reserve + base payout. The Pair constrains the atomic exchange while each
    // asset input separately authorizes its own transfer.
    let quote_reserve = mintable_state(&pair_owner, OWNER_COVID, &asset_id, QUOTE_AMOUNT, QUOTE_AMOUNT);
    let base_payout = kas_state(&trader_key, OWNER_KEY, BASE_AMOUNT);
    let quote_context = ObservedCovenantContext::from_app("mintable_asset")
        .input("payment", "MintableToken", quote_payment_utxo, quote_payment)
        .output("reserve", "MintableToken", quote_reserve);
    let base_context = ObservedCovenantContext::from_app("kas_asset")
        .input("reserve", "KasToken", base_reserve_utxo, base_reserve)
        .output("payout", "KasToken", base_payout);
    let pair_after_swap = pair_state(&pair_config, true, 1, 0);
    let swap = pair_builder
        .transition("DexPair", "swap")
        .input(pair_active_outpoint, pair_active_utxo, pair_active)
        .observe("quote", quote_context)
        .observe("base", base_context)
        .expect(pair_after_swap)
        .preserve_value()
        .co_spend_observed_with(
            "quote",
            "payment",
            "transfer",
            quote_payment_outpoint,
            |tx, input_idx| args![pair_root.covenant_id, OWNER_COVID, sign_input(tx, input_idx, &trader)],
            QUOTE_VALUE,
        )
        .co_spend_observed(
            "base",
            "reserve",
            "transfer",
            base_reserve_outpoint,
            args![trader_key, OWNER_KEY, BASE_AMOUNT, vec![0; 65]],
            BASE_AMOUNT as u64,
        )
        .build()?;

    println!("core covenant id: {}", core_root.covenant_id);
    println!("pair covenant id: {}", pair_root.covenant_id);
    println!("quote covenant id: {}", minter_root.covenant_id);
    println!("base KAS covenant id: {}", base_root.covenant_id);
    println!("core genesis tx: {}", core_root.tx.id());
    println!("pair registration tx: {}", register.transaction.id());
    println!("reserve funding tx: {}", fund_reserve.transaction.id());
    println!("swap tx: {}", swap.transaction.id());
    println!("swap shape: {} inputs -> {} outputs", swap.transaction.inputs.len(), swap.transaction.outputs.len());
    println!("artifacts: build/dex/{{core,pair,mintable_asset,kas_asset}}/artifact.json");
    Ok(())
}

fn launch_actor(
    builder: &TxBuilder<'_>,
    actor: &str,
    state: BTreeMap<String, ArtifactValue>,
    value: u64,
    funding_byte: u8,
) -> PlaygroundResult<LaunchedActor> {
    let mut tx = TxBuilder::transaction(
        vec![TxBuilder::transaction_input(demo_outpoint(funding_byte, 0), Vec::new())],
        vec![builder.genesis_output(actor, state, value)?],
    );
    let genesis = TxBuilder::populate_genesis_covenants(&mut tx, &[GenesisCovenantGroup::new(0, vec![0])])?;
    let output = genesis.output(0)?;
    Ok(LaunchedActor { outpoint: output.outpoint, utxo: output.utxo.clone(), covenant_id: output.covenant_id, tx })
}

fn output_outpoint(tx: &Transaction, index: u32) -> TransactionOutpoint {
    TransactionOutpoint { transaction_id: tx.id(), index }
}

fn core_state(governor: &[u8], core_type: &[u8], pair_type: &[u8], records: Vec<u8>, count: i64) -> BTreeMap<String, ArtifactValue> {
    state! {
        governor: governor.to_vec(),
        core_type: core_type.to_vec(),
        pair_type: pair_type.to_vec(),
        pairs: state! {
            records: records,
            count: count,
        },
    }
}

fn pair_state(config: &PairConfig, initialized: bool, swaps: i64, reserve_moves: i64) -> BTreeMap<String, ArtifactValue> {
    state! {
        initializer: config.initializer.clone(),
        initialized: initialized,
        core_id: config.core_id,
        core_type: config.core_type.clone(),
        quote_id: config.quote_id,
        quote_type: config.quote_type.clone(),
        base_id: config.base_id,
        base_type: config.base_type.clone(),
        quote_per_base_num: 2,
        quote_per_base_den: 1,
        swaps: swaps,
        reserve_moves: reserve_moves,
    }
}

fn mintable_state(owner: &[u8], owner_kind: u8, asset_id: &[u8], max_supply: i64, amount: i64) -> BTreeMap<String, ArtifactValue> {
    state! {
        owner: owner.to_vec(),
        owner_kind: owner_kind,
        policy: state! {
            asset_id: asset_id.to_vec(),
            max_supply: max_supply,
        },
        amount: amount,
    }
}

fn kas_state(owner: &[u8], owner_kind: u8, amount: i64) -> BTreeMap<String, ArtifactValue> {
    state! {
        owner: owner.to_vec(),
        owner_kind: owner_kind,
        policy: state! { network: 0u8 },
        amount: amount,
    }
}

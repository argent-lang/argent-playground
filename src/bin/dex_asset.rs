use std::collections::BTreeMap;

use argent::{build_file, build_file_app};
use argent_playground::{PlaygroundResult, demo_keypair, demo_outpoint, sign_input};
use argent_runtime::{
    ActorPath, ArtifactBundle, ArtifactValue, CovenantOutput, EntryCall, TxBuilder, TxContext, args, state, state_with,
};
use kaspa_consensus_core::{
    Hash,
    tx::{CovenantBinding, ScriptPublicKey, Transaction, TransactionOutpoint, UtxoEntry},
};
use secp256k1::Keypair;

const DEX_SOURCE: &str = "ag/dex/dex.ag";
const M_ASSET_SOURCE: &str = "ag/dex/mintable_asset.ag";
const K_ASSET_SOURCE: &str = "ag/dex/kas_asset.ag";

const OWNER_KEY: u8 = 0;
const OWNER_COVID: u8 = 1;
const REGISTRY_RECORDS_BYTES: usize = 384;
const CORE_VALUE: u64 = 4_000;
const PAIR_VALUE: u64 = 3_000;
const MINTER_VALUE: u64 = 2_000;
const QUOTE_VALUE: u64 = 2_000;
const QUOTE_AMOUNT: i64 = 20_000;
const BASE_AMOUNT: i64 = 10_000;
const ASSET_C_SUPPLY: i64 = 30_000;

struct LaunchedActor {
    tx: Transaction,
    output: CovenantOutput,
}

impl LaunchedActor {
    fn outpoint(&self) -> TransactionOutpoint {
        self.output.outpoint
    }

    fn utxo(&self) -> &UtxoEntry {
        &self.output.utxo
    }

    fn covenant_id(&self) -> Hash {
        self.output.covenant_id
    }
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

impl PairConfig {
    /// Builds the signed `register_pair` call for this pair configuration.
    fn register_call<'a>(&'a self, governor: &'a Keypair) -> EntryCall<'a> {
        EntryCall::new("register_pair").args_with(move |tx, input_idx| {
            args![
                self.initializer.clone(),
                self.quote_id,
                self.quote_type.clone(),
                self.base_id,
                self.base_type.clone(),
                2,
                1,
                sign_input(tx, input_idx, governor)
            ]
        })
    }

    /// Encodes this pair as the registry row `[pair_id, quote_id, base_id]`.
    fn record(&self, pair_id: Hash) -> [Hash; 3] {
        [pair_id, self.quote_id, self.base_id]
    }
}

/*
DEX demo story
==============

`MAsset` is a mintable asset implementation used to launch two distinct assets,
A and C, each with its own covenant id. `KAsset` is a separate implementation
representing native KAS as asset B.

Two pair covenants are then born directly from successive `DexCore`
registration spends:

    DexCore registry
        |
        +-- A/B pair: MAsset A <-> KAsset B
        |
        +-- A/C pair: MAsset A <-> MAsset C

The demo executes three atomic transitions:

    mint A
        Minter A
            -> Minter A + A(trader)

    swap on A/B
        Pair A/B + A(trader) + B(Pair A/B)
            -> Pair A/B + A(Pair A/B) + B(trader)

    move A reserve from A/B to A/C
        Pair A/B + DexCore + A(Pair A/B)
            -> Pair A/B + DexCore + A(Pair A/C)

The last tx uses DexCore to verify that A/C is a registered pair for asset A.
Pair A/B observes both the unchanged Core state and the A reserve transfer. The
asset independently authorizes the spend because its current owner, Pair A/B,
is co-spent.
*/
fn main() -> PlaygroundResult<()> {
    // 1. Compile every app and give each one the same short name used by actor
    // paths below. Dexc and Dexp come from one source file but remain separate
    // apps and covenant identities.
    let core_artifact = build_file_app(DEX_SOURCE, "Dexc", "build/dex/core")?;
    let pair_artifact = build_file_app(DEX_SOURCE, "Dexp", "build/dex/pair")?;
    let m_asset_artifact = build_file(M_ASSET_SOURCE, "build/dex/m_asset")?;
    let k_asset_artifact = build_file(K_ASSET_SOURCE, "build/dex/k_asset")?;

    let bundle = ArtifactBundle::named("dexc", &core_artifact)?
        .with_app("dexp", &pair_artifact)?
        .with_app("m_asset", &m_asset_artifact)?
        .with_app("k_asset", &k_asset_artifact)?;
    let builder = TxBuilder::from_bundle(&bundle)?;

    let governor = demo_keypair(0x31);
    let ab_initializer = demo_keypair(0x32);
    let issuer = demo_keypair(0x33);
    let trader = demo_keypair(0x34);
    let ac_initializer = demo_keypair(0x35);
    let governor_key = governor.x_only_public_key().0.serialize().to_vec();
    let ab_initializer_key = ab_initializer.x_only_public_key().0.serialize().to_vec();
    let issuer_key = issuer.x_only_public_key().0.serialize().to_vec();
    let trader_key = trader.x_only_public_key().0.serialize().to_vec();
    let ac_initializer_key = ac_initializer.x_only_public_key().0.serialize().to_vec();

    let core_type = builder.actor_type_handle("dexc::DexCore", "DexCoreCapsule")?;
    let pair_type = builder.actor_type_handle("dexp::DexPair", "DexPairState")?;
    let m_asset_type = builder.actor_type_handle("m_asset::MintableToken", "AssetCapsule")?;
    let k_asset_type = builder.actor_type_handle("k_asset::KasToken", "AssetCapsule")?;

    // 2. Launch the Core with an empty four-slot registry. The authored Core
    // state carries PairRegistry, while its external capsule exposes a digest.
    let core_initial = core_state(&governor_key, &core_type, &pair_type, vec![0; 384], 0);
    let core_root = launch_actor(&builder, "dexc::DexCore", core_initial.clone(), CORE_VALUE, 0xa0)?;

    // 3. Launch asset A's MAsset minter and mint its entire supply to the
    // trader. Minter -> Minter + MintableToken gives this app a real route
    // table even though the DEX only observes the token actor.
    let asset_a_id = vec![0x51; 32];
    let minter_initial = m_asset_state(&issuer_key, OWNER_KEY, &asset_a_id, QUOTE_AMOUNT, QUOTE_AMOUNT);
    let asset_a_root = launch_actor(&builder, "m_asset::Minter", minter_initial.clone(), MINTER_VALUE, 0xa1)?;
    let minter_next = m_asset_state(&issuer_key, OWNER_KEY, &asset_a_id, QUOTE_AMOUNT, 0);
    let quote_payment = m_asset_state(&trader_key, OWNER_KEY, &asset_a_id, QUOTE_AMOUNT, QUOTE_AMOUNT);
    let mint_context = TxContext::new()
        .actor_input(
            "m_asset::Minter",
            minter_initial,
            EntryCall::new("mint")
                .args_with(|tx, input_idx| args![trader_key.clone(), OWNER_KEY, QUOTE_AMOUNT, sign_input(tx, input_idx, &issuer)]),
            asset_a_root.outpoint(),
            asset_a_root.utxo().clone(),
            0,
        )
        .actor_output("m_asset::Minter", minter_next, CovenantBinding::new(0, asset_a_root.covenant_id()), MINTER_VALUE)
        .actor_output(
            "m_asset::MintableToken",
            quote_payment.clone(),
            CovenantBinding::new(0, asset_a_root.covenant_id()),
            QUOTE_VALUE,
        );
    let mint = builder.build(&mint_context)?;
    let quote_payment_output = CovenantOutput::from_tx(&mint, 1)?;

    // 4. Launch B as canonical KAS and C as a second MAsset deployment. A and
    // C share code and actor types but have distinct asset covenant ids.
    let base_initial = k_asset_state(&trader_key, OWNER_KEY, BASE_AMOUNT);
    let asset_b_root = launch_actor(&builder, "k_asset::KasToken", base_initial.clone(), BASE_AMOUNT as u64, 0xa2)?;
    let asset_c_id = vec![0x52; 32];
    let asset_c_initial = m_asset_state(&issuer_key, OWNER_KEY, &asset_c_id, ASSET_C_SUPPLY, ASSET_C_SUPPLY);
    let asset_c_root = launch_actor(&builder, "m_asset::Minter", asset_c_initial, MINTER_VALUE, 0xa3)?;

    // 5. Describe the A/B and A/C pairs. They do not exist yet: each Pair will
    // be genesis-created by the Core transaction that registers it.
    let ab_config = PairConfig {
        initializer: ab_initializer_key,
        core_id: core_root.covenant_id(),
        core_type: core_type.clone(),
        quote_id: asset_a_root.covenant_id(),
        quote_type: m_asset_type.clone(),
        base_id: asset_b_root.covenant_id(),
        base_type: k_asset_type,
    };
    let ac_config = PairConfig {
        initializer: ac_initializer_key,
        core_id: core_root.covenant_id(),
        core_type: core_type.clone(),
        quote_id: asset_a_root.covenant_id(),
        quote_type: m_asset_type.clone(),
        base_id: asset_c_root.covenant_id(),
        base_type: m_asset_type,
    };

    // 6. Register and birth A/B in one 1:2 transition. The Pair output is the
    // explicit `new_pair` spawn. Its state is static, so the builder can derive
    // the Pair id before resolving the Core continuation that records that id.
    let ab_active = pair_state(&ab_config, true, 0, 0);
    let register_ab_context = TxContext::new()
        .actor_input(
            "dexc::DexCore",
            core_initial,
            ab_config.register_call(&governor),
            core_root.outpoint(),
            core_root.utxo().clone(),
            0,
        )
        .actor_output(
            "dexc::DexCore",
            state_with(|state_context| {
                let pair_id = state_context.genesis_covenant_id(0, "spawn::new_pair").expect("passed as genesis group");
                let records = registry_records(&[ab_config.record(pair_id)]);
                core_state(&governor_key, &core_type, &pair_type, records, 1)
            }),
            CovenantBinding::new(0, core_root.covenant_id()),
            CORE_VALUE,
        )
        .actor_genesis_output(0, "spawn::new_pair", "dexp::DexPair", ab_active.clone(), PAIR_VALUE);
    let register_ab = builder.build(&register_ab_context)?;
    let ab_pair = CovenantOutput::from_tx(&register_ab, 1)?;
    let ab_pair_id = ab_pair.covenant_id;
    let records_once = registry_records(&[ab_config.record(ab_pair_id)]);
    let core_registered_once = core_state(&governor_key, &core_type, &pair_type, records_once, 1);
    let core_once = CovenantOutput::from_tx(&register_ab, 0)?;

    // 7. The successor Core births A/C the same way. The final registry
    // preimage now has two records and is the proof consumed by reserve movement.
    let ac_active = pair_state(&ac_config, true, 0, 0);
    let register_ac_context = TxContext::new()
        .actor_input("dexc::DexCore", core_registered_once, ac_config.register_call(&governor), core_once.outpoint, core_once.utxo, 0)
        .actor_output(
            "dexc::DexCore",
            state_with(|state_context| {
                let pair_id = state_context.genesis_covenant_id(0, "spawn::new_pair").expect("passed as genesis group");
                let records = registry_records(&[ab_config.record(ab_pair_id), ac_config.record(pair_id)]);
                core_state(&governor_key, &core_type, &pair_type, records, 2)
            }),
            CovenantBinding::new(0, core_root.covenant_id()),
            CORE_VALUE,
        )
        .actor_genesis_output(0, "spawn::new_pair", "dexp::DexPair", ac_active, PAIR_VALUE);
    let register_ac = builder.build(&register_ac_context)?;
    let ac_pair_id = CovenantOutput::from_tx(&register_ac, 1)?.covenant_id;
    let records = registry_records(&[ab_config.record(ab_pair_id), ac_config.record(ac_pair_id)]);
    let core_registered = core_state(&governor_key, &core_type, &pair_type, records.clone(), 2);
    let core_registered_output = CovenantOutput::from_tx(&register_ac, 0)?;

    // 8. Fund A/B's KAS reserve through KAsset's normal transfer entry. The
    // complete lot becomes covenant-owned by the A/B pair.
    let ab_owner = ab_pair_id.as_bytes().to_vec();
    let base_reserve = k_asset_state(&ab_owner, OWNER_COVID, BASE_AMOUNT);
    let fund_reserve_context = TxContext::new()
        .actor_input(
            "k_asset::KasToken",
            base_initial,
            EntryCall::new("transfer")
                .args_with(|tx, input_idx| args![ab_owner.clone(), OWNER_COVID, BASE_AMOUNT, sign_input(tx, input_idx, &trader)]),
            asset_b_root.outpoint(),
            asset_b_root.utxo().clone(),
            0,
        )
        .actor_output(
            "k_asset::KasToken",
            base_reserve.clone(),
            CovenantBinding::new(0, asset_b_root.covenant_id()),
            BASE_AMOUNT as u64,
        );
    let fund_reserve = builder.build(&fund_reserve_context)?;
    let base_reserve_output = CovenantOutput::from_tx(&fund_reserve, 0)?;

    // 9. Swap the trader's A lot for the complete B reserve. DexPair constrains
    // the ratio and successor ownership; each asset input authorizes itself.
    let quote_reserve = m_asset_state(&ab_owner, OWNER_COVID, &asset_a_id, QUOTE_AMOUNT, QUOTE_AMOUNT);
    let base_payout = k_asset_state(&trader_key, OWNER_KEY, BASE_AMOUNT);
    let ab_after_swap = pair_state(&ab_config, true, 1, 0);
    let swap_context = TxContext::new()
        .actor_input("dexp::DexPair", ab_active, "swap", ab_pair.outpoint, ab_pair.utxo, 0)
        .actor_input(
            "m_asset::MintableToken",
            quote_payment,
            EntryCall::new("transfer").args_with(|tx, input_idx| args![ab_pair_id, OWNER_COVID, sign_input(tx, input_idx, &trader)]),
            quote_payment_output.outpoint,
            quote_payment_output.utxo,
            0,
        )
        .actor_input(
            "k_asset::KasToken",
            base_reserve,
            EntryCall::new("transfer").args(args![trader_key.clone(), OWNER_KEY, BASE_AMOUNT, vec![0; 65]]),
            base_reserve_output.outpoint,
            base_reserve_output.utxo,
            0,
        )
        .actor_output("dexp::DexPair", ab_after_swap.clone(), CovenantBinding::new(0, ab_pair_id), PAIR_VALUE)
        .actor_output(
            "m_asset::MintableToken",
            quote_reserve.clone(),
            CovenantBinding::new(1, asset_a_root.covenant_id()),
            QUOTE_VALUE,
        )
        .actor_output("k_asset::KasToken", base_payout, CovenantBinding::new(2, asset_b_root.covenant_id()), BASE_AMOUNT as u64);
    let swap = builder.build(&swap_context)?;

    // 10. Move A from the A/B reserve to the registered A/C pair. The Pair
    // observes Core and the A transfer; Core itself remains unchanged. MAsset
    // separately sees its current A/B owner co-spent and permits the transfer.
    // The asset covenant id selects the reserve side; the observed input binds
    // the concrete asset implementation used by the unified move entry.
    let ab_after_swap_output = CovenantOutput::from_tx(&swap, 0)?;
    let quote_reserve_output = CovenantOutput::from_tx(&swap, 1)?;
    let moved_quote = m_asset_state(&ac_pair_id.as_bytes(), OWNER_COVID, &asset_a_id, QUOTE_AMOUNT, QUOTE_AMOUNT);
    let ab_after_move = pair_state(&ab_config, true, 1, 1);
    let move_reserve_context = TxContext::new()
        .actor_input(
            "dexp::DexPair",
            ab_after_swap,
            EntryCall::new("move_reserve").args(args![asset_a_root.covenant_id(), ac_pair_id, 1, registry_preimage(&records, 2)]),
            ab_after_swap_output.outpoint,
            ab_after_swap_output.utxo,
            0,
        )
        .actor_input(
            "dexc::DexCore",
            core_registered.clone(),
            "witness_registry",
            core_registered_output.outpoint,
            core_registered_output.utxo,
            0,
        )
        .actor_input(
            "m_asset::MintableToken",
            quote_reserve,
            EntryCall::new("transfer").args(args![ac_pair_id, OWNER_COVID, vec![0; 65]]),
            quote_reserve_output.outpoint,
            quote_reserve_output.utxo,
            0,
        )
        .actor_output("dexp::DexPair", ab_after_move, CovenantBinding::new(0, ab_pair_id), PAIR_VALUE)
        .actor_output("dexc::DexCore", core_registered, CovenantBinding::new(1, core_root.covenant_id()), CORE_VALUE)
        .actor_output("m_asset::MintableToken", moved_quote, CovenantBinding::new(2, asset_a_root.covenant_id()), QUOTE_VALUE);
    let move_reserve = builder.build(&move_reserve_context)?;

    println!("core covenant id: {}", core_root.covenant_id());
    println!("A/B pair covenant id: {ab_pair_id}");
    println!("A/C pair covenant id: {ac_pair_id}");
    println!("asset A covenant id: {}", asset_a_root.covenant_id());
    println!("asset B (KAS) covenant id: {}", asset_b_root.covenant_id());
    println!("asset C covenant id: {}", asset_c_root.covenant_id());
    println!("core genesis tx: {}", core_root.tx.id());
    println!("A/B registration tx: {}", register_ab.id());
    println!("A/C registration tx: {}", register_ac.id());
    println!("reserve funding tx: {}", fund_reserve.id());
    println!("swap tx: {}", swap.id());
    println!("swap shape: {} inputs -> {} outputs", swap.inputs.len(), swap.outputs.len());
    println!("reserve move tx: {}", move_reserve.id());
    println!("reserve move shape: {} inputs -> {} outputs", move_reserve.inputs.len(), move_reserve.outputs.len());
    println!("artifacts: build/dex/{{core,pair,m_asset,k_asset}}/artifact.json");
    Ok(())
}

fn launch_actor(
    builder: &TxBuilder<'_>,
    actor: impl Into<ActorPath>,
    state: BTreeMap<String, ArtifactValue>,
    value: u64,
    funding_byte: u8,
) -> PlaygroundResult<LaunchedActor> {
    let actor = actor.into();
    let funding_utxo = UtxoEntry::new(value, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let context = TxContext::new().input(demo_outpoint(funding_byte, 0), funding_utxo, Vec::new(), 0).actor_genesis_output(
        0,
        "launch::actor",
        actor,
        state,
        value,
    );
    let tx = builder.build(&context)?;
    let output = CovenantOutput::from_tx(&tx, 0)?;
    Ok(LaunchedActor { tx, output })
}

fn registry_records(records: &[[Hash; 3]]) -> Vec<u8> {
    assert!(records.len() <= 4, "demo registry has four slots");
    records
        .iter()
        .flat_map(|record| record.iter())
        .flat_map(|id| id.as_bytes())
        .chain(std::iter::repeat(0))
        .take(REGISTRY_RECORDS_BYTES)
        .collect()
}

fn registry_preimage(records: &[u8], count: i64) -> Vec<u8> {
    assert_eq!(records.len(), REGISTRY_RECORDS_BYTES);
    records.iter().copied().chain(count.to_le_bytes()).collect()
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

fn m_asset_state(owner: &[u8], owner_kind: u8, asset_id: &[u8], max_supply: i64, amount: i64) -> BTreeMap<String, ArtifactValue> {
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

fn k_asset_state(owner: &[u8], owner_kind: u8, amount: i64) -> BTreeMap<String, ArtifactValue> {
    state! {
        owner: owner.to_vec(),
        owner_kind: owner_kind,
        policy: state! { network: 0u8 },
        amount: amount,
    }
}

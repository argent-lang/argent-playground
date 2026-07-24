use std::collections::BTreeMap;

use argent_runtime::{actor, args, state, Artifact, ArtifactValue, BuilderResult, CovenantOutput, EntryCall, TxBuilder, TxContext};
use blake2b_simd::Params as Blake2bParams;
use kaspa_consensus_core::{
    hashing::{
        sighash::{calc_schnorr_signature_hash, SigHashReusedValuesUnsync},
        sighash_type::SIG_HASH_ALL,
    },
    tx::{CovenantBinding, MutableTransaction, ScriptPublicKey, Transaction, TransactionId, TransactionOutpoint, UtxoEntry},
    Hash,
};
use secp256k1::{Keypair, Message, Secp256k1, SecretKey};

const LIVE: i64 = 0;
const WWIN: i64 = 1;
const BWIN: i64 = 2;
const DRAW: i64 = 3;
const WHITE: i64 = 0;
const BLACK: i64 = 1;
const CLEAR: i64 = 0;
const OFFER: i64 = 1;
const CLAIM: i64 = 2;
const SURRENDER: i64 = 3;
const ACCEPT: i64 = 4;
const CLAIMED: i64 = 1;
const DEFENSE: i64 = 2;
const NORMAL: i64 = 3;
const WOFFER: i64 = 4;
const MOVE_TIMEOUT: i64 = 600;
const GAME_VALUE: u64 = 1_000;
const BASE_RATING: i64 = 1_200;

struct TestPlayer {
    keypair: Keypair,
    public_key: Vec<u8>,
    owner: [u8; 32],
    player_id: [u8; 32],
    player_ref: [u8; 32],
}

#[derive(Clone)]
struct PlayerStateData {
    owner: [u8; 32],
    player_id: [u8; 32],
    open_games: i64,
    rating: i64,
    games: i64,
    wins: i64,
    draws: i64,
    losses: i64,
}

struct StartedGame {
    leader_state: PlayerStateData,
    leader_output: CovenantOutput,
    other_state: PlayerStateData,
    other_output: CovenantOutput,
    game_state: GameStateData,
    game_output: CovenantOutput,
}

struct SettledGame {
    white_state: PlayerStateData,
    white_output: CovenantOutput,
    black_state: PlayerStateData,
    black_output: CovenantOutput,
}

struct ExpectedSettlement {
    white_state: PlayerStateData,
    white_value: u64,
    black_state: PlayerStateData,
    black_value: u64,
}

struct SettlementFixture {
    white_state: PlayerStateData,
    white_output: CovenantOutput,
    black_state: PlayerStateData,
    black_output: CovenantOutput,
    settlement: CovenantOutput,
}

impl PlayerStateData {
    fn registered(player: &TestPlayer) -> Self {
        Self {
            owner: player.owner,
            player_id: player.player_id,
            open_games: 0,
            rating: BASE_RATING,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        }
    }

    fn source_state(&self) -> BTreeMap<String, ArtifactValue> {
        state! {
            owner: self.owner,
            player_id: self.player_id,
            open_games: self.open_games,
            rating: self.rating,
            games: self.games,
            wins: self.wins,
            draws: self.draws,
            losses: self.losses,
        }
    }
}

#[derive(Clone)]
struct GameStateData {
    white_player: [u8; 32],
    black_player: [u8; 32],
    board: Vec<u8>,
    turn: i64,
    status: i64,
    move_timeout: i64,
    castle_rights: [u8; 4],
    en_passant_idx: i64,
    pending_src_idx: i64,
    pending_dst_idx: i64,
    pending_promo: i64,
    recent_castle: i64,
    draw_state: i64,
}

impl GameStateData {
    fn live(white_player: [u8; 32], black_player: [u8; 32], board: Vec<u8>) -> Self {
        Self {
            white_player,
            black_player,
            board,
            turn: WHITE,
            status: LIVE,
            move_timeout: MOVE_TIMEOUT,
            castle_rights: [1; 4],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: NORMAL,
        }
    }

    fn committed_route(&self, target: &str, mv: MoveSpec, termination_action: i64) -> Self {
        let mut next = self.clone();
        next.pending_src_idx = mv.source_idx();
        next.pending_dst_idx = mv.destination_idx();
        next.pending_promo = mv.promo_piece;
        if next.draw_state > NORMAL {
            next.draw_state = NORMAL;
        }
        if termination_action == OFFER {
            next.draw_state = WOFFER + self.turn;
        }
        if target != "CastleChallengePrep" {
            next.recent_castle = CLEAR;
        }
        next
    }

    fn completed_move(&self, mv: MoveSpec) -> Self {
        let mut next = self.clone();
        let piece = next.board[mv.source_idx() as usize];
        next.board[mv.source_idx() as usize] = 0;
        next.board[mv.destination_idx() as usize] = piece;
        next.turn = 1 - self.turn;
        next.en_passant_idx = -1;
        next.pending_src_idx = -1;
        next.pending_dst_idx = -1;
        next.pending_promo = 0;
        next.recent_castle = 0;
        next
    }

    fn source_state(&self) -> BTreeMap<String, ArtifactValue> {
        state! {
            white_player: self.white_player,
            black_player: self.black_player,
            board: self.board.clone(),
            turn: self.turn,
            status: self.status,
            move_timeout: self.move_timeout,
            castle_rights: self.castle_rights,
            en_passant_idx: self.en_passant_idx,
            pending_src_idx: self.pending_src_idx,
            pending_dst_idx: self.pending_dst_idx,
            pending_promo: self.pending_promo,
            recent_castle: self.recent_castle,
            draw_state: self.draw_state,
        }
    }
}

#[derive(Clone, Copy)]
struct MoveSpec {
    from_x: i64,
    from_y: i64,
    to_x: i64,
    to_y: i64,
    promo_piece: i64,
}

impl MoveSpec {
    fn new(from_x: i64, from_y: i64, to_x: i64, to_y: i64) -> Self {
        Self { from_x, from_y, to_x, to_y, promo_piece: 0 }
    }

    fn with_promotion(from_x: i64, from_y: i64, to_x: i64, to_y: i64, promo_piece: i64) -> Self {
        Self { from_x, from_y, to_x, to_y, promo_piece }
    }

    fn source_idx(self) -> i64 {
        self.from_y * 8 + self.from_x
    }

    fn destination_idx(self) -> i64 {
        self.to_y * 8 + self.to_x
    }
}

fn chess_artifact() -> Artifact {
    serde_json::from_str(include_str!("../../build/artifact.json")).expect("pinned chess artifact deserializes")
}

fn blake2b32(bytes: &[u8]) -> [u8; 32] {
    Blake2bParams::new().hash_length(32).to_state().update(bytes).finalize().as_bytes().try_into().expect("Blake2b output is 32 bytes")
}

fn player(seed: u8) -> TestPlayer {
    let secret = SecretKey::from_slice(&[seed; 32]).expect("deterministic secret key is valid");
    let keypair = Keypair::from_secret_key(&Secp256k1::new(), &secret);
    let public_key = keypair.x_only_public_key().0.serialize().to_vec();
    let player_id = blake2b32(&[b"argent-chess-player".as_slice(), public_key.as_slice()].concat());
    let owner = blake2b32(&public_key);
    let player_ref = blake2b32(&[owner.as_slice(), player_id.as_slice()].concat());
    TestPlayer { keypair, public_key, owner, player_id, player_ref }
}

fn player_with_id(seed: u8, player_id: [u8; 32]) -> TestPlayer {
    let mut player = player(seed);
    player.player_id = player_id;
    player.player_ref = blake2b32(&[player.owner.as_slice(), player.player_id.as_slice()].concat());
    player
}

fn sign_input<T: AsRef<Transaction>>(tx: &MutableTransaction<T>, input_index: usize, keypair: &Keypair) -> Vec<u8> {
    let reused_values = SigHashReusedValuesUnsync::new();
    let sighash = calc_schnorr_signature_hash(&tx.as_verifiable(), input_index, SIG_HASH_ALL, &reused_values);
    let signature = keypair.sign_schnorr(Message::from_digest(sighash.as_bytes()));
    let mut encoded = signature.as_ref().to_vec();
    encoded.push(SIG_HASH_ALL.to_u8());
    encoded
}

fn opening_board() -> Vec<u8> {
    [
        [0x04, 0x02, 0x03, 0x05, 0x06, 0x03, 0x02, 0x04],
        [0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01],
        [0x00; 8],
        [0x00; 8],
        [0x00; 8],
        [0x00; 8],
        [0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09],
        [0x0c, 0x0a, 0x0b, 0x0d, 0x0e, 0x0b, 0x0a, 0x0c],
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn execute_worker_round_trip(
    builder: &TxBuilder<'_>,
    player: &TestPlayer,
    worker: &str,
    initial: &GameStateData,
    mv: MoveSpec,
    expected: &GameStateData,
    fixture_tag: u8,
) {
    let (worker_state, worker_output) = route_to_worker(builder, player, worker, initial, mv, fixture_tag);
    execute_actor_transition(builder, worker, &worker_state, "apply", worker_output, "Mux", expected);
}

fn execute_worker_from_mux(
    builder: &TxBuilder<'_>,
    player: &TestPlayer,
    worker: &str,
    initial: &GameStateData,
    mux_output: CovenantOutput,
    mv: MoveSpec,
    expected: &GameStateData,
) -> CovenantOutput {
    let (worker_state, worker_output) = route_mux_output_to_worker(builder, player, worker, initial, mux_output, mv, CLEAR);
    execute_actor_transition(builder, worker, &worker_state, "apply", worker_output, "Mux", expected)
}

fn route_to_worker(
    builder: &TxBuilder<'_>,
    player: &TestPlayer,
    worker: &str,
    initial: &GameStateData,
    mv: MoveSpec,
    fixture_tag: u8,
) -> (GameStateData, CovenantOutput) {
    route_to_worker_with_action(builder, player, worker, initial, mv, CLEAR, fixture_tag)
}

fn route_to_worker_with_action(
    builder: &TxBuilder<'_>,
    player: &TestPlayer,
    worker: &str,
    initial: &GameStateData,
    mv: MoveSpec,
    termination_action: i64,
    fixture_tag: u8,
) -> (GameStateData, CovenantOutput) {
    let covenant_id = Hash::from_bytes([fixture_tag; 32]);
    let mux_state = initial.source_state();
    let mux_outpoint = TransactionOutpoint::new(TransactionId::from_bytes([fixture_tag.wrapping_add(1); 32]), 0);
    let mux_utxo = builder
        .covenant_utxo("Mux", mux_state.clone(), GAME_VALUE, 0, false, Some(covenant_id))
        .unwrap_or_else(|err| panic!("{worker} mux UTXO must build: {err}"));
    route_mux_output_to_worker(
        builder,
        player,
        worker,
        initial,
        CovenantOutput { index: 0, outpoint: mux_outpoint, utxo: mux_utxo, covenant_id },
        mv,
        termination_action,
    )
}

fn route_mux_output_to_worker(
    builder: &TxBuilder<'_>,
    player: &TestPlayer,
    worker: &str,
    initial: &GameStateData,
    mux_output: CovenantOutput,
    mv: MoveSpec,
    termination_action: i64,
) -> (GameStateData, CovenantOutput) {
    let covenant_id = mux_output.covenant_id;
    let output_value = mux_output.utxo.amount;
    let mux_state = initial.source_state();
    let worker_state = initial.committed_route(worker, mv, termination_action);
    let selected_worker = worker.to_string();
    let keypair = player.keypair;
    let public_key = player.public_key.clone();
    let player_id = player.player_id;

    let route_context = TxContext::new()
        .actor_input(
            "Mux",
            mux_state,
            EntryCall::new("route").args_with(move |tx, input_index| {
                args![
                    actor(selected_worker.clone()),
                    mv.from_x,
                    mv.from_y,
                    mv.to_x,
                    mv.to_y,
                    mv.promo_piece,
                    termination_action,
                    sign_input(tx, input_index, &keypair),
                    public_key.clone(),
                    player_id,
                ]
            }),
            mux_output.outpoint,
            mux_output.utxo,
            0,
        )
        .actor_output(worker, worker_state.source_state(), CovenantBinding::new(0, covenant_id), output_value);
    let route_tx = builder.build(&route_context).unwrap_or_else(|err| panic!("mux must route a signed move to {worker}: {err}"));
    let worker_output = CovenantOutput::from_tx(&route_tx, 0).expect("route output is a covenant UTXO");
    (worker_state, worker_output)
}

fn assert_mux_route_rejected(
    builder: &TxBuilder<'_>,
    player: &TestPlayer,
    worker: &str,
    initial: &GameStateData,
    mv: MoveSpec,
    output_value: u64,
    fixture_tag: u8,
) {
    let covenant_id = Hash::from_bytes([fixture_tag; 32]);
    let mux_state = initial.source_state();
    let mux_outpoint = TransactionOutpoint::new(TransactionId::from_bytes([fixture_tag.wrapping_add(1); 32]), 0);
    let mux_utxo = builder
        .covenant_utxo("Mux", mux_state.clone(), GAME_VALUE, 0, false, Some(covenant_id))
        .expect("mux rejection fixture must build");
    let worker_state = initial.committed_route(worker, mv, CLEAR);
    let selected_worker = worker.to_string();
    let keypair = player.keypair;
    let public_key = player.public_key.clone();
    let player_id = player.player_id;
    let context = TxContext::new()
        .actor_input(
            "Mux",
            mux_state,
            EntryCall::new("route").args_with(move |tx, input_index| {
                args![
                    actor(selected_worker.clone()),
                    mv.from_x,
                    mv.from_y,
                    mv.to_x,
                    mv.to_y,
                    mv.promo_piece,
                    CLEAR,
                    sign_input(tx, input_index, &keypair),
                    public_key.clone(),
                    player_id,
                ]
            }),
            mux_outpoint,
            mux_utxo,
            0,
        )
        .actor_output(worker, worker_state.source_state(), CovenantBinding::new(0, covenant_id), output_value);
    assert!(builder.build(&context).is_err(), "Mux must reject the proposed route to {worker}");
}

fn execute_actor_transition<'a>(
    builder: &TxBuilder<'_>,
    source_actor: &str,
    source_state: &GameStateData,
    entry: impl Into<EntryCall<'a>>,
    source_output: CovenantOutput,
    target_actor: &str,
    target_state: &GameStateData,
) -> CovenantOutput {
    let context = TxContext::new()
        .actor_input(source_actor, source_state.source_state(), entry, source_output.outpoint, source_output.utxo, 0)
        .actor_output(target_actor, target_state.source_state(), CovenantBinding::new(0, source_output.covenant_id), GAME_VALUE);
    let tx = builder.build(&context).unwrap_or_else(|err| panic!("{source_actor} must transition to {target_actor}: {err}"));
    CovenantOutput::from_tx(&tx, 0).expect("actor transition output is a covenant UTXO")
}

fn assert_actor_transition_rejected<'a>(
    builder: &TxBuilder<'_>,
    source_actor: &str,
    source_state: &GameStateData,
    entry: impl Into<EntryCall<'a>>,
    source_output: &CovenantOutput,
    target_actor: &str,
    target_state: &GameStateData,
) {
    let context = TxContext::new()
        .actor_input(source_actor, source_state.source_state(), entry, source_output.outpoint, source_output.utxo.clone(), 0)
        .actor_output(target_actor, target_state.source_state(), CovenantBinding::new(0, source_output.covenant_id), GAME_VALUE);
    assert!(builder.build(&context).is_err(), "{source_actor} must reject the proposed transition to {target_actor}");
}

fn execute_mux_terminate(
    builder: &TxBuilder<'_>,
    player: &TestPlayer,
    initial: &GameStateData,
    termination_action: i64,
    expected: &GameStateData,
    fixture_tag: u8,
) -> CovenantOutput {
    let covenant_id = Hash::from_bytes([fixture_tag; 32]);
    let mux_state = initial.source_state();
    let outpoint = TransactionOutpoint::new(TransactionId::from_bytes([fixture_tag.wrapping_add(1); 32]), 0);
    let utxo = builder
        .covenant_utxo("Mux", mux_state.clone(), GAME_VALUE, 0, false, Some(covenant_id))
        .expect("mux UTXO builds from source state");
    terminate_mux_output(
        builder,
        player,
        initial,
        termination_action,
        expected,
        CovenantOutput { index: 0, covenant_id, outpoint, utxo },
    )
}

fn terminate_mux_output(
    builder: &TxBuilder<'_>,
    player: &TestPlayer,
    initial: &GameStateData,
    termination_action: i64,
    expected: &GameStateData,
    mux_output: CovenantOutput,
) -> CovenantOutput {
    let covenant_id = mux_output.covenant_id;
    let output_value = mux_output.utxo.amount;
    let mux_state = initial.source_state();
    let keypair = player.keypair;
    let public_key = player.public_key.clone();
    let player_id = player.player_id;
    let context = TxContext::new()
        .actor_input(
            "Mux",
            mux_state,
            EntryCall::new("terminate").args_with(move |tx, input_index| {
                args![termination_action, sign_input(tx, input_index, &keypair), public_key.clone(), player_id]
            }),
            mux_output.outpoint,
            mux_output.utxo,
            0,
        )
        .actor_output("Mux", expected.source_state(), CovenantBinding::new(0, covenant_id), output_value);
    let tx = builder.build(&context).unwrap_or_else(|err| panic!("mux terminate action {termination_action} must execute: {err}"));
    CovenantOutput::from_tx(&tx, 0).expect("mux termination output is a covenant UTXO")
}

fn settle_state(white_player: [u8; 32], black_player: [u8; 32], status: i64) -> BTreeMap<String, ArtifactValue> {
    state! {
        white_player: white_player,
        black_player: black_player,
        status: status,
    }
}

fn league_state(admin: [u8; 32]) -> BTreeMap<String, ArtifactValue> {
    state! {
        base_rating: BASE_RATING,
        admin: admin,
    }
}

fn launch_league(builder: &TxBuilder<'_>, state: BTreeMap<String, ArtifactValue>, value: u64) -> CovenantOutput {
    let funding_outpoint = TransactionOutpoint::new(TransactionId::from_bytes([0xa1; 32]), 0);
    let funding_utxo = UtxoEntry::new(value, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
    let context = TxContext::new().input(funding_outpoint, funding_utxo, Vec::new(), 0).actor_genesis_output(
        0,
        "launch::league",
        "League",
        state,
        value,
    );
    let tx = builder.build(&context).expect("league genesis transaction executes");
    CovenantOutput::from_tx(&tx, 0).expect("league genesis output is a covenant UTXO")
}

fn actor_fixture(
    builder: &TxBuilder<'_>,
    actor: &str,
    state: BTreeMap<String, ArtifactValue>,
    value: u64,
    fixture_tag: u8,
) -> CovenantOutput {
    let covenant_id = Hash::from_bytes([fixture_tag; 32]);
    actor_fixture_in_covenant(builder, actor, state, value, covenant_id, fixture_tag.wrapping_add(1))
}

fn actor_fixture_in_covenant(
    builder: &TxBuilder<'_>,
    actor: &str,
    state: BTreeMap<String, ArtifactValue>,
    value: u64,
    covenant_id: Hash,
    fixture_tag: u8,
) -> CovenantOutput {
    let outpoint = TransactionOutpoint::new(TransactionId::from_bytes([fixture_tag; 32]), 0);
    let utxo = builder
        .covenant_utxo(actor, state, value, 0, false, Some(covenant_id))
        .unwrap_or_else(|err| panic!("{actor} fixture UTXO must build: {err}"));
    CovenantOutput { index: 0, covenant_id, outpoint, utxo }
}

fn register_player(
    builder: &TxBuilder<'_>,
    league_state: BTreeMap<String, ArtifactValue>,
    league: CovenantOutput,
    owner_seed: u8,
    player_value: u64,
) -> (CovenantOutput, TestPlayer, PlayerStateData, CovenantOutput) {
    let (tx, owner, player_state) =
        build_registration(builder, league_state.clone(), league, owner_seed, player_value, league_state, None)
            .expect("league registers a signed player");
    let next_league = CovenantOutput::from_tx(&tx, 0).expect("league continuation is a covenant UTXO");
    let player_output = CovenantOutput::from_tx(&tx, 1).expect("registered player is a covenant UTXO");
    (next_league, owner, player_state, player_output)
}

fn build_registration(
    builder: &TxBuilder<'_>,
    league_state: BTreeMap<String, ArtifactValue>,
    league: CovenantOutput,
    owner_seed: u8,
    player_value: u64,
    next_league_state: BTreeMap<String, ArtifactValue>,
    next_league_value: Option<u64>,
) -> BuilderResult<(Transaction, TestPlayer, PlayerStateData)> {
    let mut unique_preimage = b"LeaguePlayerId".to_vec();
    unique_preimage.extend_from_slice(league.outpoint.transaction_id.as_bytes().as_slice());
    unique_preimage.extend_from_slice(&league.outpoint.index.to_le_bytes());
    let owner = player_with_id(owner_seed, blake2b32(&unique_preimage));
    let player_state = PlayerStateData::registered(&owner);
    let league_value = league.utxo.amount;
    let covenant_id = league.covenant_id;
    let keypair = owner.keypair;
    let public_key = owner.public_key.clone();
    let league_value = next_league_value.unwrap_or(league_value);
    let context = TxContext::new()
        .actor_input(
            "League",
            league_state.clone(),
            EntryCall::new("register_player")
                .args_with(move |tx, input_index| args![sign_input(tx, input_index, &keypair), public_key.clone()]),
            league.outpoint,
            league.utxo,
            0,
        )
        .actor_output("League", next_league_state, CovenantBinding::new(0, covenant_id), league_value)
        .actor_output("Player", player_state.source_state(), CovenantBinding::new(0, covenant_id), player_value);
    builder.build(&context).map(|tx| (tx, owner, player_state))
}

fn execute_signed_rebalance(
    builder: &TxBuilder<'_>,
    actor: &str,
    state: BTreeMap<String, ArtifactValue>,
    source: CovenantOutput,
    signer: &TestPlayer,
) -> CovenantOutput {
    let value = source.utxo.amount;
    let tx = build_signed_rebalance(builder, actor, state.clone(), source, signer, state, value)
        .unwrap_or_else(|err| panic!("{actor} rebalance must execute: {err}"));
    CovenantOutput::from_tx(&tx, 0).expect("rebalance output is a covenant UTXO")
}

fn build_signed_rebalance(
    builder: &TxBuilder<'_>,
    actor: &str,
    state: BTreeMap<String, ArtifactValue>,
    source: CovenantOutput,
    signer: &TestPlayer,
    next_state: BTreeMap<String, ArtifactValue>,
    next_value: u64,
) -> BuilderResult<Transaction> {
    let covenant_id = source.covenant_id;
    let keypair = signer.keypair;
    let public_key = signer.public_key.clone();
    let context = TxContext::new()
        .actor_input(
            actor,
            state.clone(),
            EntryCall::new("rebalance")
                .args_with(move |tx, input_index| args![sign_input(tx, input_index, &keypair), public_key.clone()]),
            source.outpoint,
            source.utxo,
            0,
        )
        .actor_output(actor, next_state, CovenantBinding::new(0, covenant_id), next_value);
    builder.build(&context)
}

fn build_league_fork(
    builder: &TxBuilder<'_>,
    league_state: BTreeMap<String, ArtifactValue>,
    league: CovenantOutput,
    admin: &TestPlayer,
    left: (BTreeMap<String, ArtifactValue>, u64),
    right: (BTreeMap<String, ArtifactValue>, u64),
) -> BuilderResult<Transaction> {
    let covenant_id = league.covenant_id;
    let keypair = admin.keypair;
    let public_key = admin.public_key.clone();
    let context = TxContext::new()
        .actor_input(
            "League",
            league_state.clone(),
            EntryCall::new("fork").args_with(move |tx, input_index| args![sign_input(tx, input_index, &keypair), public_key.clone()]),
            league.outpoint,
            league.utxo,
            0,
        )
        .actor_output("League", left.0, CovenantBinding::new(0, covenant_id), left.1)
        .actor_output("League", right.0, CovenantBinding::new(0, covenant_id), right.1);
    builder.build(&context)
}

fn retire_player(builder: &TxBuilder<'_>, player_state: &PlayerStateData, player_output: CovenantOutput, owner: &TestPlayer) {
    build_player_retirement(builder, player_state, player_output, owner).expect("idle player retires without a covenant output");
}

fn build_player_retirement(
    builder: &TxBuilder<'_>,
    player_state: &PlayerStateData,
    player_output: CovenantOutput,
    owner: &TestPlayer,
) -> BuilderResult<Transaction> {
    let keypair = owner.keypair;
    let public_key = owner.public_key.clone();
    let context = TxContext::new().actor_input(
        "Player",
        player_state.source_state(),
        EntryCall::new("retire").args_with(move |tx, input_index| args![sign_input(tx, input_index, &keypair), public_key.clone()]),
        player_output.outpoint,
        player_output.utxo,
        0,
    );
    builder.build(&context)
}

fn start_game(
    builder: &TxBuilder<'_>,
    leader: (&TestPlayer, &PlayerStateData, CovenantOutput),
    other: (&TestPlayer, &PlayerStateData, CovenantOutput),
    self_side: i64,
) -> StartedGame {
    let (leader_owner, leader_state, leader_output) = leader;
    let (other_owner, other_state, other_output) = other;
    let covenant_id = leader_output.covenant_id;
    assert_eq!(other_output.covenant_id, covenant_id, "both players must belong to the same league");

    let mut next_leader = leader_state.clone();
    next_leader.open_games += 1;
    let mut next_other = other_state.clone();
    next_other.open_games += 1;
    let (white_player, black_player) = if self_side == WHITE {
        (leader_owner.player_ref, other_owner.player_ref)
    } else {
        (other_owner.player_ref, leader_owner.player_ref)
    };
    let game_state = GameStateData::live(white_player, black_player, opening_board());
    let leader_value = leader_output.utxo.amount;
    let other_value = other_output.utxo.amount;
    let leader_keypair = leader_owner.keypair;
    let leader_public_key = leader_owner.public_key.clone();
    let other_keypair = other_owner.keypair;
    let other_public_key = other_owner.public_key.clone();

    let context = TxContext::new()
        .actor_input(
            "Player",
            leader_state.source_state(),
            EntryCall::new("start_game").args_with(move |tx, input_index| {
                args![sign_input(tx, input_index, &leader_keypair), leader_public_key.clone(), self_side, MOVE_TIMEOUT,]
            }),
            leader_output.outpoint,
            leader_output.utxo,
            0,
        )
        .actor_input(
            "Player",
            other_state.source_state(),
            EntryCall::new("delegate_start_game").args_with(move |tx, input_index| {
                args![sign_input(tx, input_index, &other_keypair), other_public_key.clone(), MOVE_TIMEOUT]
            }),
            other_output.outpoint,
            other_output.utxo,
            0,
        )
        .actor_output("Player", next_leader.source_state(), CovenantBinding::new(0, covenant_id), leader_value)
        .actor_output("Player", next_other.source_state(), CovenantBinding::new(0, covenant_id), other_value)
        .actor_output("Mux", game_state.source_state(), CovenantBinding::new(0, covenant_id), GAME_VALUE);
    let tx = builder.build(&context).expect("two registered players start a signed game");
    let leader_output = CovenantOutput::from_tx(&tx, 0).expect("leader continuation is a covenant UTXO");
    let other_output = CovenantOutput::from_tx(&tx, 1).expect("other continuation is a covenant UTXO");
    let game_output = CovenantOutput::from_tx(&tx, 2).expect("new game is a covenant UTXO");
    StartedGame { leader_state: next_leader, leader_output, other_state: next_other, other_output, game_state, game_output }
}

fn route_game_to_settle(builder: &TxBuilder<'_>, game_state: &GameStateData, game_output: CovenantOutput) -> CovenantOutput {
    let covenant_id = game_output.covenant_id;
    let game_value = game_output.utxo.amount;
    let context = TxContext::new()
        .actor_input("Mux", game_state.source_state(), "settle", game_output.outpoint, game_output.utxo, 0)
        .actor_output(
            "Settle",
            settle_state(game_state.white_player, game_state.black_player, game_state.status),
            CovenantBinding::new(0, covenant_id),
            game_value,
        );
    let tx = builder.build(&context).expect("terminal game routes to settlement");
    CovenantOutput::from_tx(&tx, 0).expect("settlement output is a covenant UTXO")
}

fn expected_settlement(
    white_state: &PlayerStateData,
    white_value: u64,
    black_state: &PlayerStateData,
    black_value: u64,
    status: i64,
    settlement_value: u64,
) -> ExpectedSettlement {
    assert_eq!(white_state.rating, black_state.rating, "this fixture expects equal initial ratings");
    let mut next_white = white_state.clone();
    let mut next_black = black_state.clone();
    next_white.open_games -= 1;
    next_black.open_games -= 1;
    next_white.games += 1;
    next_black.games += 1;

    let (white_value, black_value) = match status {
        WWIN => {
            next_white.rating += 16;
            next_white.wins += 1;
            next_black.rating -= 16;
            next_black.losses += 1;
            (white_value + settlement_value, black_value)
        }
        BWIN => {
            next_white.rating -= 16;
            next_white.losses += 1;
            next_black.rating += 16;
            next_black.wins += 1;
            (white_value, black_value + settlement_value)
        }
        DRAW => {
            next_white.draws += 1;
            next_black.draws += 1;
            let white_share = settlement_value / 2;
            (white_value + white_share, black_value + settlement_value - white_share)
        }
        _ => panic!("unsupported settlement status {status}"),
    };

    ExpectedSettlement { white_state: next_white, white_value, black_state: next_black, black_value }
}

fn build_settlement(
    builder: &TxBuilder<'_>,
    settlement: CovenantOutput,
    status: i64,
    white: (&PlayerStateData, CovenantOutput),
    black: (&PlayerStateData, CovenantOutput),
    expected: &ExpectedSettlement,
) -> BuilderResult<Transaction> {
    let (white_state, white_output) = white;
    let (black_state, black_output) = black;
    let covenant_id = settlement.covenant_id;
    let settlement_state = settle_state(
        blake2b32(&[white_state.owner.as_slice(), white_state.player_id.as_slice()].concat()),
        blake2b32(&[black_state.owner.as_slice(), black_state.player_id.as_slice()].concat()),
        status,
    );
    let context = TxContext::new()
        .actor_input("Settle", settlement_state, "settle", settlement.outpoint, settlement.utxo, 0)
        .actor_input("Player", white_state.source_state(), "delegate_settle", white_output.outpoint, white_output.utxo, 0)
        .actor_input("Player", black_state.source_state(), "delegate_settle", black_output.outpoint, black_output.utxo, 0)
        .actor_output("Player", expected.white_state.source_state(), CovenantBinding::new(0, covenant_id), expected.white_value)
        .actor_output("Player", expected.black_state.source_state(), CovenantBinding::new(0, covenant_id), expected.black_value);
    builder.build(&context)
}

fn settle_game(
    builder: &TxBuilder<'_>,
    settlement: CovenantOutput,
    status: i64,
    white: (&PlayerStateData, CovenantOutput),
    black: (&PlayerStateData, CovenantOutput),
) -> SettledGame {
    let expected = expected_settlement(white.0, white.1.utxo.amount, black.0, black.1.utxo.amount, status, settlement.utxo.amount);
    let tx =
        build_settlement(builder, settlement, status, white, black, &expected).expect("settlement updates both delegated players");
    let white_output = CovenantOutput::from_tx(&tx, 0).expect("settled white player is a covenant UTXO");
    let black_output = CovenantOutput::from_tx(&tx, 1).expect("settled black player is a covenant UTXO");
    SettledGame { white_state: expected.white_state, white_output, black_state: expected.black_state, black_output }
}

fn settlement_fixture(builder: &TxBuilder<'_>, status: i64, settlement_value: u64, fixture_tag: u8) -> SettlementFixture {
    let white = player(fixture_tag);
    let black = player(fixture_tag.wrapping_add(1));
    let mut white_state = PlayerStateData::registered(&white);
    white_state.open_games = 1;
    white_state.games = 10;
    white_state.wins = 6;
    white_state.draws = 2;
    white_state.losses = 2;
    let mut black_state = PlayerStateData::registered(&black);
    black_state.open_games = 1;
    black_state.games = 10;
    black_state.wins = 2;
    black_state.draws = 2;
    black_state.losses = 6;

    let covenant_id = Hash::from_bytes([fixture_tag; 32]);
    let white_output =
        actor_fixture_in_covenant(builder, "Player", white_state.source_state(), 1_000, covenant_id, fixture_tag.wrapping_add(2));
    let black_output =
        actor_fixture_in_covenant(builder, "Player", black_state.source_state(), 1_000, covenant_id, fixture_tag.wrapping_add(3));
    let mut game_state = GameStateData::live(white.player_ref, black.player_ref, opening_board());
    game_state.status = status;
    let game_output = actor_fixture_in_covenant(
        builder,
        "Mux",
        game_state.source_state(),
        settlement_value,
        covenant_id,
        fixture_tag.wrapping_add(4),
    );
    let settlement = route_game_to_settle(builder, &game_state, game_output);
    SettlementFixture { white_state, white_output, black_state, black_output, settlement }
}

fn execute_to_settle<'a>(
    builder: &TxBuilder<'_>,
    source_actor: &str,
    source_state: &GameStateData,
    entry: impl Into<EntryCall<'a>>,
    sequence: u64,
    settle_status: i64,
    fixture_tag: u8,
) {
    let covenant_id = Hash::from_bytes([fixture_tag; 32]);
    let source_values = source_state.source_state();
    let outpoint = TransactionOutpoint::new(TransactionId::from_bytes([fixture_tag.wrapping_add(1); 32]), 0);
    let utxo = builder
        .covenant_utxo(source_actor, source_values.clone(), GAME_VALUE, 0, false, Some(covenant_id))
        .unwrap_or_else(|err| panic!("{source_actor} UTXO must build: {err}"));
    let context = TxContext::new().actor_input(source_actor, source_values, entry, outpoint, utxo, sequence).actor_output(
        "Settle",
        settle_state(source_state.white_player, source_state.black_player, settle_status),
        CovenantBinding::new(0, covenant_id),
        GAME_VALUE,
    );
    builder.build(&context).unwrap_or_else(|err| panic!("{source_actor} must transition to Settle: {err}"));
}

struct CastleChallengeSpec<'a> {
    worker: &'a str,
    proof_board: Vec<u8>,
    mv: MoveSpec,
    expected_status: i64,
    fixture_tag: u8,
}

fn execute_castle_challenge(builder: &TxBuilder<'_>, challenger: &TestPlayer, initial: &GameStateData, spec: CastleChallengeSpec<'_>) {
    let (prep_state, prep_output) = route_to_worker(builder, challenger, "CastleChallengePrep", initial, spec.mv, spec.fixture_tag);
    let mut worker_state = prep_state.clone();
    worker_state.board = spec.proof_board;
    let worker_output = execute_actor_transition(
        builder,
        "CastleChallengePrep",
        &prep_state,
        EntryCall::new("apply").args(args![actor(spec.worker)]),
        prep_output,
        spec.worker,
        &worker_state,
    );

    let mut expected = worker_state.completed_move(spec.mv);
    expected.status = spec.expected_status;
    execute_actor_transition(builder, spec.worker, &worker_state, "apply", worker_output, "Mux", &expected);
}

fn execute_worker_timeout(
    builder: &TxBuilder<'_>,
    worker: &str,
    worker_state: &GameStateData,
    worker_output: CovenantOutput,
    expected_status: i64,
) {
    let output_value = worker_output.utxo.amount;
    let context = TxContext::new()
        .actor_input(worker, worker_state.source_state(), "timeout", worker_output.outpoint, worker_output.utxo, MOVE_TIMEOUT as u64)
        .actor_output(
            "Settle",
            settle_state(worker_state.white_player, worker_state.black_player, expected_status),
            CovenantBinding::new(0, worker_output.covenant_id),
            output_value,
        );
    builder.build(&context).unwrap_or_else(|err| panic!("{worker} timeout must transition to Settle: {err}"));
}

#[test]
fn muxed_chess_routes_all_move_families() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x21);
    let black_player_ref = [0x42; 32];

    let pawn_move = MoveSpec::new(4, 1, 4, 3);
    let pawn_initial = GameStateData::live(white.player_ref, black_player_ref, opening_board());
    let mut pawn_expected = pawn_initial.completed_move(pawn_move);
    pawn_expected.en_passant_idx = 20;
    execute_worker_round_trip(&builder, &white, "Pawn", &pawn_initial, pawn_move, &pawn_expected, 0x61);

    let knight_move = MoveSpec::new(1, 0, 2, 2);
    let knight_initial = GameStateData::live(white.player_ref, black_player_ref, opening_board());
    let knight_expected = knight_initial.completed_move(knight_move);
    execute_worker_round_trip(&builder, &white, "Knight", &knight_initial, knight_move, &knight_expected, 0x63);

    let mut vert_board = vec![0; 64];
    vert_board[0] = 0x04;
    let vert_move = MoveSpec::new(0, 0, 0, 3);
    let vert_initial = GameStateData::live(white.player_ref, black_player_ref, vert_board);
    let mut vert_expected = vert_initial.completed_move(vert_move);
    vert_expected.castle_rights = [1, 0, 1, 1];
    execute_worker_round_trip(&builder, &white, "Vert", &vert_initial, vert_move, &vert_expected, 0x65);

    let mut horiz_board = vec![0; 64];
    horiz_board[24] = 0x04;
    let horiz_move = MoveSpec::new(0, 3, 3, 3);
    let horiz_initial = GameStateData::live(white.player_ref, black_player_ref, horiz_board);
    let horiz_expected = horiz_initial.completed_move(horiz_move);
    execute_worker_round_trip(&builder, &white, "Horiz", &horiz_initial, horiz_move, &horiz_expected, 0x67);

    let mut diag_board = vec![0; 64];
    diag_board[0] = 0x03;
    let diag_move = MoveSpec::new(0, 0, 3, 3);
    let diag_initial = GameStateData::live(white.player_ref, black_player_ref, diag_board);
    let diag_expected = diag_initial.completed_move(diag_move);
    execute_worker_round_trip(&builder, &white, "Diag", &diag_initial, diag_move, &diag_expected, 0x69);

    let mut king_board = vec![0; 64];
    king_board[4] = 0x06;
    let king_move = MoveSpec::new(4, 0, 4, 1);
    let king_initial = GameStateData::live(white.player_ref, black_player_ref, king_board);
    let mut king_expected = king_initial.completed_move(king_move);
    king_expected.castle_rights = [0, 0, 1, 1];
    execute_worker_round_trip(&builder, &white, "King", &king_initial, king_move, &king_expected, 0x6b);
}

#[test]
fn capturing_enemy_king_sets_terminal_status() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x22);
    let mut board = vec![0; 64];
    board[0] = 0x05;
    board[24] = 0x0e;

    let initial = GameStateData::live(white.player_ref, [0x43; 32], board);
    let mv = MoveSpec::new(0, 0, 0, 3);
    let mut expected = initial.completed_move(mv);
    expected.status = WWIN;
    expected.castle_rights = [1, 0, 1, 1];
    execute_worker_round_trip(&builder, &white, "Vert", &initial, mv, &expected, 0x6d);
}

#[test]
fn ignoring_single_check_is_punishable_by_next_ply_king_capture() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x23);
    let black = player(0x24);
    let mut board = vec![0; 64];
    board[4] = 0x06;
    board[6] = 0x02;
    board[60] = 0x0c;

    let initial = GameStateData::live(white.player_ref, black.player_ref, board);
    let white_move = MoveSpec::new(6, 0, 5, 2);
    let after_white = initial.completed_move(white_move);
    let (knight_state, knight_output) = route_to_worker(&builder, &white, "Knight", &initial, white_move, 0x6e);
    let mux_output = execute_actor_transition(&builder, "Knight", &knight_state, "apply", knight_output, "Mux", &after_white);

    let black_move = MoveSpec::new(4, 7, 4, 0);
    let mut expected = after_white.completed_move(black_move);
    expected.status = BWIN;
    execute_worker_from_mux(&builder, &black, "Vert", &after_white, mux_output, black_move, &expected);
}

#[test]
fn moving_a_pinned_piece_is_punishable_by_next_ply_king_capture() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x25);
    let black = player(0x26);
    let mut board = vec![0; 64];
    board[4] = 0x06;
    board[12] = 0x04;
    board[60] = 0x0c;

    let initial = GameStateData::live(white.player_ref, black.player_ref, board);
    let white_move = MoveSpec::new(4, 1, 5, 1);
    let after_white = initial.completed_move(white_move);
    let (rook_state, rook_output) = route_to_worker(&builder, &white, "Horiz", &initial, white_move, 0x6f);
    let mux_output = execute_actor_transition(&builder, "Horiz", &rook_state, "apply", rook_output, "Mux", &after_white);

    let black_move = MoveSpec::new(4, 7, 4, 0);
    let mut expected = after_white.completed_move(black_move);
    expected.status = BWIN;
    execute_worker_from_mux(&builder, &black, "Vert", &after_white, mux_output, black_move, &expected);
}

#[test]
fn king_move_into_attack_is_punishable_by_next_ply_king_capture() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x27);
    let black = player(0x28);
    let mut board = vec![0; 64];
    board[4] = 0x06;
    board[60] = 0x0c;

    let initial = GameStateData::live(white.player_ref, black.player_ref, board);
    let white_move = MoveSpec::new(4, 0, 4, 1);
    let mut after_white = initial.completed_move(white_move);
    after_white.castle_rights = [0, 0, 1, 1];
    let (king_state, king_output) = route_to_worker(&builder, &white, "King", &initial, white_move, 0x70);
    let mux_output = execute_actor_transition(&builder, "King", &king_state, "apply", king_output, "Mux", &after_white);

    let black_move = MoveSpec::new(4, 7, 4, 1);
    let mut expected = after_white.completed_move(black_move);
    expected.status = BWIN;
    execute_worker_from_mux(&builder, &black, "Vert", &after_white, mux_output, black_move, &expected);
}

#[test]
fn legal_interposition_blocks_the_immediate_king_capture_route() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x29);
    let black = player(0x2a);
    let mut board = vec![0; 64];
    board[4] = 0x06;
    board[5] = 0x03;
    board[60] = 0x0c;

    let initial = GameStateData::live(white.player_ref, black.player_ref, board);
    let white_move = MoveSpec::new(5, 0, 4, 1);
    let after_white = initial.completed_move(white_move);
    let (bishop_state, bishop_output) = route_to_worker(&builder, &white, "Diag", &initial, white_move, 0x72);
    let mux_output = execute_actor_transition(&builder, "Diag", &bishop_state, "apply", bishop_output, "Mux", &after_white);

    let black_move = MoveSpec::new(4, 7, 4, 0);
    let (rook_state, rook_output) = route_mux_output_to_worker(&builder, &black, "Vert", &after_white, mux_output, black_move, CLEAR);
    let mut invalid = after_white.clone();
    invalid.turn = WHITE;
    assert_actor_transition_rejected(&builder, "Vert", &rook_state, "apply", &rook_output, "Mux", &invalid);
}

#[test]
fn illegal_double_check_reply_is_punishable_by_next_ply_king_capture() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x2b);
    let black = player(0x2c);
    let mut board = vec![0; 64];
    board[4] = 0x06;
    board[5] = 0x03;
    board[25] = 0x0b;
    board[60] = 0x0c;

    let initial = GameStateData::live(white.player_ref, black.player_ref, board);
    let white_move = MoveSpec::new(5, 0, 4, 1);
    let after_white = initial.completed_move(white_move);
    let (bishop_state, bishop_output) = route_to_worker(&builder, &white, "Diag", &initial, white_move, 0x74);
    let mux_output = execute_actor_transition(&builder, "Diag", &bishop_state, "apply", bishop_output, "Mux", &after_white);

    let black_move = MoveSpec::new(1, 3, 4, 0);
    let mut expected = after_white.completed_move(black_move);
    expected.status = BWIN;
    execute_worker_from_mux(&builder, &black, "Diag", &after_white, mux_output, black_move, &expected);
}

#[test]
fn pawn_underpromotion_to_knight_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x2d);
    let mut board = vec![0; 64];
    board[52] = 0x01;

    let initial = GameStateData::live(white.player_ref, [0x44; 32], board);
    let mv = MoveSpec::with_promotion(4, 6, 4, 7, 2);
    let mut expected = initial.completed_move(mv);
    expected.board[60] = 0x02;
    execute_worker_round_trip(&builder, &white, "Pawn", &initial, mv, &expected, 0x76);
}

#[test]
fn pawn_promotion_requires_choice() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x2e);
    let mut board = vec![0; 64];
    board[52] = 0x01;

    let initial = GameStateData::live(white.player_ref, [0x45; 32], board);
    let mv = MoveSpec::new(4, 6, 4, 7);
    let invalid = initial.completed_move(mv);
    let (pawn_state, pawn_output) = route_to_worker(&builder, &white, "Pawn", &initial, mv, 0x78);
    assert_actor_transition_rejected(&builder, "Pawn", &pawn_state, "apply", &pawn_output, "Mux", &invalid);
}

#[test]
fn non_promotion_pawn_move_rejects_promotion_choice() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x2f);
    let mut board = vec![0; 64];
    board[12] = 0x01;

    let initial = GameStateData::live(white.player_ref, [0x46; 32], board);
    let mv = MoveSpec::with_promotion(4, 1, 4, 2, 5);
    let invalid = initial.completed_move(mv);
    let (pawn_state, pawn_output) = route_to_worker(&builder, &white, "Pawn", &initial, mv, 0x7a);
    assert_actor_transition_rejected(&builder, "Pawn", &pawn_state, "apply", &pawn_output, "Mux", &invalid);
}

#[test]
fn white_en_passant_capture_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x30);
    let mut board = vec![0; 64];
    board[36] = 0x01;
    board[35] = 0x09;

    let mut initial = GameStateData::live(white.player_ref, [0x47; 32], board);
    initial.en_passant_idx = 43;
    let mv = MoveSpec::new(4, 4, 3, 5);
    let mut expected = initial.completed_move(mv);
    expected.board[35] = 0;
    execute_worker_round_trip(&builder, &white, "Pawn", &initial, mv, &expected, 0x7c);
}

#[test]
fn black_en_passant_capture_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x33);
    let mut board = vec![0; 64];
    board[27] = 0x09;
    board[28] = 0x01;

    let mut initial = GameStateData::live([0x48; 32], black.player_ref, board);
    initial.turn = BLACK;
    initial.en_passant_idx = 20;
    let mv = MoveSpec::new(3, 3, 4, 2);
    let mut expected = initial.completed_move(mv);
    expected.board[28] = 0;
    execute_worker_round_trip(&builder, &black, "Pawn", &initial, mv, &expected, 0x7e);
}

#[test]
fn non_pawn_move_clears_en_passant_state() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x34);
    let mut board = vec![0; 64];
    board[1] = 0x02;

    let mut initial = GameStateData::live(white.player_ref, [0x49; 32], board);
    initial.en_passant_idx = 43;
    let mv = MoveSpec::new(1, 0, 2, 2);
    let expected = initial.completed_move(mv);
    execute_worker_round_trip(&builder, &white, "Knight", &initial, mv, &expected, 0x80);
}

#[test]
fn pawn_diagonal_capture_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x35);
    let mut board = vec![0; 64];
    board[36] = 0x01;
    board[45] = 0x0a;

    let initial = GameStateData::live(white.player_ref, [0x4a; 32], board);
    let mv = MoveSpec::new(4, 4, 5, 5);
    let expected = initial.completed_move(mv);
    execute_worker_round_trip(&builder, &white, "Pawn", &initial, mv, &expected, 0x82);
}

#[test]
fn pawn_double_step_blocked_by_occupied_middle_square_fails() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x36);
    let mut board = vec![0; 64];
    board[12] = 0x01;
    board[20] = 0x09;

    let initial = GameStateData::live(white.player_ref, [0x4b; 32], board);
    let mv = MoveSpec::new(4, 1, 4, 3);
    let mut invalid = initial.completed_move(mv);
    invalid.en_passant_idx = 20;
    let (pawn_state, pawn_output) = route_to_worker(&builder, &white, "Pawn", &initial, mv, 0x84);
    assert_actor_transition_rejected(&builder, "Pawn", &pawn_state, "apply", &pawn_output, "Mux", &invalid);
}

#[test]
fn pawn_diagonal_move_into_empty_square_fails_without_en_passant() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x37);
    let mut board = vec![0; 64];
    board[36] = 0x01;

    let initial = GameStateData::live(white.player_ref, [0x4c; 32], board);
    let mv = MoveSpec::new(4, 4, 5, 5);
    let invalid = initial.completed_move(mv);
    let (pawn_state, pawn_output) = route_to_worker(&builder, &white, "Pawn", &initial, mv, 0x86);
    assert_actor_transition_rejected(&builder, "Pawn", &pawn_state, "apply", &pawn_output, "Mux", &invalid);
}

#[test]
fn expired_en_passant_attempt_fails() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x38);
    let mut board = vec![0; 64];
    board[36] = 0x01;
    board[35] = 0x09;

    let initial = GameStateData::live(white.player_ref, [0x4d; 32], board);
    let mv = MoveSpec::new(4, 4, 3, 5);
    let mut invalid = initial.completed_move(mv);
    invalid.board[35] = 0;
    let (pawn_state, pawn_output) = route_to_worker(&builder, &white, "Pawn", &initial, mv, 0x88);
    assert_actor_transition_rejected(&builder, "Pawn", &pawn_state, "apply", &pawn_output, "Mux", &invalid);
}

#[test]
fn all_castle_shapes_rewrite_expected_board() {
    struct CastleCase {
        board: Vec<u8>,
        turn: i64,
        mv: MoveSpec,
        expected_board: Vec<u8>,
        expected_rights: [u8; 4],
        expected_recent_castle: i64,
        fixture_tag: u8,
    }

    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x31);
    let black = player(0x32);

    let mut white_kingside = vec![0; 64];
    white_kingside[4] = 0x06;
    white_kingside[7] = 0x04;
    let mut white_kingside_expected = vec![0; 64];
    white_kingside_expected[5] = 0x04;
    white_kingside_expected[6] = 0x06;

    let mut white_queenside = vec![0; 64];
    white_queenside[0] = 0x04;
    white_queenside[4] = 0x06;
    let mut white_queenside_expected = vec![0; 64];
    white_queenside_expected[2] = 0x06;
    white_queenside_expected[3] = 0x04;

    let mut black_kingside = vec![0; 64];
    black_kingside[60] = 0x0e;
    black_kingside[63] = 0x0c;
    let mut black_kingside_expected = vec![0; 64];
    black_kingside_expected[61] = 0x0c;
    black_kingside_expected[62] = 0x0e;

    let mut black_queenside = vec![0; 64];
    black_queenside[56] = 0x0c;
    black_queenside[60] = 0x0e;
    let mut black_queenside_expected = vec![0; 64];
    black_queenside_expected[58] = 0x0e;
    black_queenside_expected[59] = 0x0c;

    let cases = [
        CastleCase {
            board: white_kingside,
            turn: WHITE,
            mv: MoveSpec::new(4, 0, 6, 0),
            expected_board: white_kingside_expected,
            expected_rights: [0, 0, 1, 1],
            expected_recent_castle: 1,
            fixture_tag: 0x71,
        },
        CastleCase {
            board: white_queenside,
            turn: WHITE,
            mv: MoveSpec::new(4, 0, 2, 0),
            expected_board: white_queenside_expected,
            expected_rights: [0, 0, 1, 1],
            expected_recent_castle: 2,
            fixture_tag: 0x73,
        },
        CastleCase {
            board: black_kingside,
            turn: BLACK,
            mv: MoveSpec::new(4, 7, 6, 7),
            expected_board: black_kingside_expected,
            expected_rights: [1, 1, 0, 0],
            expected_recent_castle: 3,
            fixture_tag: 0x75,
        },
        CastleCase {
            board: black_queenside,
            turn: BLACK,
            mv: MoveSpec::new(4, 7, 2, 7),
            expected_board: black_queenside_expected,
            expected_rights: [1, 1, 0, 0],
            expected_recent_castle: 4,
            fixture_tag: 0x77,
        },
    ];

    for case in cases {
        let mut initial = GameStateData::live(white.player_ref, black.player_ref, case.board);
        initial.turn = case.turn;
        let mut expected = initial.clone();
        expected.board = case.expected_board;
        expected.turn = 1 - case.turn;
        expected.castle_rights = case.expected_rights;
        expected.recent_castle = case.expected_recent_castle;
        let mover = if case.turn == WHITE { &white } else { &black };
        execute_worker_round_trip(&builder, mover, "Castle", &initial, case.mv, &expected, case.fixture_tag);
    }
}

#[test]
fn castle_start_square_challenge_by_pawn_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x41);
    let black = player(0x42);

    let mut post_castle_board = vec![0; 64];
    post_castle_board[5] = 0x04;
    post_castle_board[6] = 0x06;
    post_castle_board[11] = 0x09;
    let mut mux_state = GameStateData::live(white.player_ref, black.player_ref, post_castle_board);
    mux_state.turn = BLACK;
    mux_state.castle_rights = [0, 0, 1, 1];
    mux_state.recent_castle = 1;
    let challenge_move = MoveSpec::new(3, 1, 4, 0);

    let mut proof_board = vec![0; 64];
    proof_board[4] = 0x06;
    proof_board[7] = 0x04;
    proof_board[11] = 0x09;
    execute_castle_challenge(
        &builder,
        &black,
        &mux_state,
        CastleChallengeSpec { worker: "Pawn", proof_board, mv: challenge_move, expected_status: BWIN, fixture_tag: 0x79 },
    );
}

#[test]
fn ordinary_reply_after_castle_clears_recent_castle() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x3a);
    let mut board = vec![0; 64];
    board[5] = 0x04;
    board[6] = 0x06;
    board[62] = 0x0a;

    let mut initial = GameStateData::live([0x50; 32], black.player_ref, board);
    initial.turn = BLACK;
    initial.castle_rights = [0, 0, 1, 1];
    initial.recent_castle = 1;
    let mv = MoveSpec::new(6, 7, 5, 5);
    let expected = initial.completed_move(mv);
    execute_worker_round_trip(&builder, &black, "Knight", &initial, mv, &expected, 0x8a);
}

#[test]
fn castle_transit_square_challenge_by_rook_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x3b);
    let mut board = vec![0; 64];
    board[5] = 0x04;
    board[6] = 0x06;
    board[61] = 0x0c;
    let mut initial = GameStateData::live([0x51; 32], black.player_ref, board);
    initial.turn = BLACK;
    initial.castle_rights = [0, 0, 1, 1];
    initial.recent_castle = 1;

    let mut proof_board = vec![0; 64];
    proof_board[5] = 0x06;
    proof_board[7] = 0x04;
    proof_board[61] = 0x0c;
    execute_castle_challenge(
        &builder,
        &black,
        &initial,
        CastleChallengeSpec { worker: "Vert", proof_board, mv: MoveSpec::new(5, 7, 5, 0), expected_status: BWIN, fixture_tag: 0x8c },
    );
}

#[test]
fn castle_destination_square_challenge_by_rook_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x3c);
    let mut board = vec![0; 64];
    board[5] = 0x04;
    board[6] = 0x06;
    board[62] = 0x0c;
    let mut initial = GameStateData::live([0x52; 32], black.player_ref, board);
    initial.turn = BLACK;
    initial.castle_rights = [0, 0, 1, 1];
    initial.recent_castle = 1;

    execute_castle_challenge(
        &builder,
        &black,
        &initial,
        CastleChallengeSpec {
            worker: "Vert",
            proof_board: initial.board.clone(),
            mv: MoveSpec::new(6, 7, 6, 0),
            expected_status: BWIN,
            fixture_tag: 0x8e,
        },
    );
}

#[test]
fn white_queenside_castle_destination_challenge_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x3d);
    let mut board = vec![0; 64];
    board[2] = 0x06;
    board[3] = 0x04;
    board[58] = 0x0c;
    let mut initial = GameStateData::live([0x53; 32], black.player_ref, board);
    initial.turn = BLACK;
    initial.castle_rights = [0, 0, 1, 1];
    initial.recent_castle = 2;

    execute_castle_challenge(
        &builder,
        &black,
        &initial,
        CastleChallengeSpec {
            worker: "Vert",
            proof_board: initial.board.clone(),
            mv: MoveSpec::new(2, 7, 2, 0),
            expected_status: BWIN,
            fixture_tag: 0x90,
        },
    );
}

#[test]
fn black_kingside_castle_start_challenge_by_pawn_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x3e);
    let mut board = vec![0; 64];
    board[61] = 0x0c;
    board[62] = 0x0e;
    board[51] = 0x01;
    let mut initial = GameStateData::live(white.player_ref, [0x54; 32], board);
    initial.castle_rights = [1, 1, 0, 0];
    initial.recent_castle = 3;

    let mut proof_board = vec![0; 64];
    proof_board[60] = 0x0e;
    proof_board[63] = 0x0c;
    proof_board[51] = 0x01;
    execute_castle_challenge(
        &builder,
        &white,
        &initial,
        CastleChallengeSpec { worker: "Pawn", proof_board, mv: MoveSpec::new(3, 6, 4, 7), expected_status: WWIN, fixture_tag: 0x92 },
    );
}

#[test]
fn black_queenside_castle_transit_challenge_by_rook_succeeds() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x3f);
    let mut board = vec![0; 64];
    board[58] = 0x0e;
    board[59] = 0x0c;
    board[3] = 0x04;
    let mut initial = GameStateData::live(white.player_ref, [0x55; 32], board);
    initial.castle_rights = [1, 1, 0, 0];
    initial.recent_castle = 4;

    let mut proof_board = vec![0; 64];
    proof_board[56] = 0x0c;
    proof_board[59] = 0x0e;
    proof_board[3] = 0x04;
    execute_castle_challenge(
        &builder,
        &white,
        &initial,
        CastleChallengeSpec { worker: "Vert", proof_board, mv: MoveSpec::new(3, 0, 3, 7), expected_status: WWIN, fixture_tag: 0x94 },
    );
}

#[test]
fn invalid_castle_destination_challenge_loses_by_worker_timeout() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x40);
    let mut board = vec![0; 64];
    board[5] = 0x04;
    board[6] = 0x06;
    board[38] = 0x09;
    board[62] = 0x0c;
    let mut initial = GameStateData::live([0x56; 32], black.player_ref, board);
    initial.turn = BLACK;
    initial.castle_rights = [0, 0, 1, 1];
    initial.recent_castle = 1;
    let mv = MoveSpec::new(6, 7, 6, 0);

    let (prep_state, prep_output) = route_to_worker(&builder, &black, "CastleChallengePrep", &initial, mv, 0x96);
    let worker_state = prep_state.clone();
    let worker_output = execute_actor_transition(
        &builder,
        "CastleChallengePrep",
        &prep_state,
        EntryCall::new("apply").args(args![actor("Vert")]),
        prep_output,
        "Vert",
        &worker_state,
    );

    let mut impossible = worker_state.completed_move(mv);
    impossible.status = BWIN;
    assert_actor_transition_rejected(&builder, "Vert", &worker_state, "apply", &worker_output, "Mux", &impossible);
    execute_worker_timeout(&builder, "Vert", &worker_state, worker_output, WWIN);
}

#[test]
fn ordinary_move_can_offer_draw_and_return_to_mux() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x51);
    let move_spec = MoveSpec::new(4, 1, 4, 3);
    let initial = GameStateData::live(white.player_ref, [0x52; 32], opening_board());
    let (pawn_state, pawn_output) = route_to_worker_with_action(&builder, &white, "Pawn", &initial, move_spec, OFFER, 0x81);
    assert_eq!(pawn_state.draw_state, WOFFER);

    let mut expected = initial.completed_move(move_spec);
    expected.en_passant_idx = 20;
    expected.draw_state = WOFFER;
    execute_actor_transition(&builder, "Pawn", &pawn_state, "apply", pawn_output, "Mux", &expected);
}

#[test]
fn claim_draw_flips_turn_and_enters_draw_state() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x53);
    let initial = GameStateData::live(white.player_ref, [0x54; 32], opening_board());

    let mut claimed = initial.clone();
    claimed.turn = BLACK;
    claimed.draw_state = CLAIMED;
    execute_mux_terminate(&builder, &white, &initial, CLAIM, &claimed, 0x83);
}

#[test]
fn surrender_routes_back_to_mux_with_terminal_status() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x55);
    let mut initial = GameStateData::live(white.player_ref, [0x56; 32], opening_board());
    initial.en_passant_idx = 20;
    initial.recent_castle = 1;
    initial.draw_state = CLAIMED;

    let mut surrendered = initial.clone();
    surrendered.status = BWIN;
    surrendered.en_passant_idx = -1;
    surrendered.recent_castle = CLEAR;
    surrendered.draw_state = NORMAL;
    execute_mux_terminate(&builder, &white, &initial, SURRENDER, &surrendered, 0x85);
}

#[test]
fn pending_draw_offer_can_be_accepted_on_next_mux_turn() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x57);
    let black = player(0x58);
    let mut offered = GameStateData::live(white.player_ref, black.player_ref, opening_board());
    offered.turn = BLACK;
    offered.draw_state = WOFFER;
    let mut accepted = offered.clone();
    accepted.status = DRAW;
    accepted.draw_state = NORMAL;
    execute_mux_terminate(&builder, &black, &offered, ACCEPT, &accepted, 0x87);
}

#[test]
fn route_rejects_changing_the_locked_game_value() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x59);
    let initial = GameStateData::live(white.player_ref, [0x5a; 32], opening_board());
    assert_mux_route_rejected(&builder, &white, "Knight", &initial, MoveSpec::new(6, 0, 5, 2), GAME_VALUE - 1, 0x89);
}

#[test]
fn ordinary_reply_rejects_pending_draw_offer_and_clears_draw_state() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x5b);
    let mut board = opening_board();
    board[6] = 0;
    board[21] = 0x02;
    let mut initial = GameStateData::live([0x5c; 32], black.player_ref, board);
    initial.turn = BLACK;
    initial.draw_state = WOFFER;

    let mv = MoveSpec::new(6, 7, 5, 5);
    let mut expected = initial.completed_move(mv);
    expected.draw_state = NORMAL;
    execute_worker_round_trip(&builder, &black, "Knight", &initial, mv, &expected, 0x8b);
}

#[test]
fn knight_draw_negotiation_flips_side_control_and_false_claim_loses() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x5d);
    let black = player(0x5e);
    let mut board = vec![0; 64];
    board[1] = 0x02;
    board[62] = 0x0a;
    let initial = GameStateData::live(white.player_ref, black.player_ref, board);

    let mut claimed = initial.clone();
    claimed.turn = BLACK;
    claimed.draw_state = CLAIMED;
    let mux_output = execute_mux_terminate(&builder, &white, &initial, CLAIM, &claimed, 0x8d);

    let white_piece_move = MoveSpec::new(1, 0, 2, 2);
    let mut defense = claimed.completed_move(white_piece_move);
    defense.draw_state = DEFENSE;
    let mux_output = execute_worker_from_mux(&builder, &black, "Knight", &claimed, mux_output, white_piece_move, &defense);

    let black_piece_move = MoveSpec::new(6, 7, 5, 5);
    let mut failed_claim = defense.completed_move(black_piece_move);
    failed_claim.status = BWIN;
    failed_claim.draw_state = DEFENSE;
    execute_worker_from_mux(&builder, &white, "Knight", &defense, mux_output, black_piece_move, &failed_claim);
}

#[test]
fn knight_draw_capture_awards_win_to_the_actor() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x5f);
    let black = player(0x60);
    let mut board = vec![0; 64];
    board[1] = 0x02;
    board[18] = 0x0e;
    let initial = GameStateData::live(white.player_ref, black.player_ref, board);

    let mut claimed = initial.clone();
    claimed.turn = BLACK;
    claimed.draw_state = CLAIMED;
    let mux_output = execute_mux_terminate(&builder, &white, &initial, CLAIM, &claimed, 0x8f);

    let mv = MoveSpec::new(1, 0, 2, 2);
    let mut expected = claimed.completed_move(mv);
    expected.status = BWIN;
    expected.draw_state = DEFENSE;
    execute_worker_from_mux(&builder, &black, "Knight", &claimed, mux_output, mv, &expected);
}

#[test]
fn pawn_draw_capture_awards_win_to_the_actor() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x65);
    let mut board = vec![0; 64];
    board[27] = 0x01;
    board[36] = 0x0e;
    let mut initial = GameStateData::live([0x66; 32], black.player_ref, board);
    initial.turn = BLACK;
    initial.draw_state = CLAIMED;

    let mv = MoveSpec::new(3, 3, 4, 4);
    let mut expected = initial.completed_move(mv);
    expected.status = BWIN;
    expected.draw_state = DEFENSE;
    execute_worker_round_trip(&builder, &black, "Pawn", &initial, mv, &expected, 0x97);
}

#[test]
fn draw_mode_reuses_ordinary_workers() {
    struct DrawWorkerCase {
        worker: &'static str,
        piece: u8,
        source: usize,
        mv: MoveSpec,
        castle_rights: [u8; 4],
        fixture_tag: u8,
    }

    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x67);
    let cases = [
        DrawWorkerCase {
            worker: "Pawn",
            piece: 0x01,
            source: 12,
            mv: MoveSpec::new(4, 1, 4, 2),
            castle_rights: [1; 4],
            fixture_tag: 0x99,
        },
        DrawWorkerCase {
            worker: "Vert",
            piece: 0x04,
            source: 0,
            mv: MoveSpec::new(0, 0, 0, 3),
            castle_rights: [1, 0, 1, 1],
            fixture_tag: 0x9b,
        },
        DrawWorkerCase {
            worker: "Horiz",
            piece: 0x04,
            source: 0,
            mv: MoveSpec::new(0, 0, 3, 0),
            castle_rights: [1, 0, 1, 1],
            fixture_tag: 0x9d,
        },
        DrawWorkerCase {
            worker: "Diag",
            piece: 0x03,
            source: 2,
            mv: MoveSpec::new(2, 0, 5, 3),
            castle_rights: [1; 4],
            fixture_tag: 0x9f,
        },
        DrawWorkerCase {
            worker: "King",
            piece: 0x06,
            source: 4,
            mv: MoveSpec::new(4, 0, 4, 1),
            castle_rights: [0, 0, 1, 1],
            fixture_tag: 0xa1,
        },
    ];

    for case in cases {
        let mut board = vec![0; 64];
        board[case.source] = case.piece;
        let mut initial = GameStateData::live([case.fixture_tag; 32], black.player_ref, board);
        initial.turn = BLACK;
        initial.draw_state = CLAIMED;
        let mut expected = initial.completed_move(case.mv);
        expected.draw_state = DEFENSE;
        expected.castle_rights = case.castle_rights;
        execute_worker_round_trip(&builder, &black, case.worker, &initial, case.mv, &expected, case.fixture_tag);
    }
}

#[test]
fn draw_mode_disallows_castle_and_castle_challenge_routes() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let black = player(0x68);
    let mut board = vec![0; 64];
    board[4] = 0x06;
    board[7] = 0x04;
    let mut initial = GameStateData::live([0x69; 32], black.player_ref, board);
    initial.turn = BLACK;
    initial.draw_state = CLAIMED;
    let mv = MoveSpec::new(4, 0, 6, 0);

    assert_mux_route_rejected(&builder, &black, "Castle", &initial, mv, GAME_VALUE, 0xa3);
    initial.recent_castle = 1;
    assert_mux_route_rejected(&builder, &black, "CastleChallengePrep", &initial, mv, GAME_VALUE, 0xa5);
}

#[test]
fn knight_worker_timeout_rescues_invalid_committed_state() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x61);
    let black = player(0x62);

    let initial = GameStateData::live(white.player_ref, black.player_ref, opening_board());
    let invalid_knight_move = MoveSpec::new(0, 1, 0, 2);
    let knight_state = initial.committed_route("Knight", invalid_knight_move, CLEAR);
    execute_to_settle(&builder, "Knight", &knight_state, "timeout", MOVE_TIMEOUT as u64, BWIN, 0x91);
}

#[test]
fn mux_timeout_awards_win_to_the_waiting_opponent() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x63);
    let black = player(0x64);
    let initial = GameStateData::live(white.player_ref, black.player_ref, opening_board());
    let keypair = black.keypair;
    let public_key = black.public_key.clone();
    let player_id = black.player_id;
    let mux_timeout = EntryCall::new("timeout")
        .args_with(move |tx, input_index| args![sign_input(tx, input_index, &keypair), public_key.clone(), player_id]);
    execute_to_settle(&builder, "Mux", &initial, mux_timeout, MOVE_TIMEOUT as u64, BWIN, 0x93);
}

#[test]
fn league_registers_a_real_player_contract() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x71);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let (_league, owner, player_state, player_output) = register_player(&builder, league_values, league, 0x72, 2_000);

    assert_eq!(player_state.owner, owner.owner);
    assert_eq!(player_state.open_games, 0);
    assert_eq!(player_state.rating, BASE_RATING);
    assert_eq!(player_output.utxo.amount, 2_000);
}

#[test]
fn league_register_rejects_mutated_lane_output() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x72);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let mutated = state! {
        base_rating: BASE_RATING + 1,
        admin: admin.owner,
    };

    assert!(build_registration(&builder, league_values, league, 0x73, 2_000, mutated, None).is_err());
}

#[test]
fn league_register_rejects_changed_lane_value() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x74);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);

    assert!(build_registration(&builder, league_values.clone(), league, 0x75, 2_000, league_values, Some(4_999)).is_err());
}

#[test]
fn league_rebalance_allows_same_spk_with_new_value() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x76);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);

    build_signed_rebalance(&builder, "League", league_values.clone(), league, &admin, league_values, 777)
        .expect("league rebalance permits a new value with unchanged state");
}

#[test]
fn league_rebalance_rejects_changed_state_spk() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x77);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let mutated = state! {
        base_rating: BASE_RATING + 1,
        admin: admin.owner,
    };

    assert!(build_signed_rebalance(&builder, "League", league_values, league, &admin, mutated, 777).is_err());
}

#[test]
fn league_fork_allows_two_identical_lanes() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x78);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);

    build_league_fork(&builder, league_values.clone(), league, &admin, (league_values.clone(), 2_000), (league_values, 3_000))
        .expect("league fork permits two identical state lanes");
}

#[test]
fn league_fork_rejects_mutated_lane_output() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x79);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let mutated = state! {
        base_rating: BASE_RATING + 1,
        admin: admin.owner,
    };

    assert!(build_league_fork(&builder, league_values.clone(), league, &admin, (league_values, 2_000), (mutated, 3_000),).is_err());
}

#[test]
fn player_rebalance_allows_same_spk_with_new_value() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x7a);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let (_league, owner, player_state, player_output) = register_player(&builder, league_values, league, 0x7b, 2_000);

    build_signed_rebalance(&builder, "Player", player_state.source_state(), player_output, &owner, player_state.source_state(), 1_500)
        .expect("player rebalance permits a new value with unchanged state");
}

#[test]
fn player_rebalance_rejects_changed_state_spk() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x7c);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let (_league, owner, player_state, player_output) = register_player(&builder, league_values, league, 0x7d, 2_000);
    let mut mutated = player_state.clone();
    mutated.rating += 1;

    assert!(build_signed_rebalance(
        &builder,
        "Player",
        player_state.source_state(),
        player_output,
        &owner,
        mutated.source_state(),
        2_000,
    )
    .is_err());
}

#[test]
fn player_can_retire_with_no_open_games() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x7e);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let (_league, owner, player_state, player_output) = register_player(&builder, league_values, league, 0x7f, 2_000);
    retire_player(&builder, &player_state, player_output, &owner);
}

#[test]
fn player_cannot_retire_with_open_games() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let owner = player(0x80);
    let mut player_state = PlayerStateData::registered(&owner);
    player_state.open_games = 1;
    let player_output = actor_fixture(&builder, "Player", player_state.source_state(), 2_000, 0xa7);

    assert!(build_player_retirement(&builder, &player_state, player_output, &owner).is_err());
}

#[test]
fn players_can_start_a_real_mux_game() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x81);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let (league, white, white_state, white_output) = register_player(&builder, league_values.clone(), league, 0x82, 2_000);
    let (_league, black, black_state, black_output) = register_player(&builder, league_values, league, 0x83, 2_000);

    let started = start_game(&builder, (&white, &white_state, white_output), (&black, &black_state, black_output), WHITE);
    assert_eq!(started.leader_state.open_games, 1);
    assert_eq!(started.other_state.open_games, 1);
    execute_signed_rebalance(&builder, "Player", started.leader_state.source_state(), started.leader_output, &white);
    execute_signed_rebalance(&builder, "Player", started.other_state.source_state(), started.other_output, &black);

    let move_spec = MoveSpec::new(4, 1, 4, 3);
    let (pawn_state, pawn_output) =
        route_mux_output_to_worker(&builder, &white, "Pawn", &started.game_state, started.game_output, move_spec, CLEAR);
    let mut expected = started.game_state.completed_move(move_spec);
    expected.en_passant_idx = 20;
    execute_actor_transition(&builder, "Pawn", &pawn_state, "apply", pawn_output, "Mux", &expected);
}

#[test]
fn terminal_mux_settles_black_win_back_into_players() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x76);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let (league, white, white_state, white_output) = register_player(&builder, league_values.clone(), league, 0x77, 2_000);
    let (_league, black, black_state, black_output) = register_player(&builder, league_values, league, 0x78, 2_000);
    let started = start_game(&builder, (&white, &white_state, white_output), (&black, &black_state, black_output), WHITE);

    let mut terminal_state = started.game_state.clone();
    terminal_state.status = BWIN;
    let terminal_output = terminate_mux_output(&builder, &white, &started.game_state, SURRENDER, &terminal_state, started.game_output);
    let settlement = route_game_to_settle(&builder, &terminal_state, terminal_output);
    let settled = settle_game(
        &builder,
        settlement,
        BWIN,
        (&started.leader_state, started.leader_output),
        (&started.other_state, started.other_output),
    );

    assert_eq!((settled.white_state.open_games, settled.white_state.rating), (0, BASE_RATING - 16));
    assert_eq!((settled.white_state.games, settled.white_state.losses), (1, 1));
    assert_eq!((settled.black_state.open_games, settled.black_state.rating), (0, BASE_RATING + 16));
    assert_eq!((settled.black_state.games, settled.black_state.wins), (1, 1));
    assert_eq!(settled.white_output.utxo.amount, 2_000);
    assert_eq!(settled.black_output.utxo.amount, 3_000);
    execute_signed_rebalance(&builder, "Player", settled.white_state.source_state(), settled.white_output, &white);
    execute_signed_rebalance(&builder, "Player", settled.black_state.source_state(), settled.black_output, &black);
}

#[test]
fn terminal_mux_settles_white_win_back_into_players() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let fixture = settlement_fixture(&builder, WWIN, GAME_VALUE, 0xb1);
    let settled = settle_game(
        &builder,
        fixture.settlement,
        WWIN,
        (&fixture.white_state, fixture.white_output),
        (&fixture.black_state, fixture.black_output),
    );

    assert_eq!(
        (
            settled.white_state.open_games,
            settled.white_state.rating,
            settled.white_state.games,
            settled.white_state.wins,
            settled.white_state.draws,
            settled.white_state.losses,
        ),
        (0, BASE_RATING + 16, 11, 7, 2, 2)
    );
    assert_eq!(
        (
            settled.black_state.open_games,
            settled.black_state.rating,
            settled.black_state.games,
            settled.black_state.wins,
            settled.black_state.draws,
            settled.black_state.losses,
        ),
        (0, BASE_RATING - 16, 11, 2, 2, 7)
    );
    assert_eq!((settled.white_output.utxo.amount, settled.black_output.utxo.amount), (2_000, 1_000));
}

#[test]
fn terminal_mux_settles_draw_back_into_players() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let fixture = settlement_fixture(&builder, DRAW, GAME_VALUE, 0xb7);
    let settled = settle_game(
        &builder,
        fixture.settlement,
        DRAW,
        (&fixture.white_state, fixture.white_output),
        (&fixture.black_state, fixture.black_output),
    );

    assert_eq!(
        (
            settled.white_state.open_games,
            settled.white_state.rating,
            settled.white_state.games,
            settled.white_state.wins,
            settled.white_state.draws,
            settled.white_state.losses,
        ),
        (0, BASE_RATING, 11, 6, 3, 2)
    );
    assert_eq!(
        (
            settled.black_state.open_games,
            settled.black_state.rating,
            settled.black_state.games,
            settled.black_state.wins,
            settled.black_state.draws,
            settled.black_state.losses,
        ),
        (0, BASE_RATING, 11, 2, 3, 6)
    );
    assert_eq!((settled.white_output.utxo.amount, settled.black_output.utxo.amount), (1_500, 1_500));
}

#[test]
fn draw_settlement_allows_black_to_take_the_odd_extra() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let fixture = settlement_fixture(&builder, DRAW, GAME_VALUE + 1, 0xbd);
    let settled = settle_game(
        &builder,
        fixture.settlement,
        DRAW,
        (&fixture.white_state, fixture.white_output),
        (&fixture.black_state, fixture.black_output),
    );

    assert_eq!((settled.white_output.utxo.amount, settled.black_output.utxo.amount), (1_500, 1_501));
}

#[test]
fn draw_settlement_rejects_the_wrong_odd_split() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let fixture = settlement_fixture(&builder, DRAW, GAME_VALUE + 1, 0xc3);
    let mut wrong = expected_settlement(
        &fixture.white_state,
        fixture.white_output.utxo.amount,
        &fixture.black_state,
        fixture.black_output.utxo.amount,
        DRAW,
        fixture.settlement.utxo.amount,
    );
    wrong.white_value += 1;
    wrong.black_value -= 1;

    assert!(build_settlement(
        &builder,
        fixture.settlement,
        DRAW,
        (&fixture.white_state, fixture.white_output),
        (&fixture.black_state, fixture.black_output),
        &wrong,
    )
    .is_err());
}

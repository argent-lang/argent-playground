use std::collections::BTreeMap;

use argent_runtime::{actor, args, state, Artifact, ArtifactValue, CovenantOutput, EntryCall, TxBuilder, TxContext};
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
const NORMAL: i64 = 3;
const WOFFER: i64 = 4;
const BOFFER: i64 = 5;
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

fn execute_mux_terminate(
    builder: &TxBuilder<'_>,
    player: &TestPlayer,
    initial: &GameStateData,
    termination_action: i64,
    expected: &GameStateData,
    fixture_tag: u8,
) {
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
    );
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

fn register_player(
    builder: &TxBuilder<'_>,
    league_state: BTreeMap<String, ArtifactValue>,
    league: CovenantOutput,
    owner_seed: u8,
    player_value: u64,
) -> (CovenantOutput, TestPlayer, PlayerStateData, CovenantOutput) {
    let mut unique_preimage = b"LeaguePlayerId".to_vec();
    unique_preimage.extend_from_slice(league.outpoint.transaction_id.as_bytes().as_slice());
    unique_preimage.extend_from_slice(&league.outpoint.index.to_le_bytes());
    let owner = player_with_id(owner_seed, blake2b32(&unique_preimage));
    let player_state = PlayerStateData::registered(&owner);
    let league_value = league.utxo.amount;
    let covenant_id = league.covenant_id;
    let keypair = owner.keypair;
    let public_key = owner.public_key.clone();
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
        .actor_output("League", league_state, CovenantBinding::new(0, covenant_id), league_value)
        .actor_output("Player", player_state.source_state(), CovenantBinding::new(0, covenant_id), player_value);
    let tx = builder.build(&context).expect("league registers a signed player");
    let next_league = CovenantOutput::from_tx(&tx, 0).expect("league continuation is a covenant UTXO");
    let player_output = CovenantOutput::from_tx(&tx, 1).expect("registered player is a covenant UTXO");
    (next_league, owner, player_state, player_output)
}

fn execute_signed_rebalance(
    builder: &TxBuilder<'_>,
    actor: &str,
    state: BTreeMap<String, ArtifactValue>,
    source: CovenantOutput,
    signer: &TestPlayer,
) -> CovenantOutput {
    let value = source.utxo.amount;
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
        .actor_output(actor, state, CovenantBinding::new(0, covenant_id), value);
    let tx = builder.build(&context).unwrap_or_else(|err| panic!("{actor} rebalance must execute: {err}"));
    CovenantOutput::from_tx(&tx, 0).expect("rebalance output is a covenant UTXO")
}

fn fork_league(
    builder: &TxBuilder<'_>,
    league_state: BTreeMap<String, ArtifactValue>,
    league: CovenantOutput,
    admin: &TestPlayer,
) -> (CovenantOutput, CovenantOutput) {
    let value = league.utxo.amount;
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
        .actor_output("League", league_state.clone(), CovenantBinding::new(0, covenant_id), value)
        .actor_output("League", league_state, CovenantBinding::new(0, covenant_id), value);
    let tx = builder.build(&context).expect("league forks into two identical lanes");
    let left = CovenantOutput::from_tx(&tx, 0).expect("left league lane is a covenant UTXO");
    let right = CovenantOutput::from_tx(&tx, 1).expect("right league lane is a covenant UTXO");
    (left, right)
}

fn retire_player(builder: &TxBuilder<'_>, player_state: &PlayerStateData, player_output: CovenantOutput, owner: &TestPlayer) {
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
    builder.build(&context).expect("idle player retires without a covenant output");
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

fn settle_black_win(
    builder: &TxBuilder<'_>,
    settlement: CovenantOutput,
    white: (&PlayerStateData, CovenantOutput),
    black: (&PlayerStateData, CovenantOutput),
) -> SettledGame {
    let (white_state, white_output) = white;
    let (black_state, black_output) = black;
    assert_eq!(white_state.rating, black_state.rating, "this fixture expects equal initial ratings");
    let covenant_id = settlement.covenant_id;
    let mut next_white = white_state.clone();
    next_white.open_games -= 1;
    next_white.rating -= 16;
    next_white.games += 1;
    next_white.losses += 1;
    let mut next_black = black_state.clone();
    next_black.open_games -= 1;
    next_black.rating += 16;
    next_black.games += 1;
    next_black.wins += 1;
    let white_value = white_output.utxo.amount;
    let black_value = black_output.utxo.amount + settlement.utxo.amount;
    let settlement_state = settle_state(
        blake2b32(&[white_state.owner.as_slice(), white_state.player_id.as_slice()].concat()),
        blake2b32(&[black_state.owner.as_slice(), black_state.player_id.as_slice()].concat()),
        BWIN,
    );
    let context = TxContext::new()
        .actor_input("Settle", settlement_state, "settle", settlement.outpoint, settlement.utxo, 0)
        .actor_input("Player", white_state.source_state(), "delegate_settle", white_output.outpoint, white_output.utxo, 0)
        .actor_input("Player", black_state.source_state(), "delegate_settle", black_output.outpoint, black_output.utxo, 0)
        .actor_output("Player", next_white.source_state(), CovenantBinding::new(0, covenant_id), white_value)
        .actor_output("Player", next_black.source_state(), CovenantBinding::new(0, covenant_id), black_value);
    let tx = builder.build(&context).expect("settlement updates both delegated players");
    let white_output = CovenantOutput::from_tx(&tx, 0).expect("settled white player is a covenant UTXO");
    let black_output = CovenantOutput::from_tx(&tx, 1).expect("settled black player is a covenant UTXO");
    SettledGame { white_state: next_white, white_output, black_state: next_black, black_output }
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

#[test]
fn argent_ordinary_workers_round_trip_through_mux() {
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
fn argent_castles_all_four_shapes() {
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
fn argent_castle_challenge_routes_through_prep_and_piece_worker() {
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

    let (prep_state, prep_output) = route_to_worker(&builder, &black, "CastleChallengePrep", &mux_state, challenge_move, 0x79);
    let mut pawn_state = prep_state.clone();
    pawn_state.board = vec![0; 64];
    pawn_state.board[4] = 0x06;
    pawn_state.board[7] = 0x04;
    pawn_state.board[11] = 0x09;
    let pawn_output = execute_actor_transition(
        &builder,
        "CastleChallengePrep",
        &prep_state,
        EntryCall::new("apply").args(args![actor("Pawn")]),
        prep_output,
        "Pawn",
        &pawn_state,
    );

    let mut expected = pawn_state.completed_move(challenge_move);
    expected.status = BWIN;
    execute_actor_transition(&builder, "Pawn", &pawn_state, "apply", pawn_output, "Mux", &expected);
}

#[test]
fn argent_draw_offer_survives_an_ordinary_worker_round_trip() {
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
fn argent_mux_executes_claim_surrender_and_draw_acceptance() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x53);
    let initial = GameStateData::live(white.player_ref, [0x54; 32], opening_board());

    let mut claimed = initial.clone();
    claimed.turn = BLACK;
    claimed.draw_state = CLAIMED;
    execute_mux_terminate(&builder, &white, &initial, CLAIM, &claimed, 0x83);

    let mut surrendered = initial.clone();
    surrendered.status = BWIN;
    execute_mux_terminate(&builder, &white, &initial, SURRENDER, &surrendered, 0x85);

    let mut offered = initial.clone();
    offered.draw_state = BOFFER;
    let mut accepted = offered.clone();
    accepted.status = DRAW;
    accepted.draw_state = NORMAL;
    execute_mux_terminate(&builder, &white, &offered, ACCEPT, &accepted, 0x87);
}

#[test]
fn argent_worker_and_mux_paths_exit_the_family_into_settlement() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x61);
    let black = player(0x62);

    let initial = GameStateData::live(white.player_ref, black.player_ref, opening_board());
    let invalid_knight_move = MoveSpec::new(0, 1, 0, 2);
    let knight_state = initial.committed_route("Knight", invalid_knight_move, CLEAR);
    execute_to_settle(&builder, "Knight", &knight_state, "timeout", MOVE_TIMEOUT as u64, BWIN, 0x91);

    let keypair = black.keypair;
    let public_key = black.public_key.clone();
    let player_id = black.player_id;
    let mux_timeout = EntryCall::new("timeout")
        .args_with(move |tx, input_index| args![sign_input(tx, input_index, &keypair), public_key.clone(), player_id]);
    execute_to_settle(&builder, "Mux", &initial, mux_timeout, MOVE_TIMEOUT as u64, BWIN, 0x93);

    let mut terminal = initial;
    terminal.status = BWIN;
    execute_to_settle(&builder, "Mux", &terminal, "settle", 0, BWIN, 0x95);
}

#[test]
fn argent_league_registers_a_spendable_player() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x71);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let (league, owner, player_state, player_output) = register_player(&builder, league_values.clone(), league, 0x72, 2_000);

    execute_signed_rebalance(&builder, "League", league_values, league, &admin);
    execute_signed_rebalance(&builder, "Player", player_state.source_state(), player_output, &owner);
}

#[test]
fn argent_league_forks_and_idle_player_retires() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x79);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let (league, owner, player_state, player_output) = register_player(&builder, league_values.clone(), league, 0x7a, 2_000);

    let (left, right) = fork_league(&builder, league_values.clone(), league, &admin);
    execute_signed_rebalance(&builder, "League", league_values.clone(), left, &admin);
    execute_signed_rebalance(&builder, "League", league_values, right, &admin);
    retire_player(&builder, &player_state, player_output, &owner);
}

#[test]
fn argent_registered_players_start_a_spendable_game() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let admin = player(0x73);
    let league_values = league_state(admin.owner);
    let league = launch_league(&builder, league_values.clone(), 5_000);
    let (league, white, white_state, white_output) = register_player(&builder, league_values.clone(), league, 0x74, 2_000);
    let (_league, black, black_state, black_output) = register_player(&builder, league_values, league, 0x75, 2_000);

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
fn argent_game_settles_back_into_spendable_players() {
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
    let settled = settle_black_win(
        &builder,
        settlement,
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

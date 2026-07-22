use std::collections::BTreeMap;

use argent_runtime::{actor, args, state, Artifact, ArtifactValue, CovenantOutput, EntryCall, TxBuilder, TxContext};
use blake2b_simd::Params as Blake2bParams;
use kaspa_consensus_core::{
    hashing::{
        sighash::{calc_schnorr_signature_hash, SigHashReusedValuesUnsync},
        sighash_type::SIG_HASH_ALL,
    },
    tx::{CovenantBinding, MutableTransaction, Transaction, TransactionId, TransactionOutpoint},
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

struct TestPlayer {
    keypair: Keypair,
    public_key: Vec<u8>,
    player_id: [u8; 32],
    player_ref: [u8; 32],
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
        if target != "ChessCastleChallengePrep" {
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
    serde_json::from_str(include_str!("../../build/argent/artifact.json")).expect("pinned chess artifact deserializes")
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
    TestPlayer { keypair, public_key, player_id, player_ref }
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
    execute_actor_transition(builder, worker, &worker_state, "apply", worker_output, "ChessMux", expected);
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
    let worker_state = initial.committed_route(worker, mv, termination_action);
    let mux_outpoint = TransactionOutpoint::new(TransactionId::from_bytes([fixture_tag.wrapping_add(1); 32]), 0);
    let mux_utxo = builder
        .covenant_utxo("ChessMux", mux_state.clone(), GAME_VALUE, 0, false, Some(covenant_id))
        .unwrap_or_else(|err| panic!("{worker} mux UTXO must build: {err}"));
    let selected_worker = worker.to_string();
    let keypair = player.keypair;
    let public_key = player.public_key.clone();
    let player_id = player.player_id;

    let route_context = TxContext::new()
        .actor_input(
            "ChessMux",
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
            mux_outpoint,
            mux_utxo,
            0,
        )
        .actor_output(worker, worker_state.source_state(), CovenantBinding::new(0, covenant_id), GAME_VALUE);
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
        .covenant_utxo("ChessMux", mux_state.clone(), GAME_VALUE, 0, false, Some(covenant_id))
        .expect("mux UTXO builds from source state");
    let keypair = player.keypair;
    let public_key = player.public_key.clone();
    let player_id = player.player_id;
    let context = TxContext::new()
        .actor_input(
            "ChessMux",
            mux_state,
            EntryCall::new("terminate").args_with(move |tx, input_index| {
                args![termination_action, sign_input(tx, input_index, &keypair), public_key.clone(), player_id]
            }),
            outpoint,
            utxo,
            0,
        )
        .actor_output("ChessMux", expected.source_state(), CovenantBinding::new(0, covenant_id), GAME_VALUE);
    builder.build(&context).unwrap_or_else(|err| panic!("mux terminate action {termination_action} must execute: {err}"));
}

fn settle_state(white_player: [u8; 32], black_player: [u8; 32], status: i64) -> BTreeMap<String, ArtifactValue> {
    state! {
        white_player: white_player,
        black_player: black_player,
        status: status,
    }
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
        "ChessSettle",
        settle_state(source_state.white_player, source_state.black_player, settle_status),
        CovenantBinding::new(0, covenant_id),
        GAME_VALUE,
    );
    builder.build(&context).unwrap_or_else(|err| panic!("{source_actor} must transition to ChessSettle: {err}"));
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
    execute_worker_round_trip(&builder, &white, "ChessPawn", &pawn_initial, pawn_move, &pawn_expected, 0x61);

    let knight_move = MoveSpec::new(1, 0, 2, 2);
    let knight_initial = GameStateData::live(white.player_ref, black_player_ref, opening_board());
    let knight_expected = knight_initial.completed_move(knight_move);
    execute_worker_round_trip(&builder, &white, "ChessKnight", &knight_initial, knight_move, &knight_expected, 0x63);

    let mut vert_board = vec![0; 64];
    vert_board[0] = 0x04;
    let vert_move = MoveSpec::new(0, 0, 0, 3);
    let vert_initial = GameStateData::live(white.player_ref, black_player_ref, vert_board);
    let mut vert_expected = vert_initial.completed_move(vert_move);
    vert_expected.castle_rights = [1, 0, 1, 1];
    execute_worker_round_trip(&builder, &white, "ChessVert", &vert_initial, vert_move, &vert_expected, 0x65);

    let mut horiz_board = vec![0; 64];
    horiz_board[24] = 0x04;
    let horiz_move = MoveSpec::new(0, 3, 3, 3);
    let horiz_initial = GameStateData::live(white.player_ref, black_player_ref, horiz_board);
    let horiz_expected = horiz_initial.completed_move(horiz_move);
    execute_worker_round_trip(&builder, &white, "ChessHoriz", &horiz_initial, horiz_move, &horiz_expected, 0x67);

    let mut diag_board = vec![0; 64];
    diag_board[0] = 0x03;
    let diag_move = MoveSpec::new(0, 0, 3, 3);
    let diag_initial = GameStateData::live(white.player_ref, black_player_ref, diag_board);
    let diag_expected = diag_initial.completed_move(diag_move);
    execute_worker_round_trip(&builder, &white, "ChessDiag", &diag_initial, diag_move, &diag_expected, 0x69);

    let mut king_board = vec![0; 64];
    king_board[4] = 0x06;
    let king_move = MoveSpec::new(4, 0, 4, 1);
    let king_initial = GameStateData::live(white.player_ref, black_player_ref, king_board);
    let mut king_expected = king_initial.completed_move(king_move);
    king_expected.castle_rights = [0, 0, 1, 1];
    execute_worker_round_trip(&builder, &white, "ChessKing", &king_initial, king_move, &king_expected, 0x6b);
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
        execute_worker_round_trip(&builder, mover, "ChessCastle", &initial, case.mv, &expected, case.fixture_tag);
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

    let (prep_state, prep_output) = route_to_worker(&builder, &black, "ChessCastleChallengePrep", &mux_state, challenge_move, 0x79);
    let mut pawn_state = prep_state.clone();
    pawn_state.board = vec![0; 64];
    pawn_state.board[4] = 0x06;
    pawn_state.board[7] = 0x04;
    pawn_state.board[11] = 0x09;
    let pawn_output = execute_actor_transition(
        &builder,
        "ChessCastleChallengePrep",
        &prep_state,
        EntryCall::new("apply").args(args![actor("ChessPawn")]),
        prep_output,
        "ChessPawn",
        &pawn_state,
    );

    let mut expected = pawn_state.completed_move(challenge_move);
    expected.status = BWIN;
    execute_actor_transition(&builder, "ChessPawn", &pawn_state, "apply", pawn_output, "ChessMux", &expected);
}

#[test]
fn argent_draw_offer_survives_an_ordinary_worker_round_trip() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let white = player(0x51);
    let move_spec = MoveSpec::new(4, 1, 4, 3);
    let initial = GameStateData::live(white.player_ref, [0x52; 32], opening_board());
    let (pawn_state, pawn_output) = route_to_worker_with_action(&builder, &white, "ChessPawn", &initial, move_spec, OFFER, 0x81);
    assert_eq!(pawn_state.draw_state, WOFFER);

    let mut expected = initial.completed_move(move_spec);
    expected.en_passant_idx = 20;
    expected.draw_state = WOFFER;
    execute_actor_transition(&builder, "ChessPawn", &pawn_state, "apply", pawn_output, "ChessMux", &expected);
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
    let knight_state = initial.committed_route("ChessKnight", invalid_knight_move, CLEAR);
    execute_to_settle(&builder, "ChessKnight", &knight_state, "timeout", MOVE_TIMEOUT as u64, BWIN, 0x91);

    let keypair = black.keypair;
    let public_key = black.public_key.clone();
    let player_id = black.player_id;
    let mux_timeout = EntryCall::new("timeout")
        .args_with(move |tx, input_index| args![sign_input(tx, input_index, &keypair), public_key.clone(), player_id]);
    execute_to_settle(&builder, "ChessMux", &initial, mux_timeout, MOVE_TIMEOUT as u64, BWIN, 0x93);

    let mut terminal = initial;
    terminal.status = BWIN;
    execute_to_settle(&builder, "ChessMux", &terminal, "settle", 0, BWIN, 0x95);
}

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
const WHITE: i64 = 0;
const NORMAL: i64 = 3;
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

    fn committed_move(&self, mv: MoveSpec) -> Self {
        Self { pending_src_idx: mv.source_idx(), pending_dst_idx: mv.destination_idx(), pending_promo: mv.promo_piece, ..self.clone() }
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
    let covenant_id = Hash::from_bytes([fixture_tag; 32]);
    let mux_state = initial.source_state();
    let worker_state = initial.committed_move(mv).source_state();
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
                    0,
                    sign_input(tx, input_index, &keypair),
                    public_key.clone(),
                    player_id,
                ]
            }),
            mux_outpoint,
            mux_utxo,
            0,
        )
        .actor_output(worker, worker_state.clone(), CovenantBinding::new(0, covenant_id), GAME_VALUE);
    let route_tx = builder.build(&route_context).unwrap_or_else(|err| panic!("mux must route a signed move to {worker}: {err}"));
    let worker_output = CovenantOutput::from_tx(&route_tx, 0).expect("route output is a covenant UTXO");

    let apply_context = TxContext::new()
        .actor_input(worker, worker_state, "apply", worker_output.outpoint, worker_output.utxo, 0)
        .actor_output("ChessMux", expected.source_state(), CovenantBinding::new(0, worker_output.covenant_id), GAME_VALUE);
    builder.build(&apply_context).unwrap_or_else(|err| panic!("{worker} must validate the move and return to mux: {err}"));
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

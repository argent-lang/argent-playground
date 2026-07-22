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
const BLACK: i64 = 1;
const NORMAL: i64 = 3;
const MOVE_TIMEOUT: i64 = 600;
const GAME_VALUE: u64 = 1_000;

fn chess_artifact() -> Artifact {
    serde_json::from_str(include_str!("../../build/argent/artifact.json")).expect("pinned chess artifact deserializes")
}

fn blake2b32(bytes: &[u8]) -> [u8; 32] {
    Blake2bParams::new().hash_length(32).to_state().update(bytes).finalize().as_bytes().try_into().expect("Blake2b output is 32 bytes")
}

fn player(seed: u8) -> (Keypair, Vec<u8>, [u8; 32], [u8; 32]) {
    let secret = SecretKey::from_slice(&[seed; 32]).expect("deterministic secret key is valid");
    let keypair = Keypair::from_secret_key(&Secp256k1::new(), &secret);
    let public_key = keypair.x_only_public_key().0.serialize().to_vec();
    let player_id = blake2b32(&[b"argent-chess-player".as_slice(), public_key.as_slice()].concat());
    let owner = blake2b32(&public_key);
    let player_ref = blake2b32(&[owner.as_slice(), player_id.as_slice()].concat());
    (keypair, public_key, player_id, player_ref)
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

fn game_state(
    white_player: [u8; 32],
    black_player: [u8; 32],
    board: Vec<u8>,
    turn: i64,
    pending_src_idx: i64,
    pending_dst_idx: i64,
) -> BTreeMap<String, ArtifactValue> {
    state! {
        white_player: white_player,
        black_player: black_player,
        board: board,
        turn: turn,
        status: LIVE,
        move_timeout: MOVE_TIMEOUT,
        castle_rights: [1_u8, 1, 1, 1],
        en_passant_idx: -1,
        pending_src_idx: pending_src_idx,
        pending_dst_idx: pending_dst_idx,
        pending_promo: 0,
        recent_castle: 0,
        draw_state: NORMAL,
    }
}

#[test]
fn argent_mux_routes_to_knight_and_knight_returns_to_mux() {
    let artifact = chess_artifact();
    let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
    let (white_keypair, white_public_key, white_player_id, white_player_ref) = player(0x21);
    let black_player_ref = [0x42; 32];
    let covenant_id = Hash::from_bytes([0x61; 32]);

    let board = opening_board();
    let mux_state = game_state(white_player_ref, black_player_ref, board.clone(), WHITE, -1, -1);
    let knight_state = game_state(white_player_ref, black_player_ref, board.clone(), WHITE, 1, 18);
    let mux_outpoint = TransactionOutpoint::new(TransactionId::from_bytes([0x62; 32]), 0);
    let mux_utxo = builder
        .covenant_utxo("ChessMux", mux_state.clone(), GAME_VALUE, 0, false, Some(covenant_id))
        .expect("mux UTXO builds from source state");

    let route_context = TxContext::new()
        .actor_input(
            "ChessMux",
            mux_state,
            EntryCall::new("route").args_with(move |tx, input_index| {
                args![
                    actor("ChessKnight"),
                    1,
                    0,
                    2,
                    2,
                    0,
                    0,
                    sign_input(tx, input_index, &white_keypair),
                    white_public_key.clone(),
                    white_player_id,
                ]
            }),
            mux_outpoint,
            mux_utxo,
            0,
        )
        .actor_output("ChessKnight", knight_state.clone(), CovenantBinding::new(0, covenant_id), GAME_VALUE);
    let route_tx = builder.build(&route_context).expect("mux routes a signed move to the selected knight worker");
    let knight_output = CovenantOutput::from_tx(&route_tx, 0).expect("route output is a covenant UTXO");

    let mut moved_board = board;
    moved_board[1] = 0;
    moved_board[18] = 0x02;
    let next_mux_state = game_state(white_player_ref, black_player_ref, moved_board, BLACK, -1, -1);
    let apply_context = TxContext::new()
        .actor_input("ChessKnight", knight_state, "apply", knight_output.outpoint, knight_output.utxo, 0)
        .actor_output("ChessMux", next_mux_state, CovenantBinding::new(0, knight_output.covenant_id), GAME_VALUE);

    builder.build(&apply_context).expect("knight validates the move and returns the updated state to mux");
}

use kaspa_consensus_core::hashing::sighash::SigHashReusedValuesUnsync;
use kaspa_consensus_core::tx::PopulatedTransaction;
use kaspa_consensus_core::Hash;
use kaspa_txscript::parse_script;
use silverscript_lang::ast::Expr;
use silverscript_lang::compiler::{compile_contract, CompileOptions};

use chess_covenant::{
    castle_challenge_contract_path, castle_contract_path, diag_contract_path, horiz_contract_path, king_contract_path,
    knight_contract_path, load_contract_source, mux_contract_path, pawn_contract_path, settle_contract_path, vert_contract_path,
};

const LEAGUE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/league.sil");
const PLAYER_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/player.sil");
const DEFAULT_MOVE_TIMEOUT: i64 = 600;

#[test]
fn isolated_rook_path_loop_reports_script_size() {
    let source = isolated_rook_path_source();
    let compiled = compile_contract(source, &isolated_rook_path_constructor_args(5), CompileOptions::default())
        .expect("isolated rook path should compile");

    eprintln!("isolated_rook_path script_len={}", compiled.script.len());
    assert!(!compiled.script.is_empty());
}

#[test]
fn isolated_rook_path_loop_bound_sweep() {
    for bound in 1..=7 {
        let source = isolated_rook_path_source();
        match compile_contract(source, &isolated_rook_path_constructor_args(bound), CompileOptions::default()) {
            Ok(compiled) => eprintln!("isolated_rook_path bound={bound} script_len={}", compiled.script.len()),
            Err(err) => eprintln!("isolated_rook_path bound={bound} compile_error={err}"),
        }
    }
}

#[test]
fn chess_pawn_reports_script_size_and_opcode_count() {
    let (script_len, instruction_count, charged_op_count) =
        contract_metrics("chess_pawn", pawn_contract_path(), &pawn_constructor_args());
    eprintln!("chess_pawn script_len={} instruction_count={} charged_op_count={}", script_len, instruction_count, charged_op_count);
    assert!(script_len > 0);
}

#[test]
fn chess_mux_reports_script_size_and_opcode_count() {
    let (script_len, instruction_count, charged_op_count) =
        contract_metrics("chess_mux", mux_contract_path(), &mux_constructor_args());
    eprintln!("chess_mux script_len={} instruction_count={} charged_op_count={}", script_len, instruction_count, charged_op_count);
    assert!(script_len > 0);
}

#[test]
fn chess_settle_reports_script_size_and_opcode_count() {
    let (script_len, instruction_count, charged_op_count) =
        contract_metrics("chess_settle", settle_contract_path(), &settle_constructor_args());
    eprintln!("chess_settle script_len={} instruction_count={} charged_op_count={}", script_len, instruction_count, charged_op_count);
    assert!(script_len > 0);
}

#[test]
fn chess_workers_report_script_size_and_opcode_count() {
    let workers = [
        ("knight", knight_contract_path()),
        ("king", king_contract_path()),
        ("vert", vert_contract_path()),
        ("horiz", horiz_contract_path()),
        ("diag", diag_contract_path()),
        ("castle", castle_contract_path()),
        ("castle_challenge", castle_challenge_contract_path()),
    ];

    for (name, path) in workers {
        let (script_len, instruction_count, charged_op_count) = contract_metrics(name, path, &pawn_constructor_args());
        eprintln!("{name} script_len={} instruction_count={} charged_op_count={}", script_len, instruction_count, charged_op_count);
    }
}

#[test]
fn chess_player_reports_script_size_and_opcode_count() {
    let (script_len, instruction_count, charged_op_count) = contract_metrics("chess_player", PLAYER_PATH, &player_constructor_args());
    eprintln!("chess_player script_len={} instruction_count={} charged_op_count={}", script_len, instruction_count, charged_op_count);
    assert!(script_len > 0);
}

#[test]
fn chess_league_reports_script_size_and_opcode_count() {
    let (script_len, instruction_count, charged_op_count) = contract_metrics("chess_league", LEAGUE_PATH, &league_constructor_args());
    eprintln!("chess_league script_len={} instruction_count={} charged_op_count={}", script_len, instruction_count, charged_op_count);
    assert!(script_len > 0);
}

#[test]
fn chess_all_contracts_report_script_size_and_opcode_count() {
    let reports = [
        ("chess_league", LEAGUE_PATH, league_constructor_args()),
        ("chess_player", PLAYER_PATH, player_constructor_args()),
        ("chess_mux", mux_contract_path(), mux_constructor_args()),
        ("chess_settle", settle_contract_path(), settle_constructor_args()),
        ("chess_pawn", pawn_contract_path(), pawn_constructor_args()),
        ("chess_knight", knight_contract_path(), pawn_constructor_args()),
        ("chess_vert", vert_contract_path(), pawn_constructor_args()),
        ("chess_horiz", horiz_contract_path(), pawn_constructor_args()),
        ("chess_diag", diag_contract_path(), pawn_constructor_args()),
        ("chess_king", king_contract_path(), pawn_constructor_args()),
        ("chess_castle", castle_contract_path(), pawn_constructor_args()),
        ("chess_castle_challenge", castle_challenge_contract_path(), pawn_constructor_args()),
    ];

    for (name, path, args) in reports {
        let (script_len, instruction_count, charged_op_count) = contract_metrics(name, path, &args);
        eprintln!("{name} script_len={} instruction_count={} charged_op_count={}", script_len, instruction_count, charged_op_count);
    }
}

fn isolated_rook_path_source() -> &'static str {
    r#"
pragma silverscript ^0.1.0;

contract Probe(byte[64] init_board, int path_bound) {
    byte[64] board = init_board;

    function rook_path_clear(
        byte[64] board_data,
        int from_x,
        int from_y,
        int to_x,
        int to_y
    ) : (int) {
        int step_x = 0;
        if (to_x > from_x) {
            step_x = 1;
        } else if (to_x < from_x) {
            step_x = -1;
        }

        int step_y = 0;
        if (to_y > from_y) {
            step_y = 1;
        } else if (to_y < from_y) {
            step_y = -1;
        }

        int x = from_x + step_x;
        int y = from_y + step_y;
        int clear = 1;

        for (i, 0, path_bound, path_bound) {
            bool at_target = x == to_x && y == to_y;
            if (clear == 1 && !at_target) {
                int idx = y * 8 + x;
                if (OpBin2Num(board_data[idx]) != 0) {
                    clear = 0;
                }
                x = x + step_x;
                y = y + step_y;
            }
        }

        return(clear);
    }

    entrypoint function main() {
        byte[64] board_data = board;
        (int clear) = rook_path_clear(board_data, 0, 0, 0, 7);
        require(clear == 0 || clear == 1);
    }
}
"#
}

fn isolated_rook_path_constructor_args(bound: usize) -> Vec<Expr<'static>> {
    vec![Expr::bytes(vec![0u8; 64]), Expr::int(bound as i64)]
}

fn standard_board() -> Vec<u8> {
    vec![
        0x04, 0x02, 0x03, 0x05, 0x06, 0x03, 0x02, 0x04, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x0c, 0x0a, 0x0b, 0x0d, 0x0e, 0x0b, 0x0a,
        0x0c,
    ]
}

fn sample_route_templates() -> Vec<u8> {
    let mut route_templates = Vec::with_capacity(32 * 9);
    for byte in 0x12u8..=0x1au8 {
        route_templates.extend_from_slice(&[byte; 32]);
    }
    route_templates
}

fn sample_routes_commitment() -> Hash {
    blake2b(sample_route_templates())
}

fn pawn_constructor_args() -> Vec<Expr<'static>> {
    let standard_board = standard_board();
    let route_templates = sample_route_templates();

    vec![
        Expr::bytes(vec![0x11u8; 32]),
        Expr::bytes(route_templates),
        Expr::bytes(vec![0x21u8; 32]),
        Expr::bytes(vec![0x22u8; 32]),
        Expr::bytes(standard_board),
        Expr::int(0),
        Expr::int(0),
        Expr::int(DEFAULT_MOVE_TIMEOUT),
        Expr::bytes(vec![1u8; 4]),
        Expr::int(-1),
        Expr::int(12),
        Expr::int(28),
        Expr::int(0),
        Expr::int(0),
        Expr::int(3),
    ]
}

fn mux_constructor_args() -> Vec<Expr<'static>> {
    let route_templates = sample_route_templates();

    vec![
        Expr::bytes(vec![0x11u8; 32]),
        Expr::bytes(route_templates),
        Expr::bytes(vec![0x21u8; 32]),
        Expr::bytes(vec![0x22u8; 32]),
        Expr::bytes(vec![0u8; 64]),
        Expr::int(0),
        Expr::int(0),
        Expr::int(DEFAULT_MOVE_TIMEOUT),
        Expr::bytes(vec![1u8; 4]),
        Expr::int(-1),
        Expr::int(-1),
        Expr::int(-1),
        Expr::int(0),
        Expr::int(0),
        Expr::int(3),
    ]
}

fn settle_constructor_args() -> Vec<Expr<'static>> {
    vec![Expr::bytes(vec![0x31u8; 32]), Expr::bytes(vec![0x21u8; 32]), Expr::bytes(vec![0x22u8; 32]), Expr::int(1)]
}

fn player_constructor_args() -> Vec<Expr<'static>> {
    vec![
        Expr::bytes(vec![0x11u8; 32]),
        Expr::bytes(vec![0x22u8; 32]),
        Expr::bytes(vec![0x33u8; 32]),
        Expr::bytes(sample_routes_commitment().as_bytes().to_vec()),
        Expr::bytes(vec![0x44u8; 32]),
        Expr::bytes(vec![0x55u8; 32]),
        Expr::int(0),
        Expr::int(1200),
        Expr::int(7),
        Expr::int(4),
        Expr::int(2),
        Expr::int(1),
    ]
}

fn league_constructor_args() -> Vec<Expr<'static>> {
    vec![
        Expr::bytes(vec![0x11u8; 32]),
        Expr::bytes(vec![0x22u8; 32]),
        Expr::bytes(vec![0x33u8; 32]),
        Expr::bytes(sample_routes_commitment().as_bytes().to_vec()),
        Expr::int(1200),
        Expr::bytes(vec![0x44u8; 32]),
    ]
}

fn contract_metrics(name: &str, path: &str, args: &[Expr<'static>]) -> (usize, usize, usize) {
    let source = load_contract_source(path);
    let compiled = compile_contract(&source, args, CompileOptions::default())
        .unwrap_or_else(|err| panic!("{name} contract should compile: {err}"));
    let script_len = compiled.script.len();
    let (instruction_count, charged_op_count) = script_op_counts(&compiled.script);
    (script_len, instruction_count, charged_op_count)
}

fn script_op_counts(script: &[u8]) -> (usize, usize) {
    let mut instruction_count = 0;
    let mut charged_op_count = 0;

    for opcode in parse_script::<PopulatedTransaction<'static>, SigHashReusedValuesUnsync>(script) {
        let opcode = opcode.expect("compiled script should parse");
        instruction_count += 1;
        if !opcode.is_push_opcode() {
            charged_op_count += 1;
        }
    }

    (instruction_count, charged_op_count)
}

fn blake2b(data: Vec<u8>) -> Hash {
    use blake2b_simd::Params as Blake2bParams;
    Hash::from_slice(Blake2bParams::new().hash_length(32).to_state().update(&data).finalize().as_bytes())
}

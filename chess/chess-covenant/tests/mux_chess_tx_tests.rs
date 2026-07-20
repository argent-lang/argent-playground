#![allow(clippy::too_many_arguments)]

use blake2b_simd::Params as Blake2bParams;
use kaspa_consensus_core::hashing::sighash::calc_schnorr_signature_hash;
use kaspa_consensus_core::hashing::sighash::SigHashReusedValuesUnsync;
use kaspa_consensus_core::hashing::sighash_type::SIG_HASH_ALL;
use kaspa_consensus_core::mass::units::SigopCount;
use kaspa_consensus_core::tx::{
    CovenantBinding, PopulatedTransaction, Transaction, TransactionId, TransactionInput, TransactionOutpoint, TransactionOutput,
    UtxoEntry, VerifiableTransaction,
};
use kaspa_consensus_core::Hash;
use kaspa_txscript::caches::Cache;
use kaspa_txscript::covenants::CovenantsContext;
use kaspa_txscript::{
    pay_to_script_hash_script, pay_to_script_hash_signature_script_with_flags, EngineCtx, EngineFlags, TxScriptEngine,
};
use kaspa_txscript_errors::TxScriptError;
use secp256k1::{Keypair, Message, Secp256k1, SecretKey};
use silverscript_lang::ast::Expr;
use silverscript_lang::compiler::{compile_contract, CompileOptions, CompiledContract};

use chess_covenant::{
    castle_challenge_contract_path, castle_contract_path, diag_contract_path, horiz_contract_path, king_contract_path,
    knight_contract_path, league_contract_path, load_contract_source as load_preprocessed_contract_source, mux_contract_path,
    pawn_contract_path, player_contract_path, settle_contract_path, vert_contract_path,
};

struct Player {
    keypair: Keypair,
    pubkey_bytes: Vec<u8>,
    owner_hash: Hash,
    player_id: Hash,
    player_ref: Hash,
}

struct TemplateFixture {
    source: &'static str,
    prefix: Vec<u8>,
    suffix: Vec<u8>,
    hash: Hash,
}

struct MuxChessFixture {
    mux: TemplateFixture,
    settle: TemplateFixture,
    pawn: TemplateFixture,
    knight: TemplateFixture,
    vert: TemplateFixture,
    horiz: TemplateFixture,
    diag: TemplateFixture,
    king: TemplateFixture,
    castle: TemplateFixture,
    castle_challenge: TemplateFixture,
}

struct GameStateArgs<'a> {
    board: &'a [u8],
    turn: i64,
    status: i64,
    castle_rights: [u8; 4],
    en_passant_idx: i64,
    pending_src_idx: i64,
    pending_dst_idx: i64,
    pending_promo: i64,
    recent_castle: i64,
    draw_state: i64,
}

struct PlayerStateArgs<'a> {
    league_template: &'a Hash,
    player_template: &'a Hash,
    mux_template: &'a Hash,
    routes_commitment: &'a Hash,
    owner_hash: &'a Hash,
    player_id: &'a Hash,
    open_games: i64,
    rating: i64,
    games: i64,
    wins: i64,
    draws: i64,
    losses: i64,
}

struct MoveArgs {
    from_x: i64,
    from_y: i64,
    to_x: i64,
    to_y: i64,
    promo_piece: i64,
}

const ELO_K: i64 = 32;
const ELO_SCALE: i64 = 1000;
const DEFAULT_MOVE_TIMEOUT: i64 = 600;

fn hash_expr(value: Hash) -> Expr<'static> {
    Expr::bytes(hash_bytes(value))
}

fn repeated_hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; 32])
}

fn player_ref(owner_hash: Hash, player_id: Hash) -> Hash {
    hash_pair(owner_hash, player_id)
}

fn blake2b_bytes(data: &[u8]) -> Hash {
    Hash::from_slice(Blake2bParams::new().hash_length(32).to_state().update(data).finalize().as_bytes())
}

fn hash_bytes(value: Hash) -> Vec<u8> {
    value.as_bytes().to_vec()
}

fn hash_pair(left: Hash, right: Hash) -> Hash {
    let left = left.as_bytes();
    let right = right.as_bytes();
    blake2b_bytes(&[left.as_slice(), right.as_slice()].concat())
}

fn approx_expected_score(diff: i64) -> i64 {
    if diff < -800 {
        990
    } else if diff < -600 {
        970
    } else if diff < -400 {
        910
    } else if diff < -250 {
        820
    } else if diff < -150 {
        700
    } else if diff < -75 {
        600
    } else if diff < 75 {
        500
    } else if diff < 150 {
        400
    } else if diff < 250 {
        300
    } else if diff < 400 {
        180
    } else if diff < 600 {
        90
    } else if diff < 800 {
        30
    } else {
        10
    }
}

fn approx_updated_rating(self_rating: i64, opp_rating: i64, actual_score: i64) -> i64 {
    let expected = approx_expected_score(opp_rating - self_rating);
    let delta = (ELO_K * (actual_score - expected)) / ELO_SCALE;
    self_rating + delta
}

fn packed_route_templates(fix: &MuxChessFixture) -> Vec<u8> {
    let player_template = player_template_hash(fix);
    let mut out = Vec::with_capacity(32 * 9);
    out.extend_from_slice(&fix.pawn.hash.as_bytes());
    out.extend_from_slice(&fix.knight.hash.as_bytes());
    out.extend_from_slice(&fix.vert.hash.as_bytes());
    out.extend_from_slice(&fix.horiz.hash.as_bytes());
    out.extend_from_slice(&fix.diag.hash.as_bytes());
    out.extend_from_slice(&fix.king.hash.as_bytes());
    out.extend_from_slice(&fix.castle.hash.as_bytes());
    out.extend_from_slice(&fix.castle_challenge.hash.as_bytes());
    let settle_commitment = Blake2bParams::new()
        .hash_length(32)
        .to_state()
        .update(&fix.settle.hash.as_bytes())
        .update(&player_template.as_bytes())
        .finalize()
        .as_bytes()
        .to_vec();
    out.extend_from_slice(&settle_commitment);
    out
}

fn routes_commitment(route_templates: &[u8]) -> Hash {
    Hash::from_slice(Blake2bParams::new().hash_length(32).to_state().update(route_templates).finalize().as_bytes())
}

fn player_from_seed(seed: u8) -> Player {
    let secp = Secp256k1::new();
    let secret = SecretKey::from_slice(&[seed; 32]).expect("valid deterministic secret key");
    let keypair = Keypair::from_secret_key(&secp, &secret);
    let (x_only, _) = keypair.x_only_public_key();
    let pubkey_bytes = x_only.serialize().to_vec();
    let owner_hash = blake2b_bytes(&pubkey_bytes);
    let player_id = blake2b_bytes(&[b"test-player-id".as_slice(), pubkey_bytes.as_slice()].concat());
    let player_ref = player_ref(owner_hash, player_id);
    Player { keypair, pubkey_bytes, owner_hash, player_id, player_ref }
}

fn player_template_hash(fix: &MuxChessFixture) -> Hash {
    let compiled = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &repeated_hash(0x11),
            player_template: &repeated_hash(0x22),
            mux_template: &fix.mux.hash,
            routes_commitment: &repeated_hash(0x33),
            owner_hash: &repeated_hash(0x44),
            player_id: &repeated_hash(0x55),
            open_games: 0,
            rating: 1200,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );
    Hash::from_bytes(compiled.template_hash())
}

fn load_contract_source(path: &'static str) -> &'static str {
    let source = load_preprocessed_contract_source(path);
    Box::leak(source.into_boxed_str())
}

fn league_source() -> &'static str {
    load_contract_source(league_contract_path())
}

fn player_source() -> &'static str {
    load_contract_source(player_contract_path())
}

fn template_fixture(source: &'static str, ctor: &[Expr<'_>]) -> TemplateFixture {
    let compiled = compile_contract(source, ctor, CompileOptions::default()).expect("compile template source succeeds");
    let layout = compiled.state_layout;
    let prefix = compiled.script[..layout.start].to_vec();
    let suffix = compiled.script[layout.start + layout.len..].to_vec();
    let hash = Hash::from_bytes(compiled.template_hash());
    TemplateFixture { source, prefix, suffix, hash }
}

fn standard_board() -> Vec<u8> {
    vec![
        0x04, 0x02, 0x03, 0x05, 0x06, 0x03, 0x02, 0x04, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x0c, 0x0a, 0x0b, 0x0d, 0x0e, 0x0b, 0x0a,
        0x0c,
    ]
}

fn move_piece(board: &mut [u8], from_x: usize, from_y: usize, to_x: usize, to_y: usize) {
    let from_idx = from_y * 8 + from_x;
    let to_idx = to_y * 8 + to_x;
    let piece = board[from_idx];
    board[from_idx] = 0x00;
    board[to_idx] = piece;
}

fn move_piece_to(board: &mut [u8], from_x: usize, from_y: usize, to_x: usize, to_y: usize, piece: u8) {
    let from_idx = from_y * 8 + from_x;
    let to_idx = to_y * 8 + to_x;
    board[from_idx] = 0x00;
    board[to_idx] = piece;
}

fn apply_en_passant(
    board: &mut [u8],
    from_x: usize,
    from_y: usize,
    to_x: usize,
    to_y: usize,
    captured_x: usize,
    captured_y: usize,
    piece: u8,
) {
    board[from_y * 8 + from_x] = 0x00;
    board[captured_y * 8 + captured_x] = 0x00;
    board[to_y * 8 + to_x] = piece;
}

fn square_idx(x: i64, y: i64) -> i64 {
    y * 8 + x
}

fn full_castle_rights() -> [u8; 4] {
    [1, 1, 1, 1]
}

fn castle_rights_expr(rights: [u8; 4]) -> Expr<'static> {
    Expr::bytes(rights.to_vec())
}

fn mv(from_x: i64, from_y: i64, to_x: i64, to_y: i64) -> MoveArgs {
    MoveArgs { from_x, from_y, to_x, to_y, promo_piece: 0 }
}

fn mv_promo(from_x: i64, from_y: i64, to_x: i64, to_y: i64, promo_piece: i64) -> MoveArgs {
    MoveArgs { from_x, from_y, to_x, to_y, promo_piece }
}

fn build_fixture() -> MuxChessFixture {
    let mux_source = load_contract_source(mux_contract_path());
    let settle_source = load_contract_source(settle_contract_path());
    let pawn_source = load_contract_source(pawn_contract_path());
    let knight_source = load_contract_source(knight_contract_path());
    let vert_source = load_contract_source(vert_contract_path());
    let horiz_source = load_contract_source(horiz_contract_path());
    let diag_source = load_contract_source(diag_contract_path());
    let king_source = load_contract_source(king_contract_path());
    let castle_source = load_contract_source(castle_contract_path());
    let castle_challenge_source = load_contract_source(castle_challenge_contract_path());

    let dummy_board = standard_board();
    let game_ctor = vec![
        Expr::bytes(vec![0x11u8; 32]),
        Expr::bytes(vec![0x33u8; 32 * 9]),
        Expr::bytes(vec![0x21u8; 32]),
        Expr::bytes(vec![0x22u8; 32]),
        Expr::bytes(dummy_board.clone()),
        Expr::int(0),
        Expr::int(0),
        Expr::int(DEFAULT_MOVE_TIMEOUT),
        castle_rights_expr(full_castle_rights()),
        Expr::int(-1),
        Expr::int(-1),
        Expr::int(-1),
        Expr::int(0),
        Expr::int(0),
        Expr::int(3),
    ];
    let settle_ctor = vec![Expr::bytes(vec![0x44u8; 32]), Expr::bytes(vec![0x21u8; 32]), Expr::bytes(vec![0x22u8; 32]), Expr::int(0)];

    MuxChessFixture {
        mux: template_fixture(mux_source, &game_ctor),
        settle: template_fixture(settle_source, &settle_ctor),
        pawn: template_fixture(pawn_source, &game_ctor),
        knight: template_fixture(knight_source, &game_ctor),
        vert: template_fixture(vert_source, &game_ctor),
        horiz: template_fixture(horiz_source, &game_ctor),
        diag: template_fixture(diag_source, &game_ctor),
        king: template_fixture(king_source, &game_ctor),
        castle: template_fixture(castle_source, &game_ctor),
        castle_challenge: template_fixture(castle_challenge_source, &game_ctor),
    }
}

fn compile_state(
    source: &'static str,
    fix: &MuxChessFixture,
    white_hash: &Hash,
    black_hash: &Hash,
    state: GameStateArgs<'_>,
) -> CompiledContract<'static> {
    let ctor = vec![
        hash_expr(fix.mux.hash),
        Expr::bytes(packed_route_templates(fix)),
        hash_expr(*white_hash),
        hash_expr(*black_hash),
        Expr::bytes(state.board.to_vec()),
        Expr::int(state.turn),
        Expr::int(state.status),
        Expr::int(DEFAULT_MOVE_TIMEOUT),
        castle_rights_expr(state.castle_rights),
        Expr::int(state.en_passant_idx),
        Expr::int(state.pending_src_idx),
        Expr::int(state.pending_dst_idx),
        Expr::int(state.pending_promo),
        Expr::int(state.recent_castle),
        Expr::int(state.draw_state),
    ];
    compile_contract(source, &ctor, CompileOptions::default()).expect("compile mux chess state")
}

fn compile_settle_state(
    source: &'static str,
    player_template: &Hash,
    white_hash: &Hash,
    black_hash: &Hash,
    status: i64,
) -> CompiledContract<'static> {
    let ctor = vec![hash_expr(*player_template), hash_expr(*white_hash), hash_expr(*black_hash), Expr::int(status)];
    compile_contract(source, &ctor, CompileOptions::default()).expect("compile settle chess state")
}

fn compile_player_state(source: &'static str, state: PlayerStateArgs<'_>) -> CompiledContract<'static> {
    let ctor = vec![
        hash_expr(*state.league_template),
        hash_expr(*state.player_template),
        hash_expr(*state.mux_template),
        hash_expr(*state.routes_commitment),
        hash_expr(*state.owner_hash),
        hash_expr(*state.player_id),
        Expr::int(state.open_games),
        Expr::int(state.rating),
        Expr::int(state.games),
        Expr::int(state.wins),
        Expr::int(state.draws),
        Expr::int(state.losses),
    ];
    compile_contract(source, &ctor, CompileOptions::default()).expect("compile player state")
}

fn compile_league_state(
    source: &'static str,
    league_template: &Hash,
    player_template: &Hash,
    mux_template: &Hash,
    routes_commitment: &Hash,
    base_rating: i64,
    admin_hash: &Hash,
) -> CompiledContract<'static> {
    let ctor = vec![
        hash_expr(*league_template),
        hash_expr(*player_template),
        hash_expr(*mux_template),
        hash_expr(*routes_commitment),
        Expr::int(base_rating),
        hash_expr(*admin_hash),
    ];
    compile_contract(source, &ctor, CompileOptions::default()).expect("compile league state")
}

fn entry_sigscript(compiled: &CompiledContract<'_>, function: &str, args: Vec<Expr<'_>>) -> Vec<u8> {
    let sigscript = compiled.build_sig_script(function, args).expect("sigscript builds");
    pay_to_script_hash_signature_script_with_flags(
        compiled.script.clone(),
        sigscript,
        EngineFlags { covenants_enabled: true, ..Default::default() },
    )
    .expect("wrap p2sh sigscript")
}

fn tx_input(index: u32, signature_script: Vec<u8>, sig_op_count: u8) -> TransactionInput {
    TransactionInput {
        previous_outpoint: TransactionOutpoint { transaction_id: TransactionId::from_bytes([index as u8 + 1; 32]), index },
        signature_script,
        sequence: 0,
        compute_commit: SigopCount(sig_op_count).into(),
    }
}

fn covenant_output_with_value(
    compiled: &CompiledContract<'_>,
    authorizing_input: u16,
    covenant_id: Hash,
    value: u64,
) -> TransactionOutput {
    TransactionOutput {
        value,
        script_public_key: pay_to_script_hash_script(&compiled.script),
        covenant: Some(CovenantBinding { authorizing_input, covenant_id }),
    }
}

fn covenant_output(compiled: &CompiledContract<'_>, authorizing_input: u16, covenant_id: Hash) -> TransactionOutput {
    covenant_output_with_value(compiled, authorizing_input, covenant_id, 1_000)
}

fn covenant_utxo_with_value(compiled: &CompiledContract<'_>, covenant_id: Hash, value: u64) -> UtxoEntry {
    UtxoEntry::new(value, pay_to_script_hash_script(&compiled.script), 0, false, Some(covenant_id))
}

fn covenant_utxo(compiled: &CompiledContract<'_>, covenant_id: Hash) -> UtxoEntry {
    covenant_utxo_with_value(compiled, covenant_id, 1_000)
}

fn populate_single_output_genesis_covenant(compiled: &CompiledContract<'_>) -> Hash {
    let input = TransactionInput {
        previous_outpoint: TransactionOutpoint { transaction_id: TransactionId::from_bytes([0x77u8; 32]), index: 0 },
        signature_script: vec![],
        sequence: 0,
        compute_commit: SigopCount(0).into(),
    };
    let covenant_id = kaspa_consensus_core::hashing::covenant_id::covenant_id(
        input.previous_outpoint,
        std::iter::once((
            0u32,
            &TransactionOutput { value: 1_000, script_public_key: pay_to_script_hash_script(&compiled.script), covenant: None },
        )),
    );
    let output = TransactionOutput {
        value: 1_000,
        script_public_key: pay_to_script_hash_script(&compiled.script),
        covenant: Some(CovenantBinding { authorizing_input: 0, covenant_id }),
    };
    let tx = Transaction::new(1, vec![input], vec![output], 0, Default::default(), 0, vec![]);
    let populated = PopulatedTransaction::new(&tx, vec![UtxoEntry::new(1_000, Default::default(), 0, false, None)]);
    CovenantsContext::from_tx(&populated).expect("validate genesis covenant bindings");
    covenant_id
}

fn execute_input_with_covenants(tx: Transaction, entries: Vec<UtxoEntry>, input_idx: usize) -> Result<(), TxScriptError> {
    let reused_values = SigHashReusedValuesUnsync::new();
    let sig_cache = Cache::new(10_000);
    let input = tx.inputs[input_idx].clone();
    let populated = PopulatedTransaction::new(&tx, entries);
    let cov_ctx = CovenantsContext::from_tx(&populated).map_err(TxScriptError::from)?;
    let utxo = populated.utxo(input_idx).expect("selected input utxo");
    let mut vm = TxScriptEngine::from_transaction_input(
        &populated,
        &input,
        input_idx,
        utxo,
        EngineCtx::new(&sig_cache).with_reused(&reused_values).with_covenants_ctx(&cov_ctx),
        EngineFlags { covenants_enabled: true, ..Default::default() },
    );
    vm.execute()
}

fn sign_tx_input_schnorr(tx: &Transaction, entries: &[UtxoEntry], input_idx: usize, player: &Player) -> Vec<u8> {
    let reused_values = SigHashReusedValuesUnsync::new();
    let populated = PopulatedTransaction::new(tx, entries.to_vec());
    let sig_hash = calc_schnorr_signature_hash(&populated, input_idx, SIG_HASH_ALL, &reused_values);
    let msg = Message::from_digest_slice(sig_hash.as_bytes().as_slice()).expect("valid sighash message");
    let sig = player.keypair.sign_schnorr(msg);
    let mut signature = Vec::new();
    signature.extend_from_slice(sig.as_ref());
    signature.push(SIG_HASH_ALL.to_u8());
    signature
}

fn assert_terminal_mux_settlement(status: i64, expected_white: (i64, i64, i64, i64), expected_black: (i64, i64, i64, i64)) {
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);
    let base_rating = 1200;
    let league_template = repeated_hash(0x33);
    let covenant_id = Hash::from_bytes([0x72u8; 32]);

    let white = player_from_seed(0x21);
    let black = player_from_seed(0x22);

    let player_contract = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &repeated_hash(0x44),
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &repeated_hash(0x55),
            player_id: &repeated_hash(0x56),
            open_games: 0,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );
    let player_layout = player_contract.state_layout;
    let player_template = Hash::from_bytes(player_contract.template_hash());
    let white_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &white.owner_hash,
            player_id: &white.player_id,
            open_games: 1,
            rating: base_rating,
            games: 10,
            wins: 6,
            draws: 2,
            losses: 2,
        },
    );
    let black_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &black.owner_hash,
            player_id: &black.player_id,
            open_games: 1,
            rating: base_rating,
            games: 10,
            wins: 2,
            draws: 2,
            losses: 6,
        },
    );
    let white_rating = if status == 1 {
        approx_updated_rating(base_rating, base_rating, 1000)
    } else if status == 3 {
        approx_updated_rating(base_rating, base_rating, 500)
    } else {
        approx_updated_rating(base_rating, base_rating, 0)
    };
    let black_rating = if status == 2 {
        approx_updated_rating(base_rating, base_rating, 1000)
    } else if status == 3 {
        approx_updated_rating(base_rating, base_rating, 500)
    } else {
        approx_updated_rating(base_rating, base_rating, 0)
    };

    let terminal_mux = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &standard_board(),
            turn: 0,
            status,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );

    let settled_white = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &white.owner_hash,
            player_id: &white.player_id,
            open_games: 0,
            rating: white_rating,
            games: expected_white.0,
            wins: expected_white.1,
            draws: expected_white.2,
            losses: expected_white.3,
        },
    );
    let settled_black = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &black.owner_hash,
            player_id: &black.player_id,
            open_games: 0,
            rating: black_rating,
            games: expected_black.0,
            wins: expected_black.1,
            draws: expected_black.2,
            losses: expected_black.3,
        },
    );

    let routed_settle = compile_settle_state(fix.settle.source, &player_template, &white.player_ref, &black.player_ref, status);
    let white_output_value = if status == 1 {
        2_000
    } else if status == 3 {
        1_500
    } else {
        1_000
    };
    let black_output_value = if status == 2 {
        2_000
    } else if status == 3 {
        1_500
    } else {
        1_000
    };

    let mux_settle_sigscript = entry_sigscript(
        &terminal_mux,
        "settle",
        vec![hash_expr(player_template), Expr::bytes(fix.settle.prefix.clone()), Expr::bytes(fix.settle.suffix.clone())],
    );
    let mux_outputs = vec![covenant_output(&routed_settle, 0, covenant_id)];
    let mux_entries = vec![covenant_utxo(&terminal_mux, covenant_id)];
    let mux_tx = Transaction::new(1, vec![tx_input(0, mux_settle_sigscript, 0)], mux_outputs, 0, Default::default(), 0, vec![]);
    let mux_result = execute_input_with_covenants(mux_tx, mux_entries, 0);
    assert!(mux_result.is_ok(), "ChessMux.settle runtime failed: {}", mux_result.unwrap_err());

    let settle_prefix_len = fix.settle.prefix.len() as i64;
    let settle_suffix_len = fix.settle.suffix.len() as i64;
    let settle_sigscript = entry_sigscript(
        &routed_settle,
        "settle",
        vec![
            Expr::bytes(player_contract.script[..player_layout.start].to_vec()),
            Expr::bytes(player_contract.script[player_layout.start + player_layout.len..].to_vec()),
        ],
    );
    let white_delegate_sigscript = entry_sigscript(
        &white_player,
        "delegate_settle",
        vec![
            Expr::int(settle_prefix_len),
            Expr::int(settle_suffix_len),
            hash_expr(fix.settle.hash),
            Expr::bytes(route_templates.clone()),
        ],
    );
    let black_delegate_sigscript = entry_sigscript(
        &black_player,
        "delegate_settle",
        vec![
            Expr::int(settle_prefix_len),
            Expr::int(settle_suffix_len),
            hash_expr(fix.settle.hash),
            Expr::bytes(route_templates.clone()),
        ],
    );

    let outputs = vec![
        covenant_output_with_value(&settled_white, 0, covenant_id, white_output_value),
        covenant_output_with_value(&settled_black, 0, covenant_id, black_output_value),
    ];
    let entries = vec![
        covenant_utxo(&routed_settle, covenant_id),
        covenant_utxo(&white_player, covenant_id),
        covenant_utxo(&black_player, covenant_id),
    ];
    let tx = Transaction::new(
        1,
        vec![tx_input(0, settle_sigscript, 0), tx_input(1, white_delegate_sigscript, 0), tx_input(2, black_delegate_sigscript, 0)],
        outputs,
        0,
        Default::default(),
        0,
        vec![],
    );

    let leader_result = execute_input_with_covenants(tx.clone(), entries.clone(), 0);
    assert!(leader_result.is_ok(), "ChessSettle.settle runtime failed: {}", leader_result.unwrap_err());

    let white_delegate_result = execute_input_with_covenants(tx.clone(), entries.clone(), 1);
    assert!(white_delegate_result.is_ok(), "white Player.delegate_settle runtime failed: {}", white_delegate_result.unwrap_err());

    let black_delegate_result = execute_input_with_covenants(tx, entries, 2);
    assert!(black_delegate_result.is_ok(), "black Player.delegate_settle runtime failed: {}", black_delegate_result.unwrap_err());
}

fn build_draw_settlement_tx_with_values(
    stake_value: u64,
    white_output_value: u64,
    black_output_value: u64,
) -> (Transaction, Vec<UtxoEntry>) {
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);
    let base_rating = 1200;
    let league_template = repeated_hash(0x37);
    let covenant_id = Hash::from_bytes([0x77u8; 32]);

    let white = player_from_seed(0x45);
    let black = player_from_seed(0x46);

    let player_contract = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &repeated_hash(0x44),
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &repeated_hash(0x55),
            player_id: &repeated_hash(0x56),
            open_games: 0,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );
    let player_layout = player_contract.state_layout;
    let player_template = Hash::from_bytes(player_contract.template_hash());
    let white_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &white.owner_hash,
            player_id: &white.player_id,
            open_games: 1,
            rating: base_rating,
            games: 10,
            wins: 6,
            draws: 2,
            losses: 2,
        },
    );
    let black_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &black.owner_hash,
            player_id: &black.player_id,
            open_games: 1,
            rating: base_rating,
            games: 10,
            wins: 2,
            draws: 2,
            losses: 6,
        },
    );
    let white_rating = approx_updated_rating(base_rating, base_rating, 500);
    let black_rating = approx_updated_rating(base_rating, base_rating, 500);
    let routed_settle = compile_settle_state(fix.settle.source, &player_template, &white.player_ref, &black.player_ref, 3);
    let settled_white = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &white.owner_hash,
            player_id: &white.player_id,
            open_games: 0,
            rating: white_rating,
            games: 11,
            wins: 6,
            draws: 3,
            losses: 2,
        },
    );
    let settled_black = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &black.owner_hash,
            player_id: &black.player_id,
            open_games: 0,
            rating: black_rating,
            games: 11,
            wins: 2,
            draws: 3,
            losses: 6,
        },
    );

    let settle_sigscript = entry_sigscript(
        &routed_settle,
        "settle",
        vec![
            Expr::bytes(player_contract.script[..player_layout.start].to_vec()),
            Expr::bytes(player_contract.script[player_layout.start + player_layout.len..].to_vec()),
        ],
    );
    let white_delegate_sigscript = entry_sigscript(
        &white_player,
        "delegate_settle",
        vec![
            Expr::int(fix.settle.prefix.len() as i64),
            Expr::int(fix.settle.suffix.len() as i64),
            hash_expr(fix.settle.hash),
            Expr::bytes(route_templates.clone()),
        ],
    );
    let black_delegate_sigscript = entry_sigscript(
        &black_player,
        "delegate_settle",
        vec![
            Expr::int(fix.settle.prefix.len() as i64),
            Expr::int(fix.settle.suffix.len() as i64),
            hash_expr(fix.settle.hash),
            Expr::bytes(route_templates.clone()),
        ],
    );

    let tx = Transaction::new(
        1,
        vec![tx_input(0, settle_sigscript, 0), tx_input(1, white_delegate_sigscript, 0), tx_input(2, black_delegate_sigscript, 0)],
        vec![
            covenant_output_with_value(&settled_white, 0, covenant_id, white_output_value),
            covenant_output_with_value(&settled_black, 0, covenant_id, black_output_value),
        ],
        0,
        Default::default(),
        0,
        vec![],
    );
    let entries = vec![
        covenant_utxo_with_value(&routed_settle, covenant_id, stake_value),
        covenant_utxo(&white_player, covenant_id),
        covenant_utxo(&black_player, covenant_id),
    ];
    (tx, entries)
}

fn run_route(
    active: &CompiledContract<'_>,
    selector: i64,
    mv: MoveArgs,
    player: &Player,
    target: &TemplateFixture,
    out: &CompiledContract<'_>,
    covenant_id: Hash,
) {
    run_route_with_action(active, selector, mv, 0, player, target, out, covenant_id);
}

fn run_route_with_action(
    active: &CompiledContract<'_>,
    selector: i64,
    mv: MoveArgs,
    termination_action: i64,
    player: &Player,
    target: &TemplateFixture,
    out: &CompiledContract<'_>,
    covenant_id: Hash,
) {
    run_route_with_promo_and_action(active, selector, mv, termination_action, player, target, out, covenant_id);
}

fn run_route_with_promo(
    active: &CompiledContract<'_>,
    selector: i64,
    mv: MoveArgs,
    player: &Player,
    target: &TemplateFixture,
    out: &CompiledContract<'_>,
    covenant_id: Hash,
) {
    run_route_with_promo_and_action(active, selector, mv, 0, player, target, out, covenant_id);
}

fn run_route_with_promo_and_action(
    active: &CompiledContract<'_>,
    selector: i64,
    mv: MoveArgs,
    termination_action: i64,
    player: &Player,
    target: &TemplateFixture,
    out: &CompiledContract<'_>,
    covenant_id: Hash,
) {
    let placeholder_sig = vec![0u8; 65];
    let placeholder_sigscript = entry_sigscript(
        active,
        "route",
        vec![
            selector.into(),
            mv.from_x.into(),
            mv.from_y.into(),
            mv.to_x.into(),
            mv.to_y.into(),
            mv.promo_piece.into(),
            termination_action.into(),
            Expr::bytes(placeholder_sig),
            Expr::bytes(player.pubkey_bytes.clone()),
            hash_expr(player.player_id),
            Expr::bytes(target.prefix.clone()),
            Expr::bytes(target.suffix.clone()),
        ],
    );
    let outputs = vec![covenant_output(out, 0, covenant_id)];
    let entries = vec![covenant_utxo(active, covenant_id)];
    let mut tx = Transaction::new(1, vec![tx_input(0, placeholder_sigscript, 1)], outputs, 0, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, player);
    tx.inputs[0].signature_script = entry_sigscript(
        active,
        "route",
        vec![
            selector.into(),
            mv.from_x.into(),
            mv.from_y.into(),
            mv.to_x.into(),
            mv.to_y.into(),
            mv.promo_piece.into(),
            termination_action.into(),
            Expr::bytes(sig),
            Expr::bytes(player.pubkey_bytes.clone()),
            hash_expr(player.player_id),
            Expr::bytes(target.prefix.clone()),
            Expr::bytes(target.suffix.clone()),
        ],
    );
    let result = execute_input_with_covenants(tx, entries, 0);
    assert!(result.is_ok(), "route should succeed: {:?}", result.unwrap_err());
}

fn run_route_err(
    active: &CompiledContract<'_>,
    selector: i64,
    mv: MoveArgs,
    player: &Player,
    target: &TemplateFixture,
    out: &CompiledContract<'_>,
    covenant_id: Hash,
) -> TxScriptError {
    let placeholder_sig = vec![0u8; 65];
    let placeholder_sigscript = entry_sigscript(
        active,
        "route",
        vec![
            selector.into(),
            mv.from_x.into(),
            mv.from_y.into(),
            mv.to_x.into(),
            mv.to_y.into(),
            mv.promo_piece.into(),
            0.into(),
            Expr::bytes(placeholder_sig),
            Expr::bytes(player.pubkey_bytes.clone()),
            hash_expr(player.player_id),
            Expr::bytes(target.prefix.clone()),
            Expr::bytes(target.suffix.clone()),
        ],
    );
    let outputs = vec![covenant_output(out, 0, covenant_id)];
    let entries = vec![covenant_utxo(active, covenant_id)];
    let mut tx = Transaction::new(1, vec![tx_input(0, placeholder_sigscript, 1)], outputs, 0, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, player);
    tx.inputs[0].signature_script = entry_sigscript(
        active,
        "route",
        vec![
            selector.into(),
            mv.from_x.into(),
            mv.from_y.into(),
            mv.to_x.into(),
            mv.to_y.into(),
            mv.promo_piece.into(),
            0.into(),
            Expr::bytes(sig),
            Expr::bytes(player.pubkey_bytes.clone()),
            hash_expr(player.player_id),
            Expr::bytes(target.prefix.clone()),
            Expr::bytes(target.suffix.clone()),
        ],
    );
    execute_input_with_covenants(tx, entries, 0).expect_err("route should fail")
}

fn run_route_err_with_output_value(
    active: &CompiledContract<'_>,
    selector: i64,
    mv: MoveArgs,
    player: &Player,
    target: &TemplateFixture,
    out: &CompiledContract<'_>,
    output_value: u64,
    covenant_id: Hash,
) -> TxScriptError {
    let placeholder_sig = vec![0u8; 65];
    let placeholder_sigscript = entry_sigscript(
        active,
        "route",
        vec![
            selector.into(),
            mv.from_x.into(),
            mv.from_y.into(),
            mv.to_x.into(),
            mv.to_y.into(),
            mv.promo_piece.into(),
            0.into(),
            Expr::bytes(placeholder_sig),
            Expr::bytes(player.pubkey_bytes.clone()),
            hash_expr(player.player_id),
            Expr::bytes(target.prefix.clone()),
            Expr::bytes(target.suffix.clone()),
        ],
    );
    let mut outputs = vec![covenant_output(out, 0, covenant_id)];
    outputs[0].value = output_value;
    let entries = vec![covenant_utxo(active, covenant_id)];
    let mut tx = Transaction::new(1, vec![tx_input(0, placeholder_sigscript, 1)], outputs, 0, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, player);
    tx.inputs[0].signature_script = entry_sigscript(
        active,
        "route",
        vec![
            selector.into(),
            mv.from_x.into(),
            mv.from_y.into(),
            mv.to_x.into(),
            mv.to_y.into(),
            mv.promo_piece.into(),
            0.into(),
            Expr::bytes(sig),
            Expr::bytes(player.pubkey_bytes.clone()),
            hash_expr(player.player_id),
            Expr::bytes(target.prefix.clone()),
            Expr::bytes(target.suffix.clone()),
        ],
    );
    execute_input_with_covenants(tx, entries, 0).expect_err("route should fail when output value changes")
}

fn run_worker_apply(
    label: &str,
    active: &CompiledContract<'_>,
    next: &CompiledContract<'_>,
    covenant_id: Hash,
    mux: &TemplateFixture,
) {
    let sigscript = entry_sigscript(active, "apply", vec![Expr::bytes(mux.prefix.clone()), Expr::bytes(mux.suffix.clone())]);
    let outputs = vec![covenant_output(next, 0, covenant_id)];
    let entries = vec![covenant_utxo(active, covenant_id)];
    let tx = Transaction::new(1, vec![tx_input(0, sigscript, 0)], outputs, 0, Default::default(), 0, vec![]);
    let result = execute_input_with_covenants(tx, entries, 0);
    assert!(result.is_ok(), "{label} worker apply should succeed: {:?}", result.unwrap_err());
}

fn run_worker_timeout(
    label: &str,
    active: &CompiledContract<'_>,
    next: &CompiledContract<'_>,
    covenant_id: Hash,
    settle: &TemplateFixture,
    player_template: &Hash,
    lock_time: u64,
    sequence: u64,
) {
    let sigscript = entry_sigscript(
        active,
        "timeout",
        vec![hash_expr(*player_template), Expr::bytes(settle.prefix.clone()), Expr::bytes(settle.suffix.clone())],
    );
    let outputs = vec![covenant_output(next, 0, covenant_id)];
    let entries = vec![covenant_utxo(active, covenant_id)];
    let input = TransactionInput {
        previous_outpoint: TransactionOutpoint { transaction_id: TransactionId::from_bytes([1u8; 32]), index: 0 },
        signature_script: sigscript,
        sequence,
        compute_commit: SigopCount(0).into(),
    };
    let tx = Transaction::new(1, vec![input], outputs, lock_time, Default::default(), 0, vec![]);
    let result = execute_input_with_covenants(tx, entries, 0);
    assert!(result.is_ok(), "{label} worker apply should succeed: {:?}", result.unwrap_err());
}

fn run_mux_timeout(
    label: &str,
    active: &CompiledContract<'_>,
    next: &CompiledContract<'_>,
    covenant_id: Hash,
    player: &Player,
    settle: &TemplateFixture,
    player_template: &Hash,
    lock_time: u64,
    sequence: u64,
) {
    let placeholder_sig = vec![0u8; 65];
    let placeholder_sigscript = entry_sigscript(
        active,
        "timeout",
        vec![
            Expr::bytes(placeholder_sig),
            Expr::bytes(player.pubkey_bytes.clone()),
            hash_expr(player.player_id),
            hash_expr(*player_template),
            Expr::bytes(settle.prefix.clone()),
            Expr::bytes(settle.suffix.clone()),
        ],
    );
    let outputs = vec![covenant_output(next, 0, covenant_id)];
    let entries = vec![covenant_utxo(active, covenant_id)];
    let input = TransactionInput {
        previous_outpoint: TransactionOutpoint { transaction_id: TransactionId::from_bytes([1u8; 32]), index: 0 },
        signature_script: placeholder_sigscript,
        sequence,
        compute_commit: SigopCount(1).into(),
    };
    let mut tx = Transaction::new(1, vec![input], outputs, lock_time, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, player);
    tx.inputs[0].signature_script = entry_sigscript(
        active,
        "timeout",
        vec![
            Expr::bytes(sig),
            Expr::bytes(player.pubkey_bytes.clone()),
            hash_expr(player.player_id),
            hash_expr(*player_template),
            Expr::bytes(settle.prefix.clone()),
            Expr::bytes(settle.suffix.clone()),
        ],
    );
    let result = execute_input_with_covenants(tx, entries, 0);
    assert!(result.is_ok(), "{label} mux timeout should succeed: {:?}", result.unwrap_err());
}

fn run_prep_apply(
    label: &str,
    active: &CompiledContract<'_>,
    next: &CompiledContract<'_>,
    covenant_id: Hash,
    target: &TemplateFixture,
) {
    let sigscript = entry_sigscript(active, "apply", vec![Expr::bytes(target.prefix.clone()), Expr::bytes(target.suffix.clone())]);
    let outputs = vec![covenant_output(next, 0, covenant_id)];
    let entries = vec![covenant_utxo(active, covenant_id)];
    let tx = Transaction::new(1, vec![tx_input(0, sigscript, 0)], outputs, 0, Default::default(), 0, vec![]);
    let result = execute_input_with_covenants(tx, entries, 0);
    assert!(result.is_ok(), "{label} prep apply should succeed: {:?}", result.unwrap_err());
}

fn run_worker_apply_err(
    label: &str,
    active: &CompiledContract<'_>,
    next: &CompiledContract<'_>,
    covenant_id: Hash,
    mux: &TemplateFixture,
) -> TxScriptError {
    let sigscript = entry_sigscript(active, "apply", vec![Expr::bytes(mux.prefix.clone()), Expr::bytes(mux.suffix.clone())]);
    let outputs = vec![covenant_output(next, 0, covenant_id)];
    let entries = vec![covenant_utxo(active, covenant_id)];
    let tx = Transaction::new(1, vec![tx_input(0, sigscript, 0)], outputs, 0, Default::default(), 0, vec![]);
    execute_input_with_covenants(tx, entries, 0).expect_err(label)
}

#[test]
fn muxed_chess_routes_all_move_families() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let board0 = standard_board();
    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 1),
            pending_dst_idx: square_idx(4, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 0, mv(4, 1, 4, 3), &white, &fix.pawn, &pawn0, covenant_id);
    let mut board1 = board0.clone();
    move_piece(&mut board1, 4, 1, 4, 3);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(4, 2),
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("pawn", &pawn0, &mux1, covenant_id, &fix.mux);

    let knight1 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(4, 2),
            pending_src_idx: square_idx(6, 7),
            pending_dst_idx: square_idx(5, 5),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux1, 1, mv(6, 7, 5, 5), &black, &fix.knight, &knight1, covenant_id);
    let mut board2 = board1.clone();
    move_piece(&mut board2, 6, 7, 5, 5);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board2,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("knight", &knight1, &mux2, covenant_id, &fix.mux);

    let mut board3 = vec![0u8; 64];
    board3[0] = 0x04;
    let mux3 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board3,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id3 = populate_single_output_genesis_covenant(&mux3);
    let vert = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board3,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 0),
            pending_dst_idx: square_idx(0, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux3, 2, mv(0, 0, 0, 3), &white, &fix.vert, &vert, covenant_id3);
    let mut board4 = board3.clone();
    move_piece(&mut board4, 0, 0, 0, 3);
    let mux4 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board4,
            turn: 1,
            status: 0,
            castle_rights: [1, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("vert", &vert, &mux4, covenant_id3, &fix.mux);

    let mut board4q = vec![0u8; 64];
    board4q[0] = 0x05;
    let mux4q = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board4q,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id4q = populate_single_output_genesis_covenant(&mux4q);
    let vert_queen = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board4q,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 0),
            pending_dst_idx: square_idx(0, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux4q, 2, mv(0, 0, 0, 3), &white, &fix.vert, &vert_queen, covenant_id4q);
    let mut board4q_next = board4q.clone();
    move_piece(&mut board4q_next, 0, 0, 0, 3);
    let mux4q_next = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board4q_next,
            turn: 1,
            status: 0,
            castle_rights: [1, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("vert_queen", &vert_queen, &mux4q_next, covenant_id4q, &fix.mux);

    let mut board5 = vec![0u8; 64];
    board5[56] = 0x0c;
    let mux5 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board5,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id5 = populate_single_output_genesis_covenant(&mux5);
    let vert_black = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board5,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 7),
            pending_dst_idx: square_idx(0, 4),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux5, 2, mv(0, 7, 0, 4), &black, &fix.vert, &vert_black, covenant_id5);
    let mut board6 = board5.clone();
    move_piece(&mut board6, 0, 7, 0, 4);
    let mux6 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board6,
            turn: 0,
            status: 0,
            castle_rights: [1, 1, 1, 0],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("vert_black", &vert_black, &mux6, covenant_id5, &fix.mux);

    let mut board7 = vec![0u8; 64];
    board7[31] = 0x05;
    let mux7 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board7,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id7 = populate_single_output_genesis_covenant(&mux7);
    let horiz_left = compile_state(
        fix.horiz.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board7,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(7, 3),
            pending_dst_idx: square_idx(4, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux7, 3, mv(7, 3, 4, 3), &white, &fix.horiz, &horiz_left, covenant_id7);
    let mut board8 = board7.clone();
    move_piece(&mut board8, 7, 3, 4, 3);
    let mux8 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board8,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("horiz_left", &horiz_left, &mux8, covenant_id7, &fix.mux);

    let mut board9 = vec![0u8; 64];
    board9[24] = 0x05;
    let mux9 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board9,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id9 = populate_single_output_genesis_covenant(&mux9);
    let horiz_right = compile_state(
        fix.horiz.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board9,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 3),
            pending_dst_idx: square_idx(3, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux9, 3, mv(0, 3, 3, 3), &white, &fix.horiz, &horiz_right, covenant_id9);
    let mut board10 = board9.clone();
    move_piece(&mut board10, 0, 3, 3, 3);
    let mux10 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board10,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("horiz_right", &horiz_right, &mux10, covenant_id9, &fix.mux);

    let mut board11 = vec![0u8; 64];
    board11[0] = 0x03;
    let mux11 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board11,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id11 = populate_single_output_genesis_covenant(&mux11);
    let diag_up_right = compile_state(
        fix.diag.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board11,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 0),
            pending_dst_idx: square_idx(3, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux11, 4, mv(0, 0, 3, 3), &white, &fix.diag, &diag_up_right, covenant_id11);
    let mut board12 = board11.clone();
    move_piece(&mut board12, 0, 0, 3, 3);
    let mux12 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board12,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("diag_up_right", &diag_up_right, &mux12, covenant_id11, &fix.mux);

    let mut board12q = vec![0u8; 64];
    board12q[0] = 0x05;
    let mux12q = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board12q,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id12q = populate_single_output_genesis_covenant(&mux12q);
    let diag_up_right_queen = compile_state(
        fix.diag.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board12q,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 0),
            pending_dst_idx: square_idx(3, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux12q, 4, mv(0, 0, 3, 3), &white, &fix.diag, &diag_up_right_queen, covenant_id12q);
    let mut board12q_next = board12q.clone();
    move_piece(&mut board12q_next, 0, 0, 3, 3);
    let mux12q_next = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board12q_next,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("diag_up_right_queen", &diag_up_right_queen, &mux12q_next, covenant_id12q, &fix.mux);

    let mut board13 = vec![0u8; 64];
    board13[7] = 0x03;
    let mux13 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board13,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id13 = populate_single_output_genesis_covenant(&mux13);
    let diag_up_left = compile_state(
        fix.diag.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board13,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(7, 0),
            pending_dst_idx: square_idx(4, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux13, 4, mv(7, 0, 4, 3), &white, &fix.diag, &diag_up_left, covenant_id13);
    let mut board14 = board13.clone();
    move_piece(&mut board14, 7, 0, 4, 3);
    let mux14 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board14,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("diag_up_left", &diag_up_left, &mux14, covenant_id13, &fix.mux);

    let mut board15 = vec![0u8; 64];
    board15[56] = 0x0b;
    let mux15 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board15,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id15 = populate_single_output_genesis_covenant(&mux15);
    let diag_down_right = compile_state(
        fix.diag.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board15,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 7),
            pending_dst_idx: square_idx(3, 4),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux15, 4, mv(0, 7, 3, 4), &black, &fix.diag, &diag_down_right, covenant_id15);
    let mut board16 = board15.clone();
    move_piece(&mut board16, 0, 7, 3, 4);
    let mux16 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board16,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("diag_down_right", &diag_down_right, &mux16, covenant_id15, &fix.mux);

    let mut board17 = vec![0u8; 64];
    board17[63] = 0x0b;
    let mux17 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board17,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id17 = populate_single_output_genesis_covenant(&mux17);
    let diag_down_left = compile_state(
        fix.diag.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board17,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(7, 7),
            pending_dst_idx: square_idx(4, 4),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux17, 4, mv(7, 7, 4, 4), &black, &fix.diag, &diag_down_left, covenant_id17);
    let mut board18 = board17.clone();
    move_piece(&mut board18, 7, 7, 4, 4);
    let mux18 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board18,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("diag_down_left", &diag_down_left, &mux18, covenant_id17, &fix.mux);

    let mut board19 = vec![0u8; 64];
    board19[4] = 0x06;
    let mux19 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board19,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id19 = populate_single_output_genesis_covenant(&mux19);
    let king = compile_state(
        fix.king.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board19,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 0),
            pending_dst_idx: square_idx(4, 1),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux19, 5, mv(4, 0, 4, 1), &white, &fix.king, &king, covenant_id19);
    let mut board20 = board19.clone();
    move_piece(&mut board20, 4, 0, 4, 1);
    let mux20 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board20,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("king", &king, &mux20, covenant_id19, &fix.mux);

    let mut board21 = vec![0u8; 64];
    board21[4] = 0x06;
    board21[7] = 0x04;
    let mux21 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board21,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id21 = populate_single_output_genesis_covenant(&mux21);
    let castle = compile_state(
        fix.castle.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board21,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 0),
            pending_dst_idx: square_idx(6, 0),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux21, 6, mv(4, 0, 6, 0), &white, &fix.castle, &castle, covenant_id21);
    let mut board22 = board21.clone();
    board22[4] = 0x00;
    board22[5] = 0x04;
    board22[6] = 0x06;
    board22[7] = 0x00;
    let mux22 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board22,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    run_worker_apply("castle", &castle, &mux22, covenant_id21, &fix.mux);
}

#[test]
fn capturing_enemy_king_sets_terminal_status() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[0] = 0x05;
    board0[24] = 0x0e;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let vert = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 0),
            pending_dst_idx: square_idx(0, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 2, mv(0, 0, 0, 3), &white, &fix.vert, &vert, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 0, 0, 0, 3);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 1,
            castle_rights: [1, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("vert_king_capture", &vert, &mux1, covenant_id, &fix.mux);
}

#[test]
fn ignoring_single_check_is_punishable_by_next_ply_king_capture() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 0) as usize] = 0x06;
    board0[square_idx(6, 0) as usize] = 0x02;
    board0[square_idx(4, 7) as usize] = 0x0c;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let knight0 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 0),
            pending_dst_idx: square_idx(5, 2),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 1, mv(6, 0, 5, 2), &white, &fix.knight, &knight0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 6, 0, 5, 2);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("ignore_check_knight", &knight0, &mux1, covenant_id, &fix.mux);

    let vert1 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 7),
            pending_dst_idx: square_idx(4, 0),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux1, 2, mv(4, 7, 4, 0), &black, &fix.vert, &vert1, covenant_id);

    let mut board2 = board1.clone();
    move_piece(&mut board2, 4, 7, 4, 0);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board2,
            turn: 0,
            status: 2,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("ignore_check_capture", &vert1, &mux2, covenant_id, &fix.mux);
}

#[test]
fn moving_a_pinned_piece_is_punishable_by_next_ply_king_capture() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 0) as usize] = 0x06;
    board0[square_idx(4, 1) as usize] = 0x04;
    board0[square_idx(4, 7) as usize] = 0x0c;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let horiz0 = compile_state(
        fix.horiz.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 1),
            pending_dst_idx: square_idx(5, 1),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 3, mv(4, 1, 5, 1), &white, &fix.horiz, &horiz0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 4, 1, 5, 1);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("pinned_rook_slide", &horiz0, &mux1, covenant_id, &fix.mux);

    let vert1 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 7),
            pending_dst_idx: square_idx(4, 0),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux1, 2, mv(4, 7, 4, 0), &black, &fix.vert, &vert1, covenant_id);

    let mut board2 = board1.clone();
    move_piece(&mut board2, 4, 7, 4, 0);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board2,
            turn: 0,
            status: 2,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("pinned_rook_capture", &vert1, &mux2, covenant_id, &fix.mux);
}

#[test]
fn king_move_into_attack_is_punishable_by_next_ply_king_capture() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 0) as usize] = 0x06;
    board0[square_idx(4, 7) as usize] = 0x0c;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let king0 = compile_state(
        fix.king.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 0),
            pending_dst_idx: square_idx(4, 1),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 5, mv(4, 0, 4, 1), &white, &fix.king, &king0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 4, 0, 4, 1);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("king_walk_into_attack", &king0, &mux1, covenant_id, &fix.mux);

    let vert1 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 7),
            pending_dst_idx: square_idx(4, 1),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux1, 2, mv(4, 7, 4, 1), &black, &fix.vert, &vert1, covenant_id);

    let mut board2 = board1.clone();
    move_piece(&mut board2, 4, 7, 4, 1);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board2,
            turn: 0,
            status: 2,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("king_walk_capture", &vert1, &mux2, covenant_id, &fix.mux);
}

#[test]
fn legal_interposition_blocks_the_immediate_king_capture_route() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 0) as usize] = 0x06;
    board0[square_idx(5, 0) as usize] = 0x03;
    board0[square_idx(4, 7) as usize] = 0x0c;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let diag0 = compile_state(
        fix.diag.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(5, 0),
            pending_dst_idx: square_idx(4, 1),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 4, mv(5, 0, 4, 1), &white, &fix.diag, &diag0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 5, 0, 4, 1);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("legal_interposition", &diag0, &mux1, covenant_id, &fix.mux);

    let vert1 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 7),
            pending_dst_idx: square_idx(4, 0),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux1, 2, mv(4, 7, 4, 0), &black, &fix.vert, &vert1, covenant_id);

    let mux_fail = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let _err = run_worker_apply_err("interposition_blocks_capture", &vert1, &mux_fail, covenant_id, &fix.mux);
}

#[test]
fn illegal_double_check_reply_is_punishable_by_next_ply_king_capture() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 0) as usize] = 0x06;
    board0[square_idx(5, 0) as usize] = 0x03;
    board0[square_idx(1, 3) as usize] = 0x0b;
    board0[square_idx(4, 7) as usize] = 0x0c;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let diag0 = compile_state(
        fix.diag.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(5, 0),
            pending_dst_idx: square_idx(4, 1),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 4, mv(5, 0, 4, 1), &white, &fix.diag, &diag0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 5, 0, 4, 1);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("double_check_illegal_reply", &diag0, &mux1, covenant_id, &fix.mux);

    let diag1 = compile_state(
        fix.diag.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(1, 3),
            pending_dst_idx: square_idx(4, 0),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux1, 4, mv(1, 3, 4, 0), &black, &fix.diag, &diag1, covenant_id);

    let mut board2 = board1.clone();
    move_piece(&mut board2, 1, 3, 4, 0);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board2,
            turn: 0,
            status: 2,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("double_check_capture", &diag1, &mux2, covenant_id, &fix.mux);
}

#[test]
fn pawn_underpromotion_to_knight_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 6) as usize] = 0x01;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 6),
            pending_dst_idx: square_idx(4, 7),
            pending_promo: 2,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route_with_promo(&mux0, 0, mv_promo(4, 6, 4, 7, 2), &white, &fix.pawn, &pawn0, covenant_id);

    let mut board1 = board0.clone();
    move_piece_to(&mut board1, 4, 6, 4, 7, 0x02);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("pawn_underpromotion", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn pawn_promotion_requires_choice() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 6) as usize] = 0x01;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 6),
            pending_dst_idx: square_idx(4, 7),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 0, mv(4, 6, 4, 7), &white, &fix.pawn, &pawn0, covenant_id);

    let mut board1 = board0.clone();
    move_piece_to(&mut board1, 4, 6, 4, 7, 0x01);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let _err = run_worker_apply_err("missing promotion choice should fail", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn non_promotion_pawn_move_rejects_promotion_choice() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 1) as usize] = 0x01;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 1),
            pending_dst_idx: square_idx(4, 2),
            pending_promo: 5,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route_with_promo(&mux0, 0, mv_promo(4, 1, 4, 2, 5), &white, &fix.pawn, &pawn0, covenant_id);

    let mut board1 = board0.clone();
    move_piece_to(&mut board1, 4, 1, 4, 2, 0x01);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let _err = run_worker_apply_err("ordinary pawn move with promotion choice should fail", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn white_en_passant_capture_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 4) as usize] = 0x01;
    board0[square_idx(3, 4) as usize] = 0x09;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(3, 5),
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(3, 5),
            pending_src_idx: square_idx(4, 4),
            pending_dst_idx: square_idx(3, 5),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 0, mv(4, 4, 3, 5), &white, &fix.pawn, &pawn0, covenant_id);

    let mut board1 = board0.clone();
    apply_en_passant(&mut board1, 4, 4, 3, 5, 3, 4, 0x01);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("white_en_passant", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn black_en_passant_capture_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(3, 3) as usize] = 0x09;
    board0[square_idx(4, 3) as usize] = 0x01;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(4, 2),
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(4, 2),
            pending_src_idx: square_idx(3, 3),
            pending_dst_idx: square_idx(4, 2),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 0, mv(3, 3, 4, 2), &black, &fix.pawn, &pawn0, covenant_id);

    let mut board1 = board0.clone();
    apply_en_passant(&mut board1, 3, 3, 4, 2, 4, 3, 0x09);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("black_en_passant", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn non_pawn_move_clears_en_passant_state() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(1, 0) as usize] = 0x02;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(3, 5),
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let knight0 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(3, 5),
            pending_src_idx: square_idx(1, 0),
            pending_dst_idx: square_idx(2, 2),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 1, mv(1, 0, 2, 2), &white, &fix.knight, &knight0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 1, 0, 2, 2);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("knight_clears_en_passant", &knight0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn pawn_diagonal_capture_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 4) as usize] = 0x01;
    board0[square_idx(5, 5) as usize] = 0x0a;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 4),
            pending_dst_idx: square_idx(5, 5),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 0, mv(4, 4, 5, 5), &white, &fix.pawn, &pawn0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 4, 4, 5, 5);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("pawn_diagonal_capture", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn pawn_double_step_blocked_by_occupied_middle_square_fails() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 1) as usize] = 0x01;
    board0[square_idx(4, 2) as usize] = 0x09;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 1),
            pending_dst_idx: square_idx(4, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 0, mv(4, 1, 4, 3), &white, &fix.pawn, &pawn0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 4, 1, 4, 3);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(4, 2),
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let _err = run_worker_apply_err("blocked double-step should fail", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn pawn_diagonal_move_into_empty_square_fails_without_en_passant() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 4) as usize] = 0x01;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 4),
            pending_dst_idx: square_idx(5, 5),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 0, mv(4, 4, 5, 5), &white, &fix.pawn, &pawn0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 4, 4, 5, 5);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let _err = run_worker_apply_err("diagonal move into empty square should fail", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn expired_en_passant_attempt_fails() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 4) as usize] = 0x01;
    board0[square_idx(3, 4) as usize] = 0x09;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 4),
            pending_dst_idx: square_idx(3, 5),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 0, mv(4, 4, 3, 5), &white, &fix.pawn, &pawn0, covenant_id);

    let mut board1 = board0.clone();
    apply_en_passant(&mut board1, 4, 4, 3, 5, 3, 4, 0x01);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let _err = run_worker_apply_err("expired en-passant should fail", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn all_castle_shapes_rewrite_expected_board() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    struct CastleCase {
        label: &'static str,
        board0: Vec<u8>,
        turn: i64,
        to_x: i64,
        to_y: i64,
        expected_board: Vec<u8>,
        expected_turn: i64,
        expected_castle_rights: [u8; 4],
        expected_recent_castle: i64,
    }

    let mut cases = Vec::new();

    let mut white_kingside_board = vec![0u8; 64];
    white_kingside_board[4] = 0x06;
    white_kingside_board[7] = 0x04;
    let mut white_kingside_expected = white_kingside_board.clone();
    white_kingside_expected[4] = 0x00;
    white_kingside_expected[5] = 0x04;
    white_kingside_expected[6] = 0x06;
    white_kingside_expected[7] = 0x00;
    cases.push(CastleCase {
        label: "white_kingside_castle",
        board0: white_kingside_board,
        turn: 0,
        to_x: 6,
        to_y: 0,
        expected_board: white_kingside_expected,
        expected_turn: 1,
        expected_castle_rights: [0, 0, 1, 1],
        expected_recent_castle: 1,
    });

    let mut white_queenside_board = vec![0u8; 64];
    white_queenside_board[0] = 0x04;
    white_queenside_board[4] = 0x06;
    let mut white_queenside_expected = white_queenside_board.clone();
    white_queenside_expected[0] = 0x00;
    white_queenside_expected[1] = 0x00;
    white_queenside_expected[2] = 0x06;
    white_queenside_expected[3] = 0x04;
    white_queenside_expected[4] = 0x00;
    cases.push(CastleCase {
        label: "white_queenside_castle",
        board0: white_queenside_board,
        turn: 0,
        to_x: 2,
        to_y: 0,
        expected_board: white_queenside_expected,
        expected_turn: 1,
        expected_castle_rights: [0, 0, 1, 1],
        expected_recent_castle: 2,
    });

    let mut black_kingside_board = vec![0u8; 64];
    black_kingside_board[60] = 0x0e;
    black_kingside_board[63] = 0x0c;
    let mut black_kingside_expected = black_kingside_board.clone();
    black_kingside_expected[60] = 0x00;
    black_kingside_expected[61] = 0x0c;
    black_kingside_expected[62] = 0x0e;
    black_kingside_expected[63] = 0x00;
    cases.push(CastleCase {
        label: "black_kingside_castle",
        board0: black_kingside_board,
        turn: 1,
        to_x: 6,
        to_y: 7,
        expected_board: black_kingside_expected,
        expected_turn: 0,
        expected_castle_rights: [1, 1, 0, 0],
        expected_recent_castle: 3,
    });

    let mut black_queenside_board = vec![0u8; 64];
    black_queenside_board[56] = 0x0c;
    black_queenside_board[60] = 0x0e;
    let mut black_queenside_expected = black_queenside_board.clone();
    black_queenside_expected[56] = 0x00;
    black_queenside_expected[57] = 0x00;
    black_queenside_expected[58] = 0x0e;
    black_queenside_expected[59] = 0x0c;
    black_queenside_expected[60] = 0x00;
    cases.push(CastleCase {
        label: "black_queenside_castle",
        board0: black_queenside_board,
        turn: 1,
        to_x: 2,
        to_y: 7,
        expected_board: black_queenside_expected,
        expected_turn: 0,
        expected_castle_rights: [1, 1, 0, 0],
        expected_recent_castle: 4,
    });

    for case in cases {
        let mover = if case.turn == 0 { &white } else { &black };
        let from_y = if case.turn == 0 { 0 } else { 7 };

        let mux0 = compile_state(
            fix.mux.source,
            &fix,
            &white.player_ref,
            &black.player_ref,
            GameStateArgs {
                board: &case.board0,
                turn: case.turn,
                status: 0,
                castle_rights: full_castle_rights(),
                en_passant_idx: -1,
                pending_src_idx: -1,
                pending_dst_idx: -1,
                pending_promo: 0,
                recent_castle: 0,
                draw_state: 3,
            },
        );
        let covenant_id = populate_single_output_genesis_covenant(&mux0);
        let castle = compile_state(
            fix.castle.source,
            &fix,
            &white.player_ref,
            &black.player_ref,
            GameStateArgs {
                board: &case.board0,
                turn: case.turn,
                status: 0,
                castle_rights: full_castle_rights(),
                en_passant_idx: -1,
                pending_src_idx: square_idx(4, from_y),
                pending_dst_idx: square_idx(case.to_x, case.to_y),
                pending_promo: 0,
                recent_castle: 0,
                draw_state: 3,
            },
        );
        run_route(&mux0, 6, mv(4, from_y, case.to_x, case.to_y), mover, &fix.castle, &castle, covenant_id);

        let mux1 = compile_state(
            fix.mux.source,
            &fix,
            &white.player_ref,
            &black.player_ref,
            GameStateArgs {
                board: &case.expected_board,
                turn: case.expected_turn,
                status: 0,
                castle_rights: case.expected_castle_rights,
                en_passant_idx: -1,
                pending_src_idx: -1,
                pending_dst_idx: -1,
                pending_promo: 0,
                recent_castle: case.expected_recent_castle,
                draw_state: 3,
            },
        );
        run_worker_apply(case.label, &castle, &mux1, covenant_id, &fix.mux);
    }
}

#[test]
fn ordinary_reply_after_castle_clears_recent_castle() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[5] = 0x04;
    board0[6] = 0x06;
    board0[62] = 0x0a;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let knight0 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 7),
            pending_dst_idx: square_idx(5, 5),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 1, mv(6, 7, 5, 5), &black, &fix.knight, &knight0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 6, 7, 5, 5);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("ordinary_reply_clears_recent_castle", &knight0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn castle_start_square_challenge_by_pawn_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[5] = 0x04;
    board0[6] = 0x06;
    board0[11] = 0x09;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let prep0 = compile_state(
        fix.castle_challenge.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(3, 1),
            pending_dst_idx: square_idx(4, 0),
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    run_route(&mux0, 7, mv(3, 1, 4, 0), &black, &fix.castle_challenge, &prep0, covenant_id);

    let mut proof_board = vec![0u8; 64];
    proof_board[4] = 0x06;
    proof_board[7] = 0x04;
    proof_board[11] = 0x09;
    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &proof_board,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(3, 1),
            pending_dst_idx: square_idx(4, 0),
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    run_prep_apply("castle_start_square_prep", &prep0, &pawn0, covenant_id, &fix.pawn);

    let mut board1 = proof_board.clone();
    move_piece(&mut board1, 3, 1, 4, 0);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 2,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("castle_start_square_pawn_challenge", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn castle_transit_square_challenge_by_rook_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[5] = 0x04;
    board0[6] = 0x06;
    board0[61] = 0x0c;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let prep0 = compile_state(
        fix.castle_challenge.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(5, 7),
            pending_dst_idx: square_idx(5, 0),
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    run_route(&mux0, 7, mv(5, 7, 5, 0), &black, &fix.castle_challenge, &prep0, covenant_id);

    let mut proof_board = vec![0u8; 64];
    proof_board[5] = 0x06;
    proof_board[7] = 0x04;
    proof_board[61] = 0x0c;
    let vert0 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &proof_board,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(5, 7),
            pending_dst_idx: square_idx(5, 0),
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    run_prep_apply("castle_transit_square_prep", &prep0, &vert0, covenant_id, &fix.vert);

    let mut board1 = proof_board.clone();
    move_piece(&mut board1, 5, 7, 5, 0);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 2,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("castle_transit_square_rook_challenge", &vert0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn castle_destination_square_challenge_by_rook_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[5] = 0x04;
    board0[6] = 0x06;
    board0[62] = 0x0c;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let prep0 = compile_state(
        fix.castle_challenge.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 7),
            pending_dst_idx: square_idx(6, 0),
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    run_route(&mux0, 7, mv(6, 7, 6, 0), &black, &fix.castle_challenge, &prep0, covenant_id);

    let vert0 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 7),
            pending_dst_idx: square_idx(6, 0),
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    run_prep_apply("castle_destination_square_prep", &prep0, &vert0, covenant_id, &fix.vert);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 6, 7, 6, 0);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 2,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("castle_destination_square_rook_challenge", &vert0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn white_queenside_castle_destination_challenge_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[2] = 0x06;
    board0[3] = 0x04;
    board0[58] = 0x0c;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 2,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let prep0 = compile_state(
        fix.castle_challenge.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(2, 7),
            pending_dst_idx: square_idx(2, 0),
            pending_promo: 0,
            recent_castle: 2,
            draw_state: 3,
        },
    );
    run_route(&mux0, 7, mv(2, 7, 2, 0), &black, &fix.castle_challenge, &prep0, covenant_id);

    let vert0 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(2, 7),
            pending_dst_idx: square_idx(2, 0),
            pending_promo: 0,
            recent_castle: 2,
            draw_state: 3,
        },
    );
    run_prep_apply("white_queenside_destination_prep", &prep0, &vert0, covenant_id, &fix.vert);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 2, 7, 2, 0);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 2,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("white_queenside_destination_challenge", &vert0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn black_kingside_castle_start_challenge_by_pawn_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[61] = 0x0c;
    board0[62] = 0x0e;
    board0[51] = 0x01;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: [1, 1, 0, 0],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 3,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let prep0 = compile_state(
        fix.castle_challenge.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: [1, 1, 0, 0],
            en_passant_idx: -1,
            pending_src_idx: square_idx(3, 6),
            pending_dst_idx: square_idx(4, 7),
            pending_promo: 0,
            recent_castle: 3,
            draw_state: 3,
        },
    );
    run_route(&mux0, 7, mv(3, 6, 4, 7), &white, &fix.castle_challenge, &prep0, covenant_id);

    let mut proof_board = vec![0u8; 64];
    proof_board[60] = 0x0e;
    proof_board[63] = 0x0c;
    proof_board[51] = 0x01;
    let pawn0 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &proof_board,
            turn: 0,
            status: 0,
            castle_rights: [1, 1, 0, 0],
            en_passant_idx: -1,
            pending_src_idx: square_idx(3, 6),
            pending_dst_idx: square_idx(4, 7),
            pending_promo: 0,
            recent_castle: 3,
            draw_state: 3,
        },
    );
    run_prep_apply("black_kingside_start_prep", &prep0, &pawn0, covenant_id, &fix.pawn);

    let mut board1 = proof_board.clone();
    move_piece(&mut board1, 3, 6, 4, 7);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 1,
            castle_rights: [1, 1, 0, 0],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("black_kingside_start_challenge", &pawn0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn black_queenside_castle_transit_challenge_by_rook_succeeds() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[58] = 0x0e;
    board0[59] = 0x0c;
    board0[3] = 0x04;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: [1, 1, 0, 0],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 4,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let prep0 = compile_state(
        fix.castle_challenge.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: [1, 1, 0, 0],
            en_passant_idx: -1,
            pending_src_idx: square_idx(3, 0),
            pending_dst_idx: square_idx(3, 7),
            pending_promo: 0,
            recent_castle: 4,
            draw_state: 3,
        },
    );
    run_route(&mux0, 7, mv(3, 0, 3, 7), &white, &fix.castle_challenge, &prep0, covenant_id);

    let mut proof_board = vec![0u8; 64];
    proof_board[56] = 0x0c;
    proof_board[59] = 0x0e;
    proof_board[3] = 0x04;
    let vert0 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &proof_board,
            turn: 0,
            status: 0,
            castle_rights: [1, 1, 0, 0],
            en_passant_idx: -1,
            pending_src_idx: square_idx(3, 0),
            pending_dst_idx: square_idx(3, 7),
            pending_promo: 0,
            recent_castle: 4,
            draw_state: 3,
        },
    );
    run_prep_apply("black_queenside_transit_prep", &prep0, &vert0, covenant_id, &fix.vert);

    let mut board1 = proof_board.clone();
    move_piece(&mut board1, 3, 0, 3, 7);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 1,
            castle_rights: [1, 1, 0, 0],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("black_queenside_transit_challenge", &vert0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn invalid_castle_destination_challenge_loses_by_worker_timeout() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);
    let player_template = player_template_hash(&fix);

    let mut board0 = vec![0u8; 64];
    board0[5] = 0x04;
    board0[6] = 0x06;
    board0[38] = 0x09;
    board0[62] = 0x0c;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let prep0 = compile_state(
        fix.castle_challenge.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 7),
            pending_dst_idx: square_idx(6, 0),
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    run_route(&mux0, 7, mv(6, 7, 6, 0), &black, &fix.castle_challenge, &prep0, covenant_id);

    let vert0 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 7),
            pending_dst_idx: square_idx(6, 0),
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 3,
        },
    );
    run_prep_apply("invalid_castle_destination_challenge_prep", &prep0, &vert0, covenant_id, &fix.vert);

    let mut impossible_board = board0.clone();
    move_piece(&mut impossible_board, 6, 7, 6, 0);
    let impossible_mux = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &impossible_board,
            turn: 0,
            status: 2,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let _err =
        run_worker_apply_err("invalid_castle_destination_challenge_should_not_apply", &vert0, &impossible_mux, covenant_id, &fix.mux);

    let settle_terminal = compile_settle_state(fix.settle.source, &player_template, &white.player_ref, &black.player_ref, 1);
    run_worker_timeout(
        "invalid_castle_destination_challenge_timeout",
        &vert0,
        &settle_terminal,
        covenant_id,
        &fix.settle,
        &player_template,
        0,
        600,
    );
}

#[test]
fn claim_draw_flips_turn_and_enters_draw_state() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let board0 = standard_board();
    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route_with_action(&mux0, 8, mv(-1, -1, -1, -1), 2, &white, &fix.mux, &mux1, covenant_id);
}

#[test]
fn surrender_routes_back_to_mux_with_terminal_status() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let board0 = standard_board();
    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: square_idx(4, 2),
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 1,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 2,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route_with_action(&mux0, 8, mv(-1, -1, -1, -1), 3, &white, &fix.mux, &mux1, covenant_id);
}

#[test]
fn ordinary_move_can_offer_draw_and_return_to_mux() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let board0 = standard_board();
    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let knight0 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 0),
            pending_dst_idx: square_idx(5, 2),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 4,
        },
    );
    run_route_with_action(&mux0, 1, mv(6, 0, 5, 2), 1, &white, &fix.knight, &knight0, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 6, 0, 5, 2);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 4,
        },
    );
    run_worker_apply("offer_draw_with_move", &knight0, &mux1, covenant_id, &fix.mux);
}

#[test]
fn route_rejects_changing_the_locked_game_value() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let board0 = standard_board();
    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let knight0 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 0),
            pending_dst_idx: square_idx(5, 2),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );

    let err = run_route_err_with_output_value(&mux0, 1, mv(6, 0, 5, 2), &white, &fix.knight, &knight0, 999, covenant_id);
    assert!(
        err.to_string().contains("verification failed"),
        "route with changed output value should fail the covenant check, got: {err}"
    );
}

#[test]
fn pending_draw_offer_can_be_accepted_on_next_mux_turn() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = standard_board();
    move_piece(&mut board0, 6, 0, 5, 2);

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 4,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 3,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route_with_action(&mux0, 8, mv(-1, -1, -1, -1), 4, &black, &fix.mux, &mux1, covenant_id);
}

#[test]
fn ordinary_reply_rejects_pending_draw_offer_and_clears_draw_state() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = standard_board();
    move_piece(&mut board0, 6, 0, 5, 2);

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 4,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let knight1 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 7),
            pending_dst_idx: square_idx(5, 5),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_route(&mux0, 1, mv(6, 7, 5, 5), &black, &fix.knight, &knight1, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 6, 7, 5, 5);
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    run_worker_apply("reject_draw_by_moving", &knight1, &mux1, covenant_id, &fix.mux);
}

#[test]
fn knight_draw_negotiation_flips_side_control_and_false_claim_loses() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(1, 0) as usize] = 0x02;
    board0[square_idx(6, 7) as usize] = 0x0a;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route_with_action(&mux0, 8, mv(-1, -1, -1, -1), 2, &white, &fix.mux, &mux1, covenant_id);

    let knight1 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(1, 0),
            pending_dst_idx: square_idx(2, 2),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route(&mux1, 1, mv(1, 0, 2, 2), &black, &fix.knight, &knight1, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 1, 0, 2, 2);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_worker_apply("draw_knight_refute", &knight1, &mux2, covenant_id, &fix.mux);

    let knight2 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(6, 7),
            pending_dst_idx: square_idx(5, 5),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_route(&mux2, 1, mv(6, 7, 5, 5), &white, &fix.knight, &knight2, covenant_id);

    let mut board2 = board1.clone();
    move_piece(&mut board2, 6, 7, 5, 5);
    let mux3 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board2,
            turn: 1,
            status: 2,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_worker_apply("draw_knight_counter", &knight2, &mux3, covenant_id, &fix.mux);
}

#[test]
fn knight_draw_capture_awards_win_to_the_actor() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(1, 0) as usize] = 0x02;
    board0[square_idx(2, 2) as usize] = 0x0e;

    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route_with_action(&mux0, 8, mv(-1, -1, -1, -1), 2, &white, &fix.mux, &mux1, covenant_id);

    let knight1 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(1, 0),
            pending_dst_idx: square_idx(2, 2),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route(&mux1, 1, mv(1, 0, 2, 2), &black, &fix.knight, &knight1, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 1, 0, 2, 2);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 2,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_worker_apply("draw_knight_capture_black_actor_wins", &knight1, &mux2, covenant_id, &fix.mux);
}

#[test]
fn pawn_draw_capture_awards_win_to_the_actor() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board0 = vec![0u8; 64];
    board0[square_idx(3, 3) as usize] = 0x01;
    board0[square_idx(4, 4) as usize] = 0x0e;

    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux1);

    let pawn1 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(3, 3),
            pending_dst_idx: square_idx(4, 4),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route(&mux1, 0, mv(3, 3, 4, 4), &black, &fix.pawn, &pawn1, covenant_id);

    let mut board1 = board0.clone();
    move_piece(&mut board1, 3, 3, 4, 4);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 2,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_worker_apply("draw_pawn_capture_black_actor_wins", &pawn1, &mux2, covenant_id, &fix.mux);
}

#[test]
fn draw_mode_reuses_ordinary_workers() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    // pawn
    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 1) as usize] = 0x01;
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux1);
    let pawn1 = compile_state(
        fix.pawn.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 1),
            pending_dst_idx: square_idx(4, 2),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route(&mux1, 0, mv(4, 1, 4, 2), &black, &fix.pawn, &pawn1, covenant_id);
    let mut board1 = board0.clone();
    move_piece(&mut board1, 4, 1, 4, 2);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_worker_apply("draw_pawn_refute", &pawn1, &mux2, covenant_id, &fix.mux);

    // vert
    let mut board0 = vec![0u8; 64];
    board0[square_idx(0, 0) as usize] = 0x04;
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux1);
    let vert1 = compile_state(
        fix.vert.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 0),
            pending_dst_idx: square_idx(0, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route(&mux1, 2, mv(0, 0, 0, 3), &black, &fix.vert, &vert1, covenant_id);
    let mut board1 = board0.clone();
    move_piece(&mut board1, 0, 0, 0, 3);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: [1, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_worker_apply("draw_vert_refute", &vert1, &mux2, covenant_id, &fix.mux);

    // horiz
    let mut board0 = vec![0u8; 64];
    board0[square_idx(0, 0) as usize] = 0x04;
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux1);
    let horiz1 = compile_state(
        fix.horiz.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 0),
            pending_dst_idx: square_idx(3, 0),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route(&mux1, 3, mv(0, 0, 3, 0), &black, &fix.horiz, &horiz1, covenant_id);
    let mut board1 = board0.clone();
    move_piece(&mut board1, 0, 0, 3, 0);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: [1, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_worker_apply("draw_horiz_refute", &horiz1, &mux2, covenant_id, &fix.mux);

    // diag
    let mut board0 = vec![0u8; 64];
    board0[square_idx(2, 0) as usize] = 0x03;
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux1);
    let diag1 = compile_state(
        fix.diag.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(2, 0),
            pending_dst_idx: square_idx(5, 3),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route(&mux1, 4, mv(2, 0, 5, 3), &black, &fix.diag, &diag1, covenant_id);
    let mut board1 = board0.clone();
    move_piece(&mut board1, 2, 0, 5, 3);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_worker_apply("draw_diag_refute", &diag1, &mux2, covenant_id, &fix.mux);

    // king
    let mut board0 = vec![0u8; 64];
    board0[square_idx(4, 0) as usize] = 0x06;
    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux1);
    let king1 = compile_state(
        fix.king.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 0),
            pending_dst_idx: square_idx(4, 1),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    run_route(&mux1, 5, mv(4, 0, 4, 1), &black, &fix.king, &king1, covenant_id);
    let mut board1 = board0.clone();
    move_piece(&mut board1, 4, 0, 4, 1);
    let mux2 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board1,
            turn: 0,
            status: 0,
            castle_rights: [0, 0, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 2,
        },
    );
    run_worker_apply("draw_king_refute", &king1, &mux2, covenant_id, &fix.mux);
}

#[test]
fn draw_mode_disallows_castle_and_castle_challenge_routes() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);

    let mut board = vec![0u8; 64];
    board[square_idx(4, 0) as usize] = 0x06;
    board[square_idx(7, 0) as usize] = 0x04;

    let mux1 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux1);
    let castle1 = compile_state(
        fix.castle.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 0),
            pending_dst_idx: square_idx(6, 0),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 1,
        },
    );
    let _err = run_route_err(&mux1, 6, mv(4, 0, 6, 0), &black, &fix.castle, &castle1, covenant_id);

    let prep1 = compile_state(
        fix.castle_challenge.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board,
            turn: 1,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(4, 0),
            pending_dst_idx: square_idx(6, 0),
            pending_promo: 0,
            recent_castle: 1,
            draw_state: 1,
        },
    );
    let _err = run_route_err(&mux1, 7, mv(4, 0, 6, 0), &black, &fix.castle_challenge, &prep1, covenant_id);
}

#[test]
fn knight_worker_timeout_rescues_invalid_committed_state() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);
    let player_template = player_template_hash(&fix);

    let board0 = standard_board();
    let knight1 = compile_state(
        fix.knight.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: square_idx(0, 1),
            pending_dst_idx: square_idx(0, 2),
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&knight1);
    let settle_terminal = compile_settle_state(fix.settle.source, &player_template, &white.player_ref, &black.player_ref, 2);

    run_worker_timeout("knight_timeout", &knight1, &settle_terminal, covenant_id, &fix.settle, &player_template, 0, 600);
}

#[test]
fn mux_timeout_awards_win_to_the_waiting_opponent() {
    let fix = build_fixture();
    let white = player_from_seed(1);
    let black = player_from_seed(2);
    let player_template = player_template_hash(&fix);

    let board0 = standard_board();
    let mux0 = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &board0,
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );
    let covenant_id = populate_single_output_genesis_covenant(&mux0);

    let settle_terminal = compile_settle_state(fix.settle.source, &player_template, &white.player_ref, &black.player_ref, 2);

    run_mux_timeout("mux_timeout", &mux0, &settle_terminal, covenant_id, &black, &fix.settle, &player_template, 0, 600);
}

#[test]
fn league_registers_a_real_player_contract() {
    let owner = player_from_seed(7);
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);

    let league_template = repeated_hash(0x11);
    let admin = repeated_hash(0x33);
    let base_rating = 1200i64;
    let covenant_id = Hash::from_bytes([0x66u8; 32]);
    let player_id_domain = b"LeaguePlayerId".to_vec();

    let player_template = compile_contract(
        player_source(),
        &[
            hash_expr(league_template),
            hash_expr(repeated_hash(0x44)),
            hash_expr(fix.mux.hash),
            hash_expr(routes_commitment),
            hash_expr(repeated_hash(0x55)),
            hash_expr(repeated_hash(0x77)),
            Expr::int(0),
            Expr::int(900),
            Expr::int(1),
            Expr::int(2),
            Expr::int(3),
            Expr::int(4),
        ],
        CompileOptions::default(),
    )
    .expect("compile player template succeeds");
    let layout = player_template.state_layout;
    let player_prefix = player_template.script[..layout.start].to_vec();
    let player_suffix = player_template.script[layout.start + layout.len..].to_vec();
    let player_template = Hash::from_bytes(player_template.template_hash());

    let league = compile_contract(
        league_source(),
        &[
            hash_expr(league_template),
            hash_expr(player_template),
            hash_expr(fix.mux.hash),
            hash_expr(routes_commitment),
            Expr::int(base_rating),
            hash_expr(admin),
        ],
        CompileOptions::default(),
    )
    .expect("compile league succeeds");

    let league_input = TransactionInput {
        previous_outpoint: TransactionOutpoint { transaction_id: TransactionId::from_bytes([0xabu8; 32]), index: 7 },
        signature_script: vec![],
        sequence: 0,
        compute_commit: SigopCount(1).into(),
    };

    let player_id = blake2b_bytes(&[player_id_domain.as_slice(), &[0xabu8; 32], &7u32.to_le_bytes()].concat());

    let registered_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &owner.owner_hash,
            player_id: &player_id,
            open_games: 0,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );

    let placeholder_sigscript = entry_sigscript(
        &league,
        "register_player",
        vec![
            Expr::bytes(vec![0u8; 65]),
            Expr::bytes(owner.pubkey_bytes.clone()),
            Expr::bytes(player_prefix.clone()),
            Expr::bytes(player_suffix.clone()),
        ],
    );
    let outputs = vec![covenant_output(&league, 0, covenant_id), covenant_output(&registered_player, 0, covenant_id)];
    let entries = vec![covenant_utxo(&league, covenant_id)];
    let mut tx = Transaction::new(1, vec![league_input], outputs, 0, Default::default(), 0, vec![]);
    tx.inputs[0].signature_script = placeholder_sigscript;

    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &owner);
    tx.inputs[0].signature_script = entry_sigscript(
        &league,
        "register_player",
        vec![Expr::bytes(sig), Expr::bytes(owner.pubkey_bytes.clone()), Expr::bytes(player_prefix), Expr::bytes(player_suffix)],
    );

    let register_result = execute_input_with_covenants(tx, entries, 0);
    assert!(register_result.is_ok(), "league register_player runtime failed: {}", register_result.unwrap_err());
}

#[test]
fn league_register_rejects_mutated_lane_output() {
    let owner = player_from_seed(7);
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);

    let league_template = repeated_hash(0x11);
    let admin = repeated_hash(0x33);
    let base_rating = 1200i64;
    let covenant_id = Hash::from_bytes([0x66u8; 32]);
    let player_id_domain = b"LeaguePlayerId".to_vec();

    let player_template = compile_contract(
        player_source(),
        &[
            hash_expr(league_template),
            hash_expr(repeated_hash(0x44)),
            hash_expr(fix.mux.hash),
            hash_expr(routes_commitment),
            hash_expr(repeated_hash(0x55)),
            hash_expr(repeated_hash(0x77)),
            Expr::int(0),
            Expr::int(900),
            Expr::int(1),
            Expr::int(2),
            Expr::int(3),
            Expr::int(4),
        ],
        CompileOptions::default(),
    )
    .expect("compile player template succeeds");
    let layout = player_template.state_layout;
    let player_prefix = player_template.script[..layout.start].to_vec();
    let player_suffix = player_template.script[layout.start + layout.len..].to_vec();
    let player_template = Hash::from_bytes(player_template.template_hash());

    let league = compile_contract(
        league_source(),
        &[
            hash_expr(league_template),
            hash_expr(player_template),
            hash_expr(fix.mux.hash),
            hash_expr(routes_commitment),
            Expr::int(base_rating),
            hash_expr(admin),
        ],
        CompileOptions::default(),
    )
    .expect("compile league succeeds");

    let bad_league = compile_contract(
        league_source(),
        &[
            hash_expr(league_template),
            hash_expr(player_template),
            hash_expr(fix.mux.hash),
            hash_expr(routes_commitment),
            Expr::int(base_rating + 1),
            hash_expr(admin),
        ],
        CompileOptions::default(),
    )
    .expect("compile mutated league succeeds");

    let league_input = TransactionInput {
        previous_outpoint: TransactionOutpoint { transaction_id: TransactionId::from_bytes([0xabu8; 32]), index: 7 },
        signature_script: vec![],
        sequence: 0,
        compute_commit: SigopCount(1).into(),
    };

    let player_id = blake2b_bytes(&[player_id_domain.as_slice(), &[0xabu8; 32], &7u32.to_le_bytes()].concat());

    let registered_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &owner.owner_hash,
            player_id: &player_id,
            open_games: 0,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );

    let placeholder_sigscript = entry_sigscript(
        &league,
        "register_player",
        vec![
            Expr::bytes(vec![0u8; 65]),
            Expr::bytes(owner.pubkey_bytes.clone()),
            Expr::bytes(player_prefix.clone()),
            Expr::bytes(player_suffix.clone()),
        ],
    );
    let outputs = vec![covenant_output(&bad_league, 0, covenant_id), covenant_output(&registered_player, 0, covenant_id)];
    let entries = vec![covenant_utxo(&league, covenant_id)];
    let mut tx = Transaction::new(1, vec![league_input], outputs, 0, Default::default(), 0, vec![]);
    tx.inputs[0].signature_script = placeholder_sigscript;

    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &owner);
    tx.inputs[0].signature_script = entry_sigscript(
        &league,
        "register_player",
        vec![Expr::bytes(sig), Expr::bytes(owner.pubkey_bytes.clone()), Expr::bytes(player_prefix), Expr::bytes(player_suffix)],
    );

    let err = execute_input_with_covenants(tx, entries, 0).expect_err("league register should fail when lane output mutates");
    assert!(err.to_string().contains("verification failed"), "unexpected register failure: {err}");
}

#[test]
fn league_register_rejects_changed_lane_value() {
    let owner = player_from_seed(7);
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);

    let league_template = repeated_hash(0x11);
    let admin_hash = repeated_hash(0x33);
    let base_rating = 1200i64;
    let covenant_id = Hash::from_bytes([0x66u8; 32]);
    let player_id_domain = b"LeaguePlayerId".to_vec();

    let player_template = compile_contract(
        player_source(),
        &[
            hash_expr(league_template),
            hash_expr(repeated_hash(0x44)),
            hash_expr(fix.mux.hash),
            hash_expr(routes_commitment),
            hash_expr(repeated_hash(0x55)),
            hash_expr(repeated_hash(0x77)),
            Expr::int(0),
            Expr::int(900),
            Expr::int(1),
            Expr::int(2),
            Expr::int(3),
            Expr::int(4),
        ],
        CompileOptions::default(),
    )
    .expect("compile player template succeeds");
    let layout = player_template.state_layout;
    let player_prefix = player_template.script[..layout.start].to_vec();
    let player_suffix = player_template.script[layout.start + layout.len..].to_vec();
    let player_template = Hash::from_bytes(player_template.template_hash());

    let league = compile_league_state(
        league_source(),
        &league_template,
        &player_template,
        &fix.mux.hash,
        &routes_commitment,
        base_rating,
        &admin_hash,
    );

    let player_id = blake2b_bytes(&[player_id_domain.as_slice(), &[0xabu8; 32], &7u32.to_le_bytes()].concat());

    let registered_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &owner.owner_hash,
            player_id: &player_id,
            open_games: 0,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );

    let mut tx = Transaction::new(
        1,
        vec![tx_input(7, vec![], 1)],
        vec![covenant_output_with_value(&league, 0, covenant_id, 999), covenant_output(&registered_player, 0, covenant_id)],
        0,
        Default::default(),
        0,
        vec![],
    );
    let entries = vec![covenant_utxo(&league, covenant_id)];
    tx.inputs[0].signature_script = entry_sigscript(
        &league,
        "register_player",
        vec![
            Expr::bytes(vec![0u8; 65]),
            Expr::bytes(owner.pubkey_bytes.clone()),
            Expr::bytes(player_prefix.clone()),
            Expr::bytes(player_suffix.clone()),
        ],
    );
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &owner);
    tx.inputs[0].signature_script = entry_sigscript(
        &league,
        "register_player",
        vec![Expr::bytes(sig), Expr::bytes(owner.pubkey_bytes), Expr::bytes(player_prefix), Expr::bytes(player_suffix)],
    );

    let err = execute_input_with_covenants(tx, entries, 0).expect_err("league register should fail when lane value changes");
    assert!(err.to_string().contains("verification failed"), "unexpected register failure: {err}");
}

#[test]
fn league_rebalance_allows_same_spk_with_new_value() {
    let admin = player_from_seed(13);
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);

    let league_template = repeated_hash(0x21);
    let base_rating = 1200i64;
    let covenant_id = Hash::from_bytes([0x67u8; 32]);
    let league = compile_league_state(
        league_source(),
        &league_template,
        &repeated_hash(0x44),
        &fix.mux.hash,
        &routes_commitment,
        base_rating,
        &admin.owner_hash,
    );

    let mut tx = Transaction::new(
        1,
        vec![tx_input(8, vec![], 1)],
        vec![covenant_output_with_value(&league, 0, covenant_id, 777)],
        0,
        Default::default(),
        0,
        vec![],
    );
    let entries = vec![covenant_utxo(&league, covenant_id)];
    tx.inputs[0].signature_script =
        entry_sigscript(&league, "rebalance", vec![Expr::bytes(vec![0u8; 65]), Expr::bytes(admin.pubkey_bytes.clone())]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &admin);
    tx.inputs[0].signature_script = entry_sigscript(&league, "rebalance", vec![Expr::bytes(sig), Expr::bytes(admin.pubkey_bytes)]);

    let result = execute_input_with_covenants(tx, entries, 0);
    assert!(result.is_ok(), "league rebalance runtime failed: {}", result.unwrap_err());
}

#[test]
fn league_rebalance_rejects_changed_state_spk() {
    let admin = player_from_seed(13);
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);

    let league_template = repeated_hash(0x21);
    let base_rating = 1200i64;
    let covenant_id = Hash::from_bytes([0x67u8; 32]);
    let league = compile_league_state(
        league_source(),
        &league_template,
        &repeated_hash(0x44),
        &fix.mux.hash,
        &routes_commitment,
        base_rating,
        &admin.owner_hash,
    );
    let mutated = compile_league_state(
        league_source(),
        &league_template,
        &repeated_hash(0x44),
        &fix.mux.hash,
        &routes_commitment,
        base_rating + 1,
        &admin.owner_hash,
    );

    let mut tx = Transaction::new(
        1,
        vec![tx_input(8, vec![], 1)],
        vec![covenant_output_with_value(&mutated, 0, covenant_id, 777)],
        0,
        Default::default(),
        0,
        vec![],
    );
    let entries = vec![covenant_utxo(&league, covenant_id)];
    tx.inputs[0].signature_script =
        entry_sigscript(&league, "rebalance", vec![Expr::bytes(vec![0u8; 65]), Expr::bytes(admin.pubkey_bytes.clone())]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &admin);
    tx.inputs[0].signature_script = entry_sigscript(&league, "rebalance", vec![Expr::bytes(sig), Expr::bytes(admin.pubkey_bytes)]);

    let err = execute_input_with_covenants(tx, entries, 0).expect_err("league rebalance should fail when lane state changes");
    assert!(err.to_string().contains("verification failed"), "unexpected rebalance failure: {err}");
}

#[test]
fn league_fork_allows_two_identical_lanes() {
    let admin = player_from_seed(13);
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);

    let league_template = repeated_hash(0x21);
    let base_rating = 1200i64;
    let covenant_id = Hash::from_bytes([0x68u8; 32]);
    let league = compile_league_state(
        league_source(),
        &league_template,
        &repeated_hash(0x44),
        &fix.mux.hash,
        &routes_commitment,
        base_rating,
        &admin.owner_hash,
    );

    let mut tx = Transaction::new(
        1,
        vec![tx_input(9, vec![], 1)],
        vec![covenant_output_with_value(&league, 0, covenant_id, 400), covenant_output_with_value(&league, 0, covenant_id, 600)],
        0,
        Default::default(),
        0,
        vec![],
    );
    let entries = vec![covenant_utxo(&league, covenant_id)];
    tx.inputs[0].signature_script =
        entry_sigscript(&league, "fork", vec![Expr::bytes(vec![0u8; 65]), Expr::bytes(admin.pubkey_bytes.clone())]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &admin);
    tx.inputs[0].signature_script = entry_sigscript(&league, "fork", vec![Expr::bytes(sig), Expr::bytes(admin.pubkey_bytes)]);

    let result = execute_input_with_covenants(tx, entries, 0);
    assert!(result.is_ok(), "league fork runtime failed: {}", result.unwrap_err());
}

#[test]
fn league_fork_rejects_mutated_lane_output() {
    let admin = player_from_seed(13);
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);

    let league_template = repeated_hash(0x21);
    let base_rating = 1200i64;
    let covenant_id = Hash::from_bytes([0x68u8; 32]);
    let league = compile_league_state(
        league_source(),
        &league_template,
        &repeated_hash(0x44),
        &fix.mux.hash,
        &routes_commitment,
        base_rating,
        &admin.owner_hash,
    );
    let mutated = compile_league_state(
        league_source(),
        &league_template,
        &repeated_hash(0x44),
        &fix.mux.hash,
        &routes_commitment,
        base_rating + 1,
        &admin.owner_hash,
    );

    let mut tx = Transaction::new(
        1,
        vec![tx_input(9, vec![], 1)],
        vec![covenant_output_with_value(&league, 0, covenant_id, 400), covenant_output_with_value(&mutated, 0, covenant_id, 500)],
        0,
        Default::default(),
        0,
        vec![],
    );
    let entries = vec![covenant_utxo(&league, covenant_id)];
    tx.inputs[0].signature_script =
        entry_sigscript(&league, "fork", vec![Expr::bytes(vec![0u8; 65]), Expr::bytes(admin.pubkey_bytes.clone())]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &admin);
    tx.inputs[0].signature_script = entry_sigscript(&league, "fork", vec![Expr::bytes(sig), Expr::bytes(admin.pubkey_bytes)]);

    let err = execute_input_with_covenants(tx, entries, 0).expect_err("league fork should fail when a forked lane mutates");
    assert!(err.to_string().contains("verification failed"), "unexpected fork failure: {err}");
}

#[test]
fn players_can_start_a_real_mux_game() {
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);
    let white = player_from_seed(7);
    let black = player_from_seed(9);

    let league_template = repeated_hash(0x19);
    let base_rating = 1200i64;
    let covenant_id = Hash::from_bytes([0x71u8; 32]);

    let player_contract = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &repeated_hash(0x44),
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &repeated_hash(0x55),
            player_id: &repeated_hash(0x56),
            open_games: 0,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );
    let player_layout = player_contract.state_layout;
    let player_template = Hash::from_bytes(player_contract.template_hash());
    let player_prefix_len = player_layout.start as i64;
    let player_suffix_len = (player_contract.script.len() - (player_layout.start + player_layout.len)) as i64;

    let white_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &white.owner_hash,
            player_id: &white.player_id,
            open_games: 0,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );
    let black_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &black.owner_hash,
            player_id: &black.player_id,
            open_games: 0,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );
    let next_white_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &white.owner_hash,
            player_id: &white.player_id,
            open_games: 1,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );
    let next_black_player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &league_template,
            player_template: &player_template,
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &black.owner_hash,
            player_id: &black.player_id,
            open_games: 1,
            rating: base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    );
    let opening_mux = compile_state(
        fix.mux.source,
        &fix,
        &white.player_ref,
        &black.player_ref,
        GameStateArgs {
            board: &standard_board(),
            turn: 0,
            status: 0,
            castle_rights: full_castle_rights(),
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
        },
    );

    let white_placeholder = entry_sigscript(
        &white_player,
        "start_game",
        vec![
            Expr::bytes(vec![0u8; 65]),
            Expr::bytes(white.pubkey_bytes.clone()),
            Expr::int(0),
            Expr::int(player_prefix_len),
            Expr::int(player_suffix_len),
            Expr::bytes(route_templates.clone()),
            Expr::int(DEFAULT_MOVE_TIMEOUT),
            Expr::bytes(fix.mux.prefix.clone()),
            Expr::bytes(fix.mux.suffix.clone()),
        ],
    );
    let black_placeholder = entry_sigscript(
        &black_player,
        "delegate_start_game",
        vec![
            Expr::bytes(vec![0u8; 65]),
            Expr::bytes(black.pubkey_bytes.clone()),
            Expr::int(DEFAULT_MOVE_TIMEOUT),
            Expr::int(player_prefix_len),
            Expr::int(player_suffix_len),
        ],
    );

    let outputs = vec![
        covenant_output(&next_white_player, 0, covenant_id),
        covenant_output(&next_black_player, 0, covenant_id),
        covenant_output(&opening_mux, 0, covenant_id),
    ];
    let entries = vec![covenant_utxo(&white_player, covenant_id), covenant_utxo(&black_player, covenant_id)];
    let mut tx = Transaction::new(
        1,
        vec![tx_input(0, white_placeholder, 1), tx_input(1, black_placeholder, 1)],
        outputs,
        0,
        Default::default(),
        0,
        vec![],
    );

    let white_sig = sign_tx_input_schnorr(&tx, &entries, 0, &white);
    let black_sig = sign_tx_input_schnorr(&tx, &entries, 1, &black);

    tx.inputs[0].signature_script = entry_sigscript(
        &white_player,
        "start_game",
        vec![
            Expr::bytes(white_sig),
            Expr::bytes(white.pubkey_bytes.clone()),
            Expr::int(0),
            Expr::int(player_prefix_len),
            Expr::int(player_suffix_len),
            Expr::bytes(route_templates.clone()),
            Expr::int(DEFAULT_MOVE_TIMEOUT),
            Expr::bytes(fix.mux.prefix.clone()),
            Expr::bytes(fix.mux.suffix.clone()),
        ],
    );
    tx.inputs[1].signature_script = entry_sigscript(
        &black_player,
        "delegate_start_game",
        vec![
            Expr::bytes(black_sig),
            Expr::bytes(black.pubkey_bytes.clone()),
            Expr::int(DEFAULT_MOVE_TIMEOUT),
            Expr::int(player_prefix_len),
            Expr::int(player_suffix_len),
        ],
    );

    let leader_result = execute_input_with_covenants(tx.clone(), entries.clone(), 0);
    assert!(leader_result.is_ok(), "Player.start_game leader runtime failed: {}", leader_result.unwrap_err());

    let delegate_result = execute_input_with_covenants(tx, entries, 1);
    assert!(delegate_result.is_ok(), "Player.delegate_start_game runtime failed: {}", delegate_result.unwrap_err());
}

#[test]
fn terminal_mux_settles_white_win_back_into_players() {
    assert_terminal_mux_settlement(1, (11, 7, 2, 2), (11, 2, 2, 7));
}

#[test]
fn terminal_mux_settles_black_win_back_into_players() {
    assert_terminal_mux_settlement(2, (11, 6, 2, 3), (11, 3, 2, 6));
}

#[test]
fn terminal_mux_settles_draw_back_into_players() {
    assert_terminal_mux_settlement(3, (11, 6, 3, 2), (11, 2, 3, 6));
}

#[test]
fn draw_settlement_allows_black_to_take_the_odd_extra() {
    let (tx, entries) = build_draw_settlement_tx_with_values(1_001, 1_500, 1_501);
    let leader_result = execute_input_with_covenants(tx.clone(), entries.clone(), 0);
    assert!(leader_result.is_ok(), "odd-stake draw settlement should succeed: {}", leader_result.unwrap_err());

    let white_delegate_result = execute_input_with_covenants(tx.clone(), entries.clone(), 1);
    assert!(white_delegate_result.is_ok(), "white Player.delegate_settle runtime failed: {}", white_delegate_result.unwrap_err());

    let black_delegate_result = execute_input_with_covenants(tx, entries, 2);
    assert!(black_delegate_result.is_ok(), "black Player.delegate_settle runtime failed: {}", black_delegate_result.unwrap_err());
}

#[test]
fn draw_settlement_rejects_the_wrong_odd_split() {
    let (tx, entries) = build_draw_settlement_tx_with_values(1_001, 1_501, 1_500);
    let leader_result = execute_input_with_covenants(tx, entries, 0);
    assert!(leader_result.is_err(), "draw settlement should reject giving the odd extra to white");
}

#[test]
fn player_rebalance_allows_same_spk_with_new_value() {
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);
    let owner = player_from_seed(0x33);
    let covenant_id = Hash::from_bytes([0x79u8; 32]);

    let player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &repeated_hash(0x43),
            player_template: &player_template_hash(&fix),
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &owner.owner_hash,
            player_id: &repeated_hash(0x53),
            open_games: 1,
            rating: 1200,
            games: 12,
            wins: 6,
            draws: 3,
            losses: 3,
        },
    );

    let placeholder = entry_sigscript(&player, "rebalance", vec![Expr::bytes(vec![0u8; 65]), Expr::bytes(owner.pubkey_bytes.clone())]);
    let entries = vec![covenant_utxo(&player, covenant_id)];
    let outputs = vec![covenant_output_with_value(&player, 0, covenant_id, 1_500)];
    let mut tx = Transaction::new(1, vec![tx_input(0, placeholder, 1)], outputs, 0, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &owner);
    tx.inputs[0].signature_script =
        entry_sigscript(&player, "rebalance", vec![Expr::bytes(sig), Expr::bytes(owner.pubkey_bytes.clone())]);

    let rebalance_result = execute_input_with_covenants(tx, entries, 0);
    assert!(rebalance_result.is_ok(), "Player.rebalance runtime failed: {}", rebalance_result.unwrap_err());
}

#[test]
fn player_rebalance_rejects_changed_state_spk() {
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);
    let owner = player_from_seed(0x34);
    let covenant_id = Hash::from_bytes([0x7au8; 32]);

    let player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &repeated_hash(0x44),
            player_template: &player_template_hash(&fix),
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &owner.owner_hash,
            player_id: &repeated_hash(0x54),
            open_games: 1,
            rating: 1200,
            games: 12,
            wins: 6,
            draws: 3,
            losses: 3,
        },
    );
    let mutated = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &repeated_hash(0x44),
            player_template: &player_template_hash(&fix),
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &owner.owner_hash,
            player_id: &repeated_hash(0x54),
            open_games: 1,
            rating: 1201,
            games: 12,
            wins: 6,
            draws: 3,
            losses: 3,
        },
    );

    let placeholder = entry_sigscript(&player, "rebalance", vec![Expr::bytes(vec![0u8; 65]), Expr::bytes(owner.pubkey_bytes.clone())]);
    let entries = vec![covenant_utxo(&player, covenant_id)];
    let outputs = vec![covenant_output(&mutated, 0, covenant_id)];
    let mut tx = Transaction::new(1, vec![tx_input(0, placeholder, 1)], outputs, 0, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &owner);
    tx.inputs[0].signature_script =
        entry_sigscript(&player, "rebalance", vec![Expr::bytes(sig), Expr::bytes(owner.pubkey_bytes.clone())]);

    let rebalance_result = execute_input_with_covenants(tx, entries, 0);
    assert!(rebalance_result.is_err(), "Player.rebalance should reject a changed player state/SPK");
}

#[test]
fn player_can_retire_with_no_open_games() {
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);
    let owner = player_from_seed(0x31);
    let covenant_id = Hash::from_bytes([0x73u8; 32]);

    let player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &repeated_hash(0x41),
            player_template: &player_template_hash(&fix),
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &owner.owner_hash,
            player_id: &repeated_hash(0x51),
            open_games: 0,
            rating: 1200,
            games: 12,
            wins: 6,
            draws: 3,
            losses: 3,
        },
    );

    let placeholder = entry_sigscript(&player, "retire", vec![Expr::bytes(vec![0u8; 65]), Expr::bytes(owner.pubkey_bytes.clone())]);
    let entries = vec![covenant_utxo(&player, covenant_id)];
    let mut tx = Transaction::new(1, vec![tx_input(0, placeholder, 1)], vec![], 0, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &owner);
    tx.inputs[0].signature_script =
        entry_sigscript(&player, "retire", vec![Expr::bytes(sig), Expr::bytes(owner.pubkey_bytes.clone())]);

    let retire_result = execute_input_with_covenants(tx, entries, 0);
    assert!(retire_result.is_ok(), "Player.retire runtime failed: {}", retire_result.unwrap_err());
}

#[test]
fn player_cannot_retire_with_open_games() {
    let fix = build_fixture();
    let route_templates = packed_route_templates(&fix);
    let routes_commitment = routes_commitment(&route_templates);
    let owner = player_from_seed(0x32);
    let covenant_id = Hash::from_bytes([0x74u8; 32]);

    let player = compile_player_state(
        player_source(),
        PlayerStateArgs {
            league_template: &repeated_hash(0x42),
            player_template: &player_template_hash(&fix),
            mux_template: &fix.mux.hash,
            routes_commitment: &routes_commitment,
            owner_hash: &owner.owner_hash,
            player_id: &repeated_hash(0x52),
            open_games: 1,
            rating: 1200,
            games: 12,
            wins: 6,
            draws: 3,
            losses: 3,
        },
    );

    let placeholder = entry_sigscript(&player, "retire", vec![Expr::bytes(vec![0u8; 65]), Expr::bytes(owner.pubkey_bytes.clone())]);
    let entries = vec![covenant_utxo(&player, covenant_id)];
    let mut tx = Transaction::new(1, vec![tx_input(0, placeholder, 1)], vec![], 0, Default::default(), 0, vec![]);
    let sig = sign_tx_input_schnorr(&tx, &entries, 0, &owner);
    tx.inputs[0].signature_script =
        entry_sigscript(&player, "retire", vec![Expr::bytes(sig), Expr::bytes(owner.pubkey_bytes.clone())]);

    let retire_result = execute_input_with_covenants(tx, entries, 0);
    assert!(retire_result.is_err(), "Player.retire should reject when open_games > 0");
}

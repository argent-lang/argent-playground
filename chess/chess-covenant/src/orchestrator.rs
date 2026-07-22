use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use argent_runtime::{actor as actor_arg, args, state, Artifact, ArtifactValue, EntryCall, TxBuilder, TxContext};
use blake2b_simd::Params as Blake2bParams;
use kaspa_consensus_core::hashing::sighash::calc_schnorr_signature_hash;
use kaspa_consensus_core::hashing::sighash::SigHashReusedValuesUnsync;
use kaspa_consensus_core::hashing::sighash_type::SIG_HASH_ALL;
use kaspa_consensus_core::mass::units::SigopCount;
use kaspa_consensus_core::tx::{
    CovenantBinding, MutableTransaction, PopulatedTransaction, ScriptPublicKey, Transaction, TransactionId, TransactionInput,
    TransactionOutpoint, TransactionOutput, UtxoEntry, VerifiableTransaction,
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

use crate::protocol_move::{apply_protocol_move, apply_standard_chess_move, ProtocolMoveSpec, ProtocolState};
use crate::{
    castle_challenge_contract_path, castle_contract_path, diag_contract_path, horiz_contract_path, king_contract_path,
    knight_contract_path, league_contract_path, load_contract_source, mux_contract_path, pawn_contract_path, player_contract_path,
    settle_contract_path, vert_contract_path,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateWitness {
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
    pub hash: Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChessTemplateFamily {
    pub league: TemplateWitness,
    pub player: TemplateWitness,
    pub mux: TemplateWitness,
    pub settle: TemplateWitness,
    pub pawn: TemplateWitness,
    pub knight: TemplateWitness,
    pub vert: TemplateWitness,
    pub horiz: TemplateWitness,
    pub diag: TemplateWitness,
    pub king: TemplateWitness,
    pub castle: TemplateWitness,
    pub castle_challenge: TemplateWitness,
    pub route_templates: Vec<u8>,
    pub routes_commitment: Hash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerKind {
    Pawn,
    Knight,
    Vert,
    Horiz,
    Diag,
    King,
    Castle,
    CastleChallenge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameResult {
    WhiteWin,
    BlackWin,
    Draw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignerRequirement {
    None,
    Owner,
    SideToMove,
    WaitingOpponent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractRole {
    League,
    Player,
    Mux,
    Settle,
    Worker(WorkerKind),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedCall {
    pub role: ContractRole,
    pub function: &'static str,
    pub signer: SignerRequirement,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedOutput {
    pub role: ContractRole,
    pub count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxRecipe {
    pub name: &'static str,
    pub calls: Vec<PlannedCall>,
    pub outputs: Vec<PlannedOutput>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettlementRecipe {
    pub mux_step: TxRecipe,
    pub settle_step: TxRecipe,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChessTxPlanner {
    pub family: ChessTemplateFamily,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorError(pub String);

impl std::fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OrchestratorError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    White,
    Black,
}

const DEFAULT_MOVE_TIMEOUT: i64 = 600;
const WHITE: i64 = 0;
const BLACK: i64 = 1;
const LIVE: i64 = 0;
const WWIN: i64 = 1;
const BWIN: i64 = 2;
const DRAW: i64 = 3;
const CLEAR: i64 = 0;
const CLAIMED: i64 = 1;
const DEFENSE: i64 = 2;

fn hash_expr(value: Hash) -> Expr<'static> {
    Expr::bytes(hash_bytes(value))
}

fn player_ref_hash(owner_hash: Hash, player_id: Hash) -> Hash {
    hash_pair(owner_hash, player_id)
}

fn repeated_hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; 32])
}

fn hash_bytes(value: Hash) -> Vec<u8> {
    value.as_bytes().to_vec()
}

fn hash_pair(left: Hash, right: Hash) -> Hash {
    let left = left.as_bytes();
    let right = right.as_bytes();
    blake2b(&[left.as_slice(), right.as_slice()].concat())
}

impl Side {
    fn other(self) -> Self {
        match self {
            Self::White => Self::Black,
            Self::Black => Self::White,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerHandle {
    pub name: String,
    pub pubkey_bytes: Vec<u8>,
    pub owner_hash: Hash,
    pub player_id: Option<Hash>,
    pub player_ref: Option<Hash>,
}

impl PlayerHandle {
    pub fn new(name: impl Into<String>, seed: u8) -> Self {
        let name = name.into();
        let pubkey_bytes = vec![seed; 32];
        let owner_hash = blake2b([name.as_bytes(), pubkey_bytes.as_slice()].concat().as_slice());
        Self { name, pubkey_bytes, owner_hash, player_id: None, player_ref: None }
    }
}

impl SigningPlayer {
    pub fn from_seed(name: impl Into<String>, seed: u8) -> Self {
        let name = name.into();
        let secp = Secp256k1::new();
        let secret = SecretKey::from_slice(&[seed; 32]).expect("valid deterministic secret key");
        let keypair = Keypair::from_secret_key(&secp, &secret);
        let (x_only, _) = keypair.x_only_public_key();
        let pubkey_bytes = x_only.serialize().to_vec();
        let owner_hash = blake2b(&pubkey_bytes);
        Self { name, keypair, pubkey_bytes, owner_hash, player_id: None, player_ref: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerAccount {
    pub owner_name: String,
    pub owner_hash: Hash,
    pub player_id: Hash,
    pub player_ref: Hash,
    pub value: u64,
    pub open_games: i64,
    pub rating: i64,
    pub games: i64,
    pub wins: i64,
    pub draws: i64,
    pub losses: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameSession {
    pub white_player_ref: Hash,
    pub black_player_ref: Hash,
    pub turn: Side,
    pub move_log: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerTransit {
    pub kind: WorkerKind,
    pub actor: Side,
    pub move_label: String,
    pub white_player_ref: Hash,
    pub black_player_ref: Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettlementTicket {
    pub result: GameResult,
    pub white_player_ref: Hash,
    pub black_player_ref: Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalUtxo {
    LeagueLane,
    Player(PlayerAccount),
    Mux(GameSession),
    Worker(WorkerTransit),
    Settle(SettlementTicket),
}

pub type LocalUtxoId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OffchainMessageKind {
    GameInvite { proposed_white: String, proposed_black: String },
    InviteAccepted { white: String, black: String },
    GameStarted { white: String, black: String },
    MoveNotice { actor: String, worker: WorkerKind, move_label: String, mv: MoveSpec },
    TimeoutClaimAvailable { result: GameResult, worker: WorkerKind, move_label: String },
    SettlementRequest { result: GameResult },
    SettlementNotice { result: GameResult },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OffchainMessage {
    pub from: String,
    pub to: String,
    pub kind: OffchainMessageKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmittedTx {
    pub recipe_name: &'static str,
    pub consumed: Vec<LocalUtxoId>,
    pub produced: Vec<LocalUtxoId>,
    pub signer_names: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveSpec {
    pub from_x: i64,
    pub from_y: i64,
    pub to_x: i64,
    pub to_y: i64,
    pub promo_piece: i64,
}

impl MoveSpec {
    pub fn new(from_x: i64, from_y: i64, to_x: i64, to_y: i64) -> Self {
        Self { from_x, from_y, to_x, to_y, promo_piece: 0 }
    }

    pub fn with_promotion(from_x: i64, from_y: i64, to_x: i64, to_y: i64, promo_piece: i64) -> Self {
        Self { from_x, from_y, to_x, to_y, promo_piece }
    }

    pub fn label(self) -> String {
        format!("{}{}{}{}", file_char(self.from_x), rank_char(self.from_y), file_char(self.to_x), rank_char(self.to_y))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActualGameSnapshot {
    pub white_player_ref: Hash,
    pub black_player_ref: Hash,
    pub phase: String,
    pub board: Vec<u8>,
    pub turn: Side,
    pub status: i64,
    pub move_log: Vec<String>,
}

#[derive(Clone)]
pub struct SigningPlayer {
    pub name: String,
    keypair: Keypair,
    pub pubkey_bytes: Vec<u8>,
    pub owner_hash: Hash,
    pub player_id: Option<Hash>,
    pub player_ref: Option<Hash>,
}

#[derive(Clone, Debug)]
pub struct LocalArena {
    pub planner: ChessTxPlanner,
    utxos: BTreeMap<LocalUtxoId, LocalUtxo>,
    mailboxes: BTreeMap<String, Vec<OffchainMessage>>,
    history: Vec<SubmittedTx>,
    next_utxo_id: LocalUtxoId,
    next_player_nonce: u32,
    base_rating: i64,
}

#[derive(Clone)]
struct TemplateFixture {
    source: &'static str,
    prefix: Vec<u8>,
    suffix: Vec<u8>,
    hash: Hash,
}

#[derive(Clone)]
struct ExecutionFixture {
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

#[derive(Clone)]
struct PlayerStateData {
    owner_hash: Hash,
    player_id: Hash,
    outpoint: TransactionOutpoint,
    value: u64,
    open_games: i64,
    rating: i64,
    games: i64,
    wins: i64,
    draws: i64,
    losses: i64,
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

fn league_source_state(base_rating: i64, admin: Hash) -> BTreeMap<String, ArtifactValue> {
    state! {
        base_rating: base_rating,
        admin: admin,
    }
}

fn player_source_state(state: &PlayerStateData) -> BTreeMap<String, ArtifactValue> {
    state! {
        owner: state.owner_hash,
        player_id: state.player_id,
        open_games: state.open_games,
        rating: state.rating,
        games: state.games,
        wins: state.wins,
        draws: state.draws,
        losses: state.losses,
    }
}

fn game_source_state(state: &GameStateData) -> BTreeMap<String, ArtifactValue> {
    state! {
        white_player: state.white_player,
        black_player: state.black_player,
        board: state.board.clone(),
        turn: state.turn,
        status: state.status,
        move_timeout: state.move_timeout,
        castle_rights: state.castle_rights,
        en_passant_idx: state.en_passant_idx,
        pending_src_idx: state.pending_src_idx,
        pending_dst_idx: state.pending_dst_idx,
        pending_promo: state.pending_promo,
        recent_castle: state.recent_castle,
        draw_state: state.draw_state,
    }
}

fn settle_source_state(white_player: Hash, black_player: Hash, status: i64) -> BTreeMap<String, ArtifactValue> {
    state! {
        white_player: white_player,
        black_player: black_player,
        status: status,
    }
}

fn orchestrator_builder_error(context: &'static str) -> impl FnOnce(argent_runtime::BuilderError) -> OrchestratorError {
    move |err| OrchestratorError(format!("{context}: {err}"))
}

#[derive(Clone)]
struct GameStateData {
    white_player: Hash,
    black_player: Hash,
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
    move_log: Vec<String>,
}

#[derive(Clone)]
struct ActiveWorkerState {
    kind: WorkerKind,
    state: GameStateData,
    outpoint: TransactionOutpoint,
}

#[derive(Clone)]
struct ActiveSettleState {
    white_player: Hash,
    black_player: Hash,
    status: i64,
    outpoint: TransactionOutpoint,
}

pub struct TxArena {
    artifact: Artifact,
    league_state: BTreeMap<String, ArtifactValue>,
    league_outpoint: TransactionOutpoint,
    fix: ExecutionFixture,
    league_template: Hash,
    base_rating: i64,
    player_template: Hash,
    player_prefix: Vec<u8>,
    player_suffix: Vec<u8>,
    covenant_id: Hash,
    players: BTreeMap<String, PlayerStateData>,
    game: Option<GameStateData>,
    game_outpoint: Option<TransactionOutpoint>,
    active_worker: Option<ActiveWorkerState>,
    active_settle: Option<ActiveSettleState>,
    messages: BTreeMap<String, Vec<OffchainMessage>>,
    history: Vec<SubmittedTx>,
    transactions: Vec<Transaction>,
}

#[derive(Clone)]
pub struct TxOrchestrator {
    pub player: SigningPlayer,
    arena: Rc<RefCell<TxArena>>,
}

impl ChessTxPlanner {
    pub fn load() -> Result<Self, OrchestratorError> {
        Ok(Self { family: load_template_family()? })
    }

    pub fn register_player_recipe(&self) -> TxRecipe {
        TxRecipe {
            name: "register_player",
            calls: vec![PlannedCall { role: ContractRole::League, function: "register_player", signer: SignerRequirement::Owner }],
            outputs: vec![
                PlannedOutput { role: ContractRole::League, count: 1 },
                PlannedOutput { role: ContractRole::Player, count: 1 },
            ],
        }
    }

    pub fn start_game_recipe(&self) -> TxRecipe {
        TxRecipe {
            name: "start_game",
            calls: vec![
                PlannedCall { role: ContractRole::Player, function: "start_game", signer: SignerRequirement::Owner },
                PlannedCall { role: ContractRole::Player, function: "delegate_start_game", signer: SignerRequirement::Owner },
            ],
            outputs: vec![PlannedOutput { role: ContractRole::Player, count: 2 }, PlannedOutput { role: ContractRole::Mux, count: 1 }],
        }
    }

    pub fn route_recipe(&self, worker: WorkerKind) -> TxRecipe {
        TxRecipe {
            name: "route",
            calls: vec![PlannedCall { role: ContractRole::Mux, function: "route", signer: SignerRequirement::SideToMove }],
            outputs: vec![PlannedOutput { role: ContractRole::Worker(worker), count: 1 }],
        }
    }

    pub fn worker_apply_recipe(&self, worker: WorkerKind) -> TxRecipe {
        TxRecipe {
            name: "worker_apply",
            calls: vec![PlannedCall { role: ContractRole::Worker(worker), function: "apply", signer: SignerRequirement::None }],
            outputs: vec![PlannedOutput { role: ContractRole::Mux, count: 1 }],
        }
    }

    pub fn mux_timeout_recipe(&self) -> TxRecipe {
        TxRecipe {
            name: "mux_timeout",
            calls: vec![PlannedCall { role: ContractRole::Mux, function: "timeout", signer: SignerRequirement::WaitingOpponent }],
            outputs: vec![PlannedOutput { role: ContractRole::Settle, count: 1 }],
        }
    }

    pub fn worker_timeout_recipe(&self, worker: WorkerKind) -> TxRecipe {
        TxRecipe {
            name: "worker_timeout",
            calls: vec![PlannedCall { role: ContractRole::Worker(worker), function: "timeout", signer: SignerRequirement::None }],
            outputs: vec![PlannedOutput { role: ContractRole::Settle, count: 1 }],
        }
    }

    pub fn settlement_recipe(&self, _result: GameResult) -> SettlementRecipe {
        SettlementRecipe {
            mux_step: TxRecipe {
                name: "mux_settle",
                calls: vec![PlannedCall { role: ContractRole::Mux, function: "settle", signer: SignerRequirement::None }],
                outputs: vec![PlannedOutput { role: ContractRole::Settle, count: 1 }],
            },
            settle_step: TxRecipe {
                name: "settle",
                calls: vec![
                    PlannedCall { role: ContractRole::Settle, function: "settle", signer: SignerRequirement::None },
                    PlannedCall { role: ContractRole::Player, function: "delegate_settle", signer: SignerRequirement::None },
                    PlannedCall { role: ContractRole::Player, function: "delegate_settle", signer: SignerRequirement::None },
                ],
                outputs: vec![PlannedOutput { role: ContractRole::Player, count: 2 }],
            },
        }
    }

    pub fn retire_recipe(&self) -> TxRecipe {
        TxRecipe {
            name: "retire",
            calls: vec![PlannedCall { role: ContractRole::Player, function: "retire", signer: SignerRequirement::Owner }],
            outputs: vec![],
        }
    }
}

impl LocalArena {
    pub fn new(planner: ChessTxPlanner) -> Self {
        let mut utxos = BTreeMap::new();
        utxos.insert(1, LocalUtxo::LeagueLane);
        Self {
            planner,
            utxos,
            mailboxes: BTreeMap::new(),
            history: Vec::new(),
            next_utxo_id: 2,
            next_player_nonce: 0,
            base_rating: 1200,
        }
    }

    pub fn history(&self) -> &[SubmittedTx] {
        &self.history
    }

    pub fn inbox(&self, player: &PlayerHandle) -> &[OffchainMessage] {
        self.mailboxes.get(&player.name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn drain_inbox(&mut self, player: &PlayerHandle) -> Vec<OffchainMessage> {
        self.mailboxes.remove(&player.name).unwrap_or_default()
    }

    pub fn register_player(&mut self, player: &mut PlayerHandle) -> Result<SubmittedTx, OrchestratorError> {
        if player.player_ref.is_some() {
            return Err(OrchestratorError(format!("{} is already registered", player.name)));
        }

        let league_id = self
            .utxos
            .iter()
            .find_map(|(id, utxo)| matches!(utxo, LocalUtxo::LeagueLane).then_some(*id))
            .ok_or_else(|| OrchestratorError("missing league lane".to_string()))?;

        let player_id = derive_player_id(self.next_player_nonce, &player.owner_hash);
        self.next_player_nonce += 1;
        let player_ref = player_ref_hash(player.owner_hash, player_id);

        let account = PlayerAccount {
            owner_name: player.name.clone(),
            owner_hash: player.owner_hash,
            player_id,
            player_ref,
            value: 1_000,
            open_games: 0,
            rating: self.base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        };

        player.player_id = Some(player_id);
        player.player_ref = Some(player_ref);

        let next_league_id = self.alloc_utxo(LocalUtxo::LeagueLane);
        let next_player_id = self.alloc_utxo(LocalUtxo::Player(account));
        self.utxos.remove(&league_id);

        let submission = SubmittedTx {
            recipe_name: self.planner.register_player_recipe().name,
            consumed: vec![league_id],
            produced: vec![next_league_id, next_player_id],
            signer_names: vec![player.name.clone()],
        };
        self.history.push(submission.clone());
        Ok(submission)
    }

    pub fn send_game_invite(&mut self, white: &PlayerHandle, black: &PlayerHandle) -> Result<(), OrchestratorError> {
        self.require_registered(white)?;
        self.require_registered(black)?;
        self.push_message(
            &black.name,
            OffchainMessage {
                from: white.name.clone(),
                to: black.name.clone(),
                kind: OffchainMessageKind::GameInvite { proposed_white: white.name.clone(), proposed_black: black.name.clone() },
            },
        );
        Ok(())
    }

    pub fn start_game(&mut self, white: &PlayerHandle, black: &PlayerHandle) -> Result<SubmittedTx, OrchestratorError> {
        let white_ref = white.player_ref.ok_or_else(|| OrchestratorError("white player is not registered".to_string()))?;
        let black_ref = black.player_ref.ok_or_else(|| OrchestratorError("black player is not registered".to_string()))?;

        let white_id = self.find_player_utxo_id(white_ref)?;
        let black_id = self.find_player_utxo_id(black_ref)?;
        let mut white_account = self.player_account(white_ref)?;
        let mut black_account = self.player_account(black_ref)?;

        white_account.open_games += 1;
        black_account.open_games += 1;

        self.utxos.remove(&white_id);
        self.utxos.remove(&black_id);

        let next_white_id = self.alloc_utxo(LocalUtxo::Player(white_account));
        let next_black_id = self.alloc_utxo(LocalUtxo::Player(black_account));
        let mux_id = self.alloc_utxo(LocalUtxo::Mux(GameSession {
            white_player_ref: white_ref,
            black_player_ref: black_ref,
            turn: Side::White,
            move_log: Vec::new(),
        }));

        let submission = SubmittedTx {
            recipe_name: self.planner.start_game_recipe().name,
            consumed: vec![white_id, black_id],
            produced: vec![next_white_id, next_black_id, mux_id],
            signer_names: vec![white.name.clone(), black.name.clone()],
        };
        self.history.push(submission.clone());
        Ok(submission)
    }

    pub fn submit_move(
        &mut self,
        actor: &PlayerHandle,
        worker: WorkerKind,
        move_label: impl Into<String>,
    ) -> Result<Vec<SubmittedTx>, OrchestratorError> {
        let actor_ref = actor.player_ref.ok_or_else(|| OrchestratorError(format!("{} is not registered", actor.name)))?;
        let move_label = move_label.into();
        let (mux_id, mux) = self.active_mux()?;

        let actor_side = if actor_ref == mux.white_player_ref {
            Side::White
        } else if actor_ref == mux.black_player_ref {
            Side::Black
        } else {
            return Err(OrchestratorError(format!("{} is not part of the current game", actor.name)));
        };

        if actor_side != mux.turn {
            return Err(OrchestratorError(format!("it is not {}'s turn", actor.name)));
        }

        self.utxos.remove(&mux_id);
        let worker_id = self.alloc_utxo(LocalUtxo::Worker(WorkerTransit {
            kind: worker,
            actor: actor_side,
            move_label: move_label.clone(),
            white_player_ref: mux.white_player_ref,
            black_player_ref: mux.black_player_ref,
        }));
        let route_tx = SubmittedTx {
            recipe_name: self.planner.route_recipe(worker).name,
            consumed: vec![mux_id],
            produced: vec![worker_id],
            signer_names: vec![actor.name.clone()],
        };
        self.history.push(route_tx.clone());

        let worker_state = match self.utxos.remove(&worker_id) {
            Some(LocalUtxo::Worker(worker_state)) => worker_state,
            _ => return Err(OrchestratorError("missing worker transit".to_string())),
        };

        let next_turn = worker_state.actor.other();
        let mut move_log = mux.move_log.clone();
        move_log.push(format!("{}:{:?}:{}", actor.name, worker, move_label));
        let next_mux_id = self.alloc_utxo(LocalUtxo::Mux(GameSession {
            white_player_ref: worker_state.white_player_ref,
            black_player_ref: worker_state.black_player_ref,
            turn: next_turn,
            move_log,
        }));
        let apply_tx = SubmittedTx {
            recipe_name: self.planner.worker_apply_recipe(worker).name,
            consumed: vec![worker_id],
            produced: vec![next_mux_id],
            signer_names: vec![],
        };
        self.history.push(apply_tx.clone());

        let recipient = if actor_side == Side::White {
            self.owner_name(worker_state.black_player_ref)?
        } else {
            self.owner_name(worker_state.white_player_ref)?
        };
        self.push_message(
            &recipient,
            OffchainMessage {
                from: actor.name.clone(),
                to: recipient.clone(),
                kind: OffchainMessageKind::MoveNotice {
                    actor: actor.name.clone(),
                    worker,
                    move_label,
                    mv: MoveSpec::new(-1, -1, -1, -1),
                },
            },
        );

        Ok(vec![route_tx, apply_tx])
    }

    pub fn settle_game(
        &mut self,
        white: &PlayerHandle,
        black: &PlayerHandle,
        result: GameResult,
    ) -> Result<Vec<SubmittedTx>, OrchestratorError> {
        self.require_registered(white)?;
        self.require_registered(black)?;
        let (mux_id, mux) = self.active_mux()?;
        let white_ref = white.player_ref.ok_or_else(|| OrchestratorError("white player is not registered".to_string()))?;
        let black_ref = black.player_ref.ok_or_else(|| OrchestratorError("black player is not registered".to_string()))?;
        if mux.white_player_ref != white_ref || mux.black_player_ref != black_ref {
            return Err(OrchestratorError("active mux does not match provided players".to_string()));
        }

        self.utxos.remove(&mux_id);
        let settle_id = self.alloc_utxo(LocalUtxo::Settle(SettlementTicket {
            result,
            white_player_ref: mux.white_player_ref,
            black_player_ref: mux.black_player_ref,
        }));
        let mux_tx = SubmittedTx {
            recipe_name: self.planner.settlement_recipe(result).mux_step.name,
            consumed: vec![mux_id],
            produced: vec![settle_id],
            signer_names: vec![],
        };
        self.history.push(mux_tx.clone());

        let white_player_id = self.find_player_utxo_id(white_ref)?;
        let black_player_id = self.find_player_utxo_id(black_ref)?;
        let mut white_account = self.player_account(white_ref)?;
        let mut black_account = self.player_account(black_ref)?;
        if white_account.open_games <= 0 || black_account.open_games <= 0 {
            return Err(OrchestratorError("cannot settle players without open games".to_string()));
        }

        white_account.open_games -= 1;
        black_account.open_games -= 1;
        white_account.games += 1;
        black_account.games += 1;

        let (white_actual, black_actual) = match result {
            GameResult::WhiteWin => {
                white_account.wins += 1;
                black_account.losses += 1;
                (1000, 0)
            }
            GameResult::BlackWin => {
                white_account.losses += 1;
                black_account.wins += 1;
                (0, 1000)
            }
            GameResult::Draw => {
                white_account.draws += 1;
                black_account.draws += 1;
                (500, 500)
            }
        };

        let white_old_rating = white_account.rating;
        let black_old_rating = black_account.rating;
        white_account.rating = approx_updated_rating(white_old_rating, black_old_rating, white_actual);
        black_account.rating = approx_updated_rating(black_old_rating, white_old_rating, black_actual);

        let stake = 1_000u64;
        match result {
            GameResult::WhiteWin => {
                white_account.value += stake;
            }
            GameResult::BlackWin => {
                black_account.value += stake;
            }
            GameResult::Draw => {
                let white_share = stake / 2;
                let black_share = stake - white_share;
                white_account.value += white_share;
                black_account.value += black_share;
            }
        }

        self.utxos.remove(&settle_id);
        self.utxos.remove(&white_player_id);
        self.utxos.remove(&black_player_id);

        let next_white_id = self.alloc_utxo(LocalUtxo::Player(white_account));
        let next_black_id = self.alloc_utxo(LocalUtxo::Player(black_account));
        let settle_tx = SubmittedTx {
            recipe_name: self.planner.settlement_recipe(result).settle_step.name,
            consumed: vec![settle_id, white_player_id, black_player_id],
            produced: vec![next_white_id, next_black_id],
            signer_names: vec![],
        };
        self.history.push(settle_tx.clone());

        self.push_message(
            &white.name,
            OffchainMessage {
                from: "arena".to_string(),
                to: white.name.clone(),
                kind: OffchainMessageKind::SettlementNotice { result },
            },
        );
        self.push_message(
            &black.name,
            OffchainMessage {
                from: "arena".to_string(),
                to: black.name.clone(),
                kind: OffchainMessageKind::SettlementNotice { result },
            },
        );

        Ok(vec![mux_tx, settle_tx])
    }

    pub fn retire_player(&mut self, player: &PlayerHandle) -> Result<SubmittedTx, OrchestratorError> {
        let player_ref = player.player_ref.ok_or_else(|| OrchestratorError(format!("{} is not registered", player.name)))?;
        let player_id = self.find_player_utxo_id(player_ref)?;
        let account = self.player_account(player_ref)?;
        if account.open_games != 0 {
            return Err(OrchestratorError(format!("{} still has open games", player.name)));
        }
        self.utxos.remove(&player_id);
        let submission = SubmittedTx {
            recipe_name: self.planner.retire_recipe().name,
            consumed: vec![player_id],
            produced: vec![],
            signer_names: vec![player.name.clone()],
        };
        self.history.push(submission.clone());
        Ok(submission)
    }

    pub fn player_account_snapshot(&self, player: &PlayerHandle) -> Result<PlayerAccount, OrchestratorError> {
        let player_ref = player.player_ref.ok_or_else(|| OrchestratorError(format!("{} is not registered", player.name)))?;
        self.player_account(player_ref)
    }

    pub fn active_game_snapshot(&self) -> Option<GameSession> {
        self.utxos.values().find_map(|utxo| match utxo {
            LocalUtxo::Mux(game) => Some(game.clone()),
            _ => None,
        })
    }

    fn alloc_utxo(&mut self, utxo: LocalUtxo) -> LocalUtxoId {
        let id = self.next_utxo_id;
        self.next_utxo_id += 1;
        self.utxos.insert(id, utxo);
        id
    }

    fn push_message(&mut self, recipient: &str, message: OffchainMessage) {
        self.mailboxes.entry(recipient.to_string()).or_default().push(message);
    }

    fn require_registered(&self, player: &PlayerHandle) -> Result<(), OrchestratorError> {
        if player.player_ref.is_none() || player.player_id.is_none() {
            return Err(OrchestratorError(format!("{} is not registered", player.name)));
        }
        Ok(())
    }

    fn find_player_utxo_id(&self, player_ref: Hash) -> Result<LocalUtxoId, OrchestratorError> {
        self.utxos
            .iter()
            .find_map(|(id, utxo)| match utxo {
                LocalUtxo::Player(account) if account.player_ref == player_ref => Some(*id),
                _ => None,
            })
            .ok_or_else(|| OrchestratorError("missing player UTXO".to_string()))
    }

    fn player_account(&self, player_ref: Hash) -> Result<PlayerAccount, OrchestratorError> {
        self.utxos
            .values()
            .find_map(|utxo| match utxo {
                LocalUtxo::Player(account) if account.player_ref == player_ref => Some(account.clone()),
                _ => None,
            })
            .ok_or_else(|| OrchestratorError("missing player account".to_string()))
    }

    fn active_mux(&self) -> Result<(LocalUtxoId, GameSession), OrchestratorError> {
        self.utxos
            .iter()
            .find_map(|(id, utxo)| match utxo {
                LocalUtxo::Mux(game) => Some((*id, game.clone())),
                _ => None,
            })
            .ok_or_else(|| OrchestratorError("missing active mux".to_string()))
    }

    fn owner_name(&self, player_ref: Hash) -> Result<String, OrchestratorError> {
        Ok(self.player_account(player_ref)?.owner_name)
    }
}

impl TxOrchestrator {
    pub fn new(name: impl Into<String>, seed: u8, arena: Rc<RefCell<TxArena>>) -> Self {
        Self { player: SigningPlayer::from_seed(name, seed), arena }
    }

    pub fn inbox(&self) -> Vec<OffchainMessage> {
        self.arena.borrow_mut().drain_messages(&self.player.name)
    }

    pub fn register(&mut self) -> Result<(), OrchestratorError> {
        self.arena.borrow_mut().register_player(&mut self.player)
    }

    pub fn send_game_invite(&self, other: &TxOrchestrator) -> Result<(), OrchestratorError> {
        self.arena.borrow_mut().send_game_invite(&self.player, &other.player)
    }

    pub fn accept_game_invite(&self, other: &TxOrchestrator) -> Result<(), OrchestratorError> {
        self.arena.borrow_mut().accept_game_invite(&self.player, &other.player)
    }

    pub fn start_game(&self, other: &TxOrchestrator) -> Result<(), OrchestratorError> {
        self.arena.borrow_mut().start_game(&self.player, &other.player)
    }

    pub fn submit_move(&self, mv: MoveSpec) -> Result<Vec<SubmittedTx>, OrchestratorError> {
        self.arena.borrow_mut().submit_move(&self.player, mv)
    }

    pub fn force_move(&self, mv: MoveSpec) -> Result<Vec<SubmittedTx>, OrchestratorError> {
        self.arena.borrow_mut().force_move(&self.player, mv)
    }

    pub fn surrender(&self) -> Result<(), OrchestratorError> {
        self.arena.borrow_mut().surrender(&self.player)
    }

    pub fn claim_timeout(&self) -> Result<(), OrchestratorError> {
        self.arena.borrow_mut().claim_timeout(&self.player)
    }

    pub fn request_settlement(&self, other: &TxOrchestrator, result: GameResult) -> Result<(), OrchestratorError> {
        self.arena.borrow_mut().request_settlement(&self.player, &other.player, result)
    }

    pub fn settle(&self, other: &TxOrchestrator, result: GameResult) -> Result<(), OrchestratorError> {
        self.arena.borrow_mut().settle_game(&self.player, &other.player, result)
    }

    pub fn retire(&self) -> Result<(), OrchestratorError> {
        self.arena.borrow_mut().retire_player(&self.player)
    }
}

impl TxArena {
    pub fn new() -> Result<Self, OrchestratorError> {
        let artifact: Artifact = serde_json::from_str(include_str!("../../build/argent/artifact.json"))
            .map_err(|err| OrchestratorError(format!("failed to load pinned Argent artifact: {err}")))?;
        let fix = build_execution_fixture();
        let league_template = repeated_hash(0x11);
        let admin = repeated_hash(0x33);
        let base_rating = 1200;
        let routes_commitment = routes_commitment(&packed_execution_route_templates(&fix));
        let player_contract = compile_player_state(
            player_static_source(),
            PlayerStateArgs {
                league_template: &repeated_hash(0x11),
                player_template: &repeated_hash(0x22),
                mux_template: &fix.mux.hash,
                routes_commitment: &routes_commitment,
                owner_hash: &repeated_hash(0x44),
                player_id: &repeated_hash(0x55),
                open_games: 0,
                rating: base_rating,
                games: 0,
                wins: 0,
                draws: 0,
                losses: 0,
            },
        );
        let layout = player_contract.state_layout;
        let player_prefix = player_contract.script[..layout.start].to_vec();
        let player_suffix = player_contract.script[layout.start + layout.len..].to_vec();
        let player_template = Hash::from_bytes(player_contract.template_hash());
        let league_state = league_source_state(base_rating, admin);
        let league_output = {
            let builder = TxBuilder::new(&artifact).map_err(orchestrator_builder_error("initialize Argent builder"))?;
            let funding_outpoint = TransactionOutpoint::new(TransactionId::from_bytes([0x77; 32]), 0);
            let funding_utxo = UtxoEntry::new(1_000, ScriptPublicKey::from_vec(0, vec![0x51]), 0, false, None);
            let context = TxContext::new().input(funding_outpoint, funding_utxo, Vec::new(), 0).actor_genesis_output(
                0,
                "launch::league",
                "League",
                league_state.clone(),
                1_000,
            );
            let tx = builder.build(&context).map_err(orchestrator_builder_error("launch League"))?;
            argent_runtime::CovenantOutput::from_tx(&tx, 0).map_err(orchestrator_builder_error("read League genesis output"))?
        };

        Ok(Self {
            artifact,
            league_state,
            league_outpoint: league_output.outpoint,
            fix,
            league_template,
            base_rating,
            player_template,
            player_prefix,
            player_suffix,
            covenant_id: league_output.covenant_id,
            players: BTreeMap::new(),
            game: None,
            game_outpoint: None,
            active_worker: None,
            active_settle: None,
            messages: BTreeMap::new(),
            history: Vec::new(),
            transactions: Vec::new(),
        })
    }

    pub fn shared() -> Result<Rc<RefCell<Self>>, OrchestratorError> {
        Ok(Rc::new(RefCell::new(Self::new()?)))
    }

    pub fn drain_messages(&mut self, name: &str) -> Vec<OffchainMessage> {
        self.messages.remove(name).unwrap_or_default()
    }

    pub fn history(&self) -> &[SubmittedTx] {
        &self.history
    }

    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    pub fn covenant_id(&self) -> Hash {
        self.covenant_id
    }

    pub fn player_account_snapshot(&self, player: &SigningPlayer) -> Result<PlayerAccount, OrchestratorError> {
        let player_ref = player.player_ref.ok_or_else(|| OrchestratorError(format!("{} is not registered", player.name)))?;
        self.player_account(player_ref)
    }

    fn owner_name(&self, player_ref: Hash) -> Result<String, OrchestratorError> {
        self.players
            .iter()
            .find_map(|(name, state)| (player_ref_hash(state.owner_hash, state.player_id) == player_ref).then_some(name.clone()))
            .ok_or_else(|| OrchestratorError("missing player owner".to_string()))
    }

    pub fn active_game_snapshot(&self) -> Option<ActualGameSnapshot> {
        self.game
            .as_ref()
            .map(|game| ActualGameSnapshot {
                white_player_ref: game.white_player,
                black_player_ref: game.black_player,
                phase: "mux".to_string(),
                board: game.board.clone(),
                turn: side_from_turn(game.turn),
                status: game.status,
                move_log: game.move_log.clone(),
            })
            .or_else(|| {
                self.active_worker.as_ref().map(|worker| ActualGameSnapshot {
                    white_player_ref: worker.state.white_player,
                    black_player_ref: worker.state.black_player,
                    phase: format!("worker:{:?}", worker.kind),
                    board: worker.state.board.clone(),
                    turn: side_from_turn(worker.state.turn),
                    status: worker.state.status,
                    move_log: worker.state.move_log.clone(),
                })
            })
    }

    pub fn register_player(&mut self, player: &mut SigningPlayer) -> Result<(), OrchestratorError> {
        let player_id = blake2b(
            &[
                b"LeaguePlayerId".as_slice(),
                self.league_outpoint.transaction_id.as_bytes().as_slice(),
                &self.league_outpoint.index.to_le_bytes(),
            ]
            .concat(),
        );
        let player_ref = player_ref_hash(player.owner_hash, player_id);
        let registered = PlayerStateData {
            owner_hash: player.owner_hash,
            player_id,
            outpoint: self.league_outpoint,
            value: 1_000,
            open_games: 0,
            rating: self.base_rating,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        };
        let executed_tx = {
            let builder = TxBuilder::new(&self.artifact).map_err(orchestrator_builder_error("initialize Argent builder"))?;
            let league_utxo = builder
                .covenant_utxo("League", self.league_state.clone(), 1_000, 0, false, Some(self.covenant_id))
                .map_err(orchestrator_builder_error("build League UTXO"))?;
            let keypair = player.keypair;
            let public_key = player.pubkey_bytes.clone();
            let context = TxContext::new()
                .actor_input(
                    "League",
                    self.league_state.clone(),
                    EntryCall::new("register_player")
                        .args_with(move |tx, input_index| args![sign_builder_input(tx, input_index, &keypair), public_key.clone()]),
                    self.league_outpoint,
                    league_utxo,
                    0,
                )
                .actor_output("League", self.league_state.clone(), CovenantBinding::new(0, self.covenant_id), 1_000)
                .actor_output("Player", player_source_state(&registered), CovenantBinding::new(0, self.covenant_id), registered.value);
            builder.build(&context).map_err(orchestrator_builder_error("register player"))?
        };
        let executed_txid = executed_tx.id();
        self.league_outpoint = TransactionOutpoint::new(executed_txid, 0);
        self.transactions.push(executed_tx);

        player.player_id = Some(player_id);
        player.player_ref = Some(player_ref);
        self.players
            .insert(player.name.clone(), PlayerStateData { outpoint: TransactionOutpoint::new(executed_txid, 1), ..registered });
        self.history.push(SubmittedTx {
            recipe_name: "register_player",
            consumed: vec![],
            produced: vec![],
            signer_names: vec![player.name.clone()],
        });
        Ok(())
    }

    pub fn send_game_invite(&mut self, white: &SigningPlayer, black: &SigningPlayer) -> Result<(), OrchestratorError> {
        self.require_registered(white)?;
        self.require_registered(black)?;
        self.push_message(
            &black.name,
            OffchainMessage {
                from: white.name.clone(),
                to: black.name.clone(),
                kind: OffchainMessageKind::GameInvite { proposed_white: white.name.clone(), proposed_black: black.name.clone() },
            },
        );
        Ok(())
    }

    pub fn accept_game_invite(&mut self, black: &SigningPlayer, white: &SigningPlayer) -> Result<(), OrchestratorError> {
        self.require_registered(white)?;
        self.require_registered(black)?;
        self.push_message(
            &white.name,
            OffchainMessage {
                from: black.name.clone(),
                to: white.name.clone(),
                kind: OffchainMessageKind::InviteAccepted { white: white.name.clone(), black: black.name.clone() },
            },
        );
        Ok(())
    }

    pub fn start_game(&mut self, white: &SigningPlayer, black: &SigningPlayer) -> Result<(), OrchestratorError> {
        let white_state = self.players.get(&white.name).cloned().ok_or_else(|| OrchestratorError("missing white".to_string()))?;
        let black_state = self.players.get(&black.name).cloned().ok_or_else(|| OrchestratorError("missing black".to_string()))?;

        let mut next_white = white_state.clone();
        next_white.open_games += 1;
        let mut next_black = black_state.clone();
        next_black.open_games += 1;

        let white_ref = white.player_ref.ok_or_else(|| OrchestratorError("white missing player ref".to_string()))?;
        let black_ref = black.player_ref.ok_or_else(|| OrchestratorError("black missing player ref".to_string()))?;
        let opening = GameStateData {
            white_player: white_ref,
            black_player: black_ref,
            board: standard_board(),
            turn: 0,
            status: 0,
            move_timeout: DEFAULT_MOVE_TIMEOUT,
            castle_rights: [1, 1, 1, 1],
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
            move_log: Vec::new(),
        };
        let builder = TxBuilder::new(&self.artifact).map_err(orchestrator_builder_error("initialize Argent builder"))?;
        let white_utxo = builder
            .covenant_utxo("Player", player_source_state(&white_state), white_state.value, 0, false, Some(self.covenant_id))
            .map_err(orchestrator_builder_error("build white Player UTXO"))?;
        let black_utxo = builder
            .covenant_utxo("Player", player_source_state(&black_state), black_state.value, 0, false, Some(self.covenant_id))
            .map_err(orchestrator_builder_error("build black Player UTXO"))?;
        let white_keypair = white.keypair;
        let white_public_key = white.pubkey_bytes.clone();
        let black_keypair = black.keypair;
        let black_public_key = black.pubkey_bytes.clone();
        let context = TxContext::new()
            .actor_input(
                "Player",
                player_source_state(&white_state),
                EntryCall::new("start_game").args_with(move |tx, input_index| {
                    args![sign_builder_input(tx, input_index, &white_keypair), white_public_key.clone(), WHITE, DEFAULT_MOVE_TIMEOUT,]
                }),
                white_state.outpoint,
                white_utxo,
                0,
            )
            .actor_input(
                "Player",
                player_source_state(&black_state),
                EntryCall::new("delegate_start_game").args_with(move |tx, input_index| {
                    args![sign_builder_input(tx, input_index, &black_keypair), black_public_key.clone(), DEFAULT_MOVE_TIMEOUT]
                }),
                black_state.outpoint,
                black_utxo,
                0,
            )
            .actor_output("Player", player_source_state(&next_white), CovenantBinding::new(0, self.covenant_id), next_white.value)
            .actor_output("Player", player_source_state(&next_black), CovenantBinding::new(0, self.covenant_id), next_black.value)
            .actor_output("ChessMux", game_source_state(&opening), CovenantBinding::new(0, self.covenant_id), 1_000);
        let executed_tx = builder.build(&context).map_err(orchestrator_builder_error("start game"))?;
        let executed_txid = executed_tx.id();
        self.transactions.push(executed_tx);

        self.players.insert(white.name.clone(), next_white);
        self.players.insert(black.name.clone(), next_black);
        self.players.get_mut(&white.name).expect("white tracked").outpoint =
            TransactionOutpoint { transaction_id: executed_txid, index: 0 };
        self.players.get_mut(&black.name).expect("black tracked").outpoint =
            TransactionOutpoint { transaction_id: executed_txid, index: 1 };
        self.game = Some(opening);
        self.game_outpoint = Some(TransactionOutpoint { transaction_id: executed_txid, index: 2 });
        self.push_message(
            &white.name,
            OffchainMessage {
                from: "arena".to_string(),
                to: white.name.clone(),
                kind: OffchainMessageKind::GameStarted { white: white.name.clone(), black: black.name.clone() },
            },
        );
        self.push_message(
            &black.name,
            OffchainMessage {
                from: "arena".to_string(),
                to: black.name.clone(),
                kind: OffchainMessageKind::GameStarted { white: white.name.clone(), black: black.name.clone() },
            },
        );
        self.history.push(SubmittedTx {
            recipe_name: "start_game",
            consumed: vec![],
            produced: vec![],
            signer_names: vec![white.name.clone(), black.name.clone()],
        });
        Ok(())
    }

    pub fn submit_move(&mut self, actor: &SigningPlayer, mv: MoveSpec) -> Result<Vec<SubmittedTx>, OrchestratorError> {
        self.submit_move_internal(actor, mv, false)
    }

    pub fn force_move(&mut self, actor: &SigningPlayer, mv: MoveSpec) -> Result<Vec<SubmittedTx>, OrchestratorError> {
        self.submit_move_internal(actor, mv, true)
    }

    fn submit_move_internal(
        &mut self,
        actor: &SigningPlayer,
        mv: MoveSpec,
        allow_partial_commit: bool,
    ) -> Result<Vec<SubmittedTx>, OrchestratorError> {
        let game = self.game.clone().ok_or_else(|| OrchestratorError("missing game".to_string()))?;
        let actor_ref = actor.player_ref.ok_or_else(|| OrchestratorError(format!("{} missing player ref", actor.name)))?;
        let actor_side = if actor_ref == game.white_player {
            Side::White
        } else if actor_ref == game.black_player {
            Side::Black
        } else {
            return Err(OrchestratorError(format!("{} is not part of the active game", actor.name)));
        };
        if actor_side != side_from_turn(game.turn) {
            return Err(OrchestratorError(format!("it is not {}'s turn", actor.name)));
        }

        let worker = determine_worker(&game.board, mv)?;
        let pending = pending_state_for_move(&game, mv);
        let game_outpoint = self.game_outpoint.ok_or_else(|| OrchestratorError("missing game outpoint".to_string()))?;
        let builder = TxBuilder::new(&self.artifact).map_err(orchestrator_builder_error("initialize Argent builder"))?;
        let game_utxo = builder
            .covenant_utxo("ChessMux", game_source_state(&game), 1_000, 0, false, Some(self.covenant_id))
            .map_err(orchestrator_builder_error("build active ChessMux UTXO"))?;
        let selected_worker = worker_actor(worker).to_string();
        let keypair = actor.keypair;
        let public_key = actor.pubkey_bytes.clone();
        let player_id = actor.player_id.ok_or_else(|| OrchestratorError("missing player id".to_string()))?;
        let route_context = TxContext::new()
            .actor_input(
                "ChessMux",
                game_source_state(&game),
                EntryCall::new("route").args_with(move |tx, input_index| {
                    args![
                        actor_arg(selected_worker.clone()),
                        mv.from_x,
                        mv.from_y,
                        mv.to_x,
                        mv.to_y,
                        mv.promo_piece,
                        CLEAR,
                        sign_builder_input(tx, input_index, &keypair),
                        public_key.clone(),
                        player_id,
                    ]
                }),
                game_outpoint,
                game_utxo,
                0,
            )
            .actor_output(worker_actor(worker), game_source_state(&pending), CovenantBinding::new(0, self.covenant_id), 1_000);
        let executed_route_tx = builder.build(&route_context).map_err(orchestrator_builder_error("route move to worker"))?;
        let worker_output = argent_runtime::CovenantOutput::from_tx(&executed_route_tx, 0)
            .map_err(orchestrator_builder_error("read worker output"))?;
        let worker_outpoint = worker_output.outpoint;
        let next = apply_worker_state(worker, &pending, mv, allow_partial_commit)?;
        let apply_context = TxContext::new()
            .actor_input(worker_actor(worker), game_source_state(&pending), "apply", worker_output.outpoint, worker_output.utxo, 0)
            .actor_output("ChessMux", game_source_state(&next), CovenantBinding::new(0, self.covenant_id), 1_000);
        let apply_result = builder.build(&apply_context);
        let executed_apply_tx = match apply_result {
            Ok(tx) => tx,
            Err(err) => {
                if !allow_partial_commit {
                    return Err(OrchestratorError(format!("apply failed: {err}")));
                }
                self.transactions.push(executed_route_tx);
                self.game = None;
                self.game_outpoint = None;
                self.active_worker = Some(ActiveWorkerState { kind: worker, state: pending, outpoint: worker_outpoint });

                let winner = actor_side.other();
                let result = if winner == Side::White { GameResult::WhiteWin } else { GameResult::BlackWin };
                let recipient = if winner == Side::White {
                    self.players
                        .iter()
                        .find_map(|(name, state)| {
                            (player_ref_hash(state.owner_hash, state.player_id) == game.white_player).then_some(name.clone())
                        })
                        .ok_or_else(|| OrchestratorError("missing white owner".to_string()))?
                } else {
                    self.players
                        .iter()
                        .find_map(|(name, state)| {
                            (player_ref_hash(state.owner_hash, state.player_id) == game.black_player).then_some(name.clone())
                        })
                        .ok_or_else(|| OrchestratorError("missing black owner".to_string()))?
                };
                self.push_message(
                    &recipient,
                    OffchainMessage {
                        from: actor.name.clone(),
                        to: recipient.clone(),
                        kind: OffchainMessageKind::TimeoutClaimAvailable { result, worker, move_label: mv.label() },
                    },
                );
                let route_submission = SubmittedTx {
                    recipe_name: self.planner().route_recipe(worker).name,
                    consumed: vec![],
                    produced: vec![],
                    signer_names: vec![actor.name.clone()],
                };
                self.history.push(route_submission.clone());
                return Ok(vec![route_submission]);
            }
        };

        self.transactions.push(executed_route_tx);
        self.transactions.push(executed_apply_tx);

        let move_label = mv.label();
        let recipient = if actor_side == Side::White {
            self.players
                .iter()
                .find_map(|(name, state)| {
                    (player_ref_hash(state.owner_hash, state.player_id) == game.black_player).then_some(name.clone())
                })
                .ok_or_else(|| OrchestratorError("missing black owner".to_string()))?
        } else {
            self.players
                .iter()
                .find_map(|(name, state)| {
                    (player_ref_hash(state.owner_hash, state.player_id) == game.white_player).then_some(name.clone())
                })
                .ok_or_else(|| OrchestratorError("missing white owner".to_string()))?
        };
        self.push_message(
            &recipient,
            OffchainMessage {
                from: actor.name.clone(),
                to: recipient.clone(),
                kind: OffchainMessageKind::MoveNotice { actor: actor.name.clone(), worker, move_label: move_label.clone(), mv },
            },
        );
        self.game = Some(next);
        self.game_outpoint =
            Some(TransactionOutpoint { transaction_id: self.transactions.last().expect("apply tx exists").id(), index: 0 });

        let submissions = vec![
            SubmittedTx {
                recipe_name: self.planner().route_recipe(worker).name,
                consumed: vec![],
                produced: vec![],
                signer_names: vec![actor.name.clone()],
            },
            SubmittedTx {
                recipe_name: self.planner().worker_apply_recipe(worker).name,
                consumed: vec![],
                produced: vec![],
                signer_names: vec![],
            },
        ];
        self.history.extend(submissions.clone());
        Ok(submissions)
    }

    pub fn claim_timeout(&mut self, claimer: &SigningPlayer) -> Result<(), OrchestratorError> {
        let active_worker = self.active_worker.clone().ok_or_else(|| OrchestratorError("missing active worker".to_string()))?;
        let claimer_ref = claimer.player_ref.ok_or_else(|| OrchestratorError(format!("{} missing player ref", claimer.name)))?;
        let timed_out_side = side_from_turn(active_worker.state.turn);
        let winner = timed_out_side.other();
        let (winner_ref, loser_ref, result, status) = if winner == Side::White {
            (active_worker.state.white_player, active_worker.state.black_player, GameResult::WhiteWin, 1)
        } else {
            (active_worker.state.black_player, active_worker.state.white_player, GameResult::BlackWin, 2)
        };
        if claimer_ref != winner_ref {
            return Err(OrchestratorError(format!("{} is not entitled to claim this timeout", claimer.name)));
        }

        let builder = TxBuilder::new(&self.artifact).map_err(orchestrator_builder_error("initialize Argent builder"))?;
        let worker_utxo = builder
            .covenant_utxo(
                worker_actor(active_worker.kind),
                game_source_state(&active_worker.state),
                1_000,
                0,
                false,
                Some(self.covenant_id),
            )
            .map_err(orchestrator_builder_error("build timed-out worker UTXO"))?;
        let context = TxContext::new()
            .actor_input(
                worker_actor(active_worker.kind),
                game_source_state(&active_worker.state),
                "timeout",
                active_worker.outpoint,
                worker_utxo,
                DEFAULT_MOVE_TIMEOUT as u64,
            )
            .actor_output(
                "ChessSettle",
                settle_source_state(active_worker.state.white_player, active_worker.state.black_player, status),
                CovenantBinding::new(0, self.covenant_id),
                1_000,
            );
        let executed_tx = builder.build(&context).map_err(orchestrator_builder_error("claim worker timeout"))?;
        self.transactions.push(executed_tx.clone());
        self.active_worker = None;
        self.active_settle = Some(ActiveSettleState {
            white_player: active_worker.state.white_player,
            black_player: active_worker.state.black_player,
            status,
            outpoint: TransactionOutpoint { transaction_id: executed_tx.id(), index: 0 },
        });
        self.push_message(
            &claimer.name,
            OffchainMessage {
                from: "arena".to_string(),
                to: claimer.name.clone(),
                kind: OffchainMessageKind::SettlementRequest { result },
            },
        );
        let loser_name = self.owner_name(loser_ref)?;
        self.push_message(
            &loser_name,
            OffchainMessage {
                from: "arena".to_string(),
                to: loser_name.clone(),
                kind: OffchainMessageKind::SettlementRequest { result },
            },
        );
        self.history.push(SubmittedTx {
            recipe_name: self.planner().worker_timeout_recipe(active_worker.kind).name,
            consumed: vec![],
            produced: vec![],
            signer_names: vec![],
        });
        Ok(())
    }

    pub fn surrender(&mut self, actor: &SigningPlayer) -> Result<(), OrchestratorError> {
        let game = self.game.clone().ok_or_else(|| OrchestratorError("missing game".to_string()))?;
        let actor_ref = actor.player_ref.ok_or_else(|| OrchestratorError(format!("{} missing player ref", actor.name)))?;
        let actor_side = if actor_ref == game.white_player {
            Side::White
        } else if actor_ref == game.black_player {
            Side::Black
        } else {
            return Err(OrchestratorError(format!("{} is not part of the active game", actor.name)));
        };
        if actor_side != side_from_turn(game.turn) {
            return Err(OrchestratorError(format!("it is not {}'s turn", actor.name)));
        }

        let active = self.compile_mux(&game);
        let next = GameStateData {
            white_player: game.white_player,
            black_player: game.black_player,
            board: game.board.clone(),
            turn: game.turn,
            status: if game.turn == 0 { 2 } else { 1 },
            move_timeout: game.move_timeout,
            castle_rights: game.castle_rights,
            en_passant_idx: -1,
            pending_src_idx: -1,
            pending_dst_idx: -1,
            pending_promo: 0,
            recent_castle: 0,
            draw_state: 3,
            move_log: game.move_log.clone(),
        };
        let terminal = self.compile_mux(&next);
        let placeholder = entry_sigscript(
            &active,
            "route",
            vec![
                Expr::int(8),
                Expr::int(-1),
                Expr::int(-1),
                Expr::int(-1),
                Expr::int(-1),
                Expr::int(0),
                Expr::int(3),
                Expr::bytes(vec![0u8; 65]),
                Expr::bytes(actor.pubkey_bytes.clone()),
                hash_expr(actor.player_id.ok_or_else(|| OrchestratorError("missing player id".to_string()))?),
                Expr::bytes(self.fix.mux.prefix.clone()),
                Expr::bytes(self.fix.mux.suffix.clone()),
            ],
        );
        let outputs = vec![covenant_output(&terminal, 0, self.covenant_id)];
        let entries = vec![covenant_utxo(&active, self.covenant_id)];
        let mut tx = Transaction::new(
            1,
            vec![tx_input(self.game_outpoint.ok_or_else(|| OrchestratorError("missing game outpoint".to_string()))?, placeholder, 1)],
            outputs,
            0,
            Default::default(),
            0,
            vec![],
        );
        let sig = sign_tx_input_schnorr(&tx, &entries, 0, actor);
        tx.inputs[0].signature_script = entry_sigscript(
            &active,
            "route",
            vec![
                Expr::int(8),
                Expr::int(-1),
                Expr::int(-1),
                Expr::int(-1),
                Expr::int(-1),
                Expr::int(0),
                Expr::int(3),
                Expr::bytes(sig),
                Expr::bytes(actor.pubkey_bytes.clone()),
                hash_expr(actor.player_id.ok_or_else(|| OrchestratorError("missing player id".to_string()))?),
                Expr::bytes(self.fix.mux.prefix.clone()),
                Expr::bytes(self.fix.mux.suffix.clone()),
            ],
        );
        let executed_tx = tx.clone();
        execute_input_with_covenants(tx, entries, 0).map_err(|err| OrchestratorError(format!("surrender failed: {err}")))?;
        self.transactions.push(executed_tx);
        self.game = Some(next);
        self.game_outpoint =
            Some(TransactionOutpoint { transaction_id: self.transactions.last().expect("surrender tx exists").id(), index: 0 });
        self.history.push(SubmittedTx {
            recipe_name: "route",
            consumed: vec![],
            produced: vec![],
            signer_names: vec![actor.name.clone()],
        });
        Ok(())
    }

    pub fn request_settlement(
        &mut self,
        requester: &SigningPlayer,
        opponent: &SigningPlayer,
        result: GameResult,
    ) -> Result<(), OrchestratorError> {
        self.require_registered(requester)?;
        self.require_registered(opponent)?;
        let game = self.game.as_ref().ok_or_else(|| OrchestratorError("missing game".to_string()))?;
        let requester_ref =
            requester.player_ref.as_ref().ok_or_else(|| OrchestratorError("requester missing player ref".to_string()))?;
        let opponent_ref = opponent.player_ref.as_ref().ok_or_else(|| OrchestratorError("opponent missing player ref".to_string()))?;
        let white_name = if requester_ref == &game.white_player {
            requester.name.clone()
        } else if opponent_ref == &game.white_player {
            opponent.name.clone()
        } else {
            return Err(OrchestratorError("missing white player in settlement request".to_string()));
        };
        let black_name = if requester_ref == &game.black_player {
            requester.name.clone()
        } else if opponent_ref == &game.black_player {
            opponent.name.clone()
        } else {
            return Err(OrchestratorError("missing black player in settlement request".to_string()));
        };
        for recipient in [white_name, black_name] {
            self.push_message(
                &recipient,
                OffchainMessage {
                    from: requester.name.clone(),
                    to: recipient.clone(),
                    kind: OffchainMessageKind::SettlementRequest { result },
                },
            );
        }
        Ok(())
    }

    pub fn settle_game(&mut self, white: &SigningPlayer, black: &SigningPlayer, result: GameResult) -> Result<(), OrchestratorError> {
        let expected_status = status_from_result(result);
        let white_state = self.players.get(&white.name).cloned().ok_or_else(|| OrchestratorError("missing white".to_string()))?;
        let black_state = self.players.get(&black.name).cloned().ok_or_else(|| OrchestratorError("missing black".to_string()))?;
        let white_contract = self.compile_player(&white_state);
        let black_contract = self.compile_player(&black_state);

        let white_ref = white.player_ref.ok_or_else(|| OrchestratorError("white missing player ref".to_string()))?;
        let black_ref = black.player_ref.ok_or_else(|| OrchestratorError("black missing player ref".to_string()))?;
        let (routed_settle, settle_outpoint, include_mux_settle_history) = if let Some(active_settle) = self.active_settle.clone() {
            if active_settle.status != expected_status {
                return Err(OrchestratorError(format!(
                    "active settle status {} does not match requested result {}",
                    active_settle.status, expected_status
                )));
            }
            if active_settle.white_player != white_ref || active_settle.black_player != black_ref {
                return Err(OrchestratorError("active settle does not match provided players".to_string()));
            }
            (
                compile_settle_state(self.fix.settle.source, &self.player_template, &white_ref, &black_ref, expected_status),
                active_settle.outpoint,
                false,
            )
        } else {
            let game = self.game.clone().ok_or_else(|| OrchestratorError("missing game".to_string()))?;
            if game.status != expected_status {
                return Err(OrchestratorError(format!(
                    "terminal game status {} does not match requested result {}",
                    game.status, expected_status
                )));
            }
            let terminal = self.compile_mux(&game);
            let routed_settle =
                compile_settle_state(self.fix.settle.source, &self.player_template, &white_ref, &black_ref, expected_status);
            let mux_settle_sigscript = entry_sigscript(
                &terminal,
                "settle",
                vec![
                    hash_expr(self.player_template),
                    Expr::bytes(self.fix.settle.prefix.clone()),
                    Expr::bytes(self.fix.settle.suffix.clone()),
                ],
            );
            let mux_tx = Transaction::new(
                1,
                vec![tx_input(
                    self.game_outpoint.ok_or_else(|| OrchestratorError("missing game outpoint".to_string()))?,
                    mux_settle_sigscript,
                    0,
                )],
                vec![covenant_output(&routed_settle, 0, self.covenant_id)],
                0,
                Default::default(),
                0,
                vec![],
            );
            let executed_mux_tx = mux_tx.clone();
            execute_input_with_covenants(mux_tx, vec![covenant_utxo(&terminal, self.covenant_id)], 0)
                .map_err(|err| OrchestratorError(format!("mux settle failed: {err}")))?;
            self.transactions.push(executed_mux_tx.clone());
            (routed_settle, TransactionOutpoint { transaction_id: executed_mux_tx.id(), index: 0 }, true)
        };

        let mut next_white = white_state.clone();
        let mut next_black = black_state.clone();
        if next_white.open_games <= 0 || next_black.open_games <= 0 {
            return Err(OrchestratorError("cannot settle players without open games".to_string()));
        }
        next_white.open_games -= 1;
        next_black.open_games -= 1;
        next_white.games += 1;
        next_black.games += 1;

        let (white_actual, black_actual) = match result {
            GameResult::WhiteWin => {
                next_white.wins += 1;
                next_black.losses += 1;
                (1000, 0)
            }
            GameResult::BlackWin => {
                next_white.losses += 1;
                next_black.wins += 1;
                (0, 1000)
            }
            GameResult::Draw => {
                next_white.draws += 1;
                next_black.draws += 1;
                (500, 500)
            }
        };

        let white_old_rating = next_white.rating;
        let black_old_rating = next_black.rating;
        next_white.rating = approx_updated_rating(white_old_rating, black_old_rating, white_actual);
        next_black.rating = approx_updated_rating(black_old_rating, white_old_rating, black_actual);

        let stake = 1_000u64;
        match result {
            GameResult::WhiteWin => {
                next_white.value += stake;
            }
            GameResult::BlackWin => {
                next_black.value += stake;
            }
            GameResult::Draw => {
                let white_share = stake / 2;
                let black_share = stake - white_share;
                next_white.value += white_share;
                next_black.value += black_share;
            }
        }

        let settled_white = self.compile_player(&next_white);
        let settled_black = self.compile_player(&next_black);
        let route_templates = packed_execution_route_templates(&self.fix);
        let settle_sigscript = entry_sigscript(
            &routed_settle,
            "settle",
            vec![Expr::bytes(self.player_prefix.clone()), Expr::bytes(self.player_suffix.clone())],
        );

        let white_placeholder = entry_sigscript(
            &white_contract,
            "delegate_settle",
            vec![
                Expr::int(self.fix.settle.prefix.len() as i64),
                Expr::int(self.fix.settle.suffix.len() as i64),
                hash_expr(self.fix.settle.hash),
                Expr::bytes(route_templates.clone()),
            ],
        );
        let black_placeholder = entry_sigscript(
            &black_contract,
            "delegate_settle",
            vec![
                Expr::int(self.fix.settle.prefix.len() as i64),
                Expr::int(self.fix.settle.suffix.len() as i64),
                hash_expr(self.fix.settle.hash),
                Expr::bytes(route_templates.clone()),
            ],
        );
        let outputs = vec![
            covenant_output_with_value(&settled_white, 0, self.covenant_id, next_white.value),
            covenant_output_with_value(&settled_black, 0, self.covenant_id, next_black.value),
        ];
        let entries = vec![
            covenant_utxo(&routed_settle, self.covenant_id),
            covenant_utxo_with_value(&white_contract, self.covenant_id, white_state.value),
            covenant_utxo_with_value(&black_contract, self.covenant_id, black_state.value),
        ];
        let tx = Transaction::new(
            1,
            vec![
                tx_input(settle_outpoint, settle_sigscript, 0),
                tx_input(white_state.outpoint, white_placeholder, 0),
                tx_input(black_state.outpoint, black_placeholder, 0),
            ],
            outputs,
            0,
            Default::default(),
            0,
            vec![],
        );
        let executed_tx = tx.clone();
        let executed_txid = executed_tx.id();
        execute_input_with_covenants(tx.clone(), entries.clone(), 0)
            .map_err(|err| OrchestratorError(format!("settle leader failed: {err}")))?;
        execute_input_with_covenants(tx.clone(), entries.clone(), 1)
            .map_err(|err| OrchestratorError(format!("settle white delegate failed: {err}")))?;
        execute_input_with_covenants(tx, entries, 2)
            .map_err(|err| OrchestratorError(format!("settle black delegate failed: {err}")))?;
        self.transactions.push(executed_tx);

        self.players.insert(white.name.clone(), next_white);
        self.players.insert(black.name.clone(), next_black);
        self.players.get_mut(&white.name).expect("white tracked").outpoint =
            TransactionOutpoint { transaction_id: executed_txid, index: 0 };
        self.players.get_mut(&black.name).expect("black tracked").outpoint =
            TransactionOutpoint { transaction_id: executed_txid, index: 1 };
        self.game = None;
        self.game_outpoint = None;
        self.active_worker = None;
        self.active_settle = None;
        self.push_message(
            &white.name,
            OffchainMessage {
                from: "arena".to_string(),
                to: white.name.clone(),
                kind: OffchainMessageKind::SettlementNotice { result },
            },
        );
        self.push_message(
            &black.name,
            OffchainMessage {
                from: "arena".to_string(),
                to: black.name.clone(),
                kind: OffchainMessageKind::SettlementNotice { result },
            },
        );
        if include_mux_settle_history {
            self.history.push(SubmittedTx { recipe_name: "mux_settle", consumed: vec![], produced: vec![], signer_names: vec![] });
        }
        self.history.push(SubmittedTx { recipe_name: "settle", consumed: vec![], produced: vec![], signer_names: vec![] });
        Ok(())
    }

    pub fn retire_player(&mut self, player: &SigningPlayer) -> Result<(), OrchestratorError> {
        let state = self.players.get(&player.name).cloned().ok_or_else(|| OrchestratorError("missing player".to_string()))?;
        let contract = self.compile_player(&state);
        let placeholder =
            entry_sigscript(&contract, "retire", vec![Expr::bytes(vec![0u8; 65]), Expr::bytes(player.pubkey_bytes.clone())]);
        let entries = vec![covenant_utxo_with_value(&contract, self.covenant_id, state.value)];
        let mut tx = Transaction::new(1, vec![tx_input(state.outpoint, placeholder, 1)], vec![], 0, Default::default(), 0, vec![]);
        let sig = sign_tx_input_schnorr(&tx, &entries, 0, player);
        tx.inputs[0].signature_script =
            entry_sigscript(&contract, "retire", vec![Expr::bytes(sig), Expr::bytes(player.pubkey_bytes.clone())]);
        let executed_tx = tx.clone();
        execute_input_with_covenants(tx, entries, 0).map_err(|err| OrchestratorError(format!("retire failed: {err}")))?;
        self.transactions.push(executed_tx);
        self.players.remove(&player.name);
        self.history.push(SubmittedTx {
            recipe_name: "retire",
            consumed: vec![],
            produced: vec![],
            signer_names: vec![player.name.clone()],
        });
        Ok(())
    }

    fn planner(&self) -> ChessTxPlanner {
        ChessTxPlanner {
            family: ChessTemplateFamily {
                league: TemplateWitness { prefix: Vec::new(), suffix: Vec::new(), hash: self.league_template },
                player: TemplateWitness {
                    prefix: self.player_prefix.clone(),
                    suffix: self.player_suffix.clone(),
                    hash: self.player_template,
                },
                mux: TemplateWitness {
                    prefix: self.fix.mux.prefix.clone(),
                    suffix: self.fix.mux.suffix.clone(),
                    hash: self.fix.mux.hash,
                },
                settle: TemplateWitness {
                    prefix: self.fix.settle.prefix.clone(),
                    suffix: self.fix.settle.suffix.clone(),
                    hash: self.fix.settle.hash,
                },
                pawn: TemplateWitness {
                    prefix: self.fix.pawn.prefix.clone(),
                    suffix: self.fix.pawn.suffix.clone(),
                    hash: self.fix.pawn.hash,
                },
                knight: TemplateWitness {
                    prefix: self.fix.knight.prefix.clone(),
                    suffix: self.fix.knight.suffix.clone(),
                    hash: self.fix.knight.hash,
                },
                vert: TemplateWitness {
                    prefix: self.fix.vert.prefix.clone(),
                    suffix: self.fix.vert.suffix.clone(),
                    hash: self.fix.vert.hash,
                },
                horiz: TemplateWitness {
                    prefix: self.fix.horiz.prefix.clone(),
                    suffix: self.fix.horiz.suffix.clone(),
                    hash: self.fix.horiz.hash,
                },
                diag: TemplateWitness {
                    prefix: self.fix.diag.prefix.clone(),
                    suffix: self.fix.diag.suffix.clone(),
                    hash: self.fix.diag.hash,
                },
                king: TemplateWitness {
                    prefix: self.fix.king.prefix.clone(),
                    suffix: self.fix.king.suffix.clone(),
                    hash: self.fix.king.hash,
                },
                castle: TemplateWitness {
                    prefix: self.fix.castle.prefix.clone(),
                    suffix: self.fix.castle.suffix.clone(),
                    hash: self.fix.castle.hash,
                },
                castle_challenge: TemplateWitness {
                    prefix: self.fix.castle_challenge.prefix.clone(),
                    suffix: self.fix.castle_challenge.suffix.clone(),
                    hash: self.fix.castle_challenge.hash,
                },
                route_templates: packed_execution_route_templates(&self.fix),
                routes_commitment: routes_commitment(&packed_execution_route_templates(&self.fix)),
            },
        }
    }

    fn push_message(&mut self, recipient: &str, message: OffchainMessage) {
        self.messages.entry(recipient.to_string()).or_default().push(message);
    }

    fn require_registered(&self, player: &SigningPlayer) -> Result<(), OrchestratorError> {
        if player.player_ref.is_none() || player.player_id.is_none() {
            return Err(OrchestratorError(format!("{} is not registered", player.name)));
        }
        Ok(())
    }

    fn player_account(&self, player_ref: Hash) -> Result<PlayerAccount, OrchestratorError> {
        self.players
            .iter()
            .find_map(|(name, state)| {
                (player_ref_hash(state.owner_hash, state.player_id) == player_ref).then_some(PlayerAccount {
                    owner_name: name.clone(),
                    owner_hash: state.owner_hash,
                    player_id: state.player_id,
                    player_ref,
                    value: state.value,
                    open_games: state.open_games,
                    rating: state.rating,
                    games: state.games,
                    wins: state.wins,
                    draws: state.draws,
                    losses: state.losses,
                })
            })
            .ok_or_else(|| OrchestratorError("missing player account".to_string()))
    }

    fn compile_player(&self, state: &PlayerStateData) -> CompiledContract<'static> {
        compile_player_state(
            player_static_source(),
            PlayerStateArgs {
                league_template: &self.league_template,
                player_template: &self.player_template,
                mux_template: &self.fix.mux.hash,
                routes_commitment: &routes_commitment(&packed_execution_route_templates(&self.fix)),
                owner_hash: &state.owner_hash,
                player_id: &state.player_id,
                open_games: state.open_games,
                rating: state.rating,
                games: state.games,
                wins: state.wins,
                draws: state.draws,
                losses: state.losses,
            },
        )
    }

    fn compile_mux(&self, state: &GameStateData) -> CompiledContract<'static> {
        compile_game_state(self.fix.mux.source, &self.fix, state)
    }
}

fn build_execution_fixture() -> ExecutionFixture {
    let mux_source = load_static_contract_source(mux_contract_path());
    let settle_source = load_static_contract_source(settle_contract_path());
    let pawn_source = load_static_contract_source(pawn_contract_path());
    let knight_source = load_static_contract_source(knight_contract_path());
    let vert_source = load_static_contract_source(vert_contract_path());
    let horiz_source = load_static_contract_source(horiz_contract_path());
    let diag_source = load_static_contract_source(diag_contract_path());
    let king_source = load_static_contract_source(king_contract_path());
    let castle_source = load_static_contract_source(castle_contract_path());
    let castle_challenge_source = load_static_contract_source(castle_challenge_contract_path());
    let dummy_board = standard_board();
    let game_ctor = vec![
        Expr::bytes(vec![0x11u8; 32]),
        Expr::bytes(vec![0x33u8; 32 * 9]),
        Expr::bytes(vec![0x21u8; 32]),
        Expr::bytes(vec![0x22u8; 32]),
        Expr::bytes(dummy_board),
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
    ];
    let settle_ctor = vec![Expr::bytes(vec![0x44u8; 32]), Expr::bytes(vec![0x21u8; 32]), Expr::bytes(vec![0x22u8; 32]), Expr::int(0)];
    ExecutionFixture {
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

fn load_static_contract_source(path: &'static str) -> &'static str {
    Box::leak(load_contract_source(path).into_boxed_str())
}

fn player_static_source() -> &'static str {
    load_static_contract_source(player_contract_path())
}

fn template_fixture(source: &'static str, ctor: &[Expr<'_>]) -> TemplateFixture {
    let compiled = compile_contract(source, ctor, CompileOptions::default()).expect("compile template source succeeds");
    let layout = compiled.state_layout;
    let prefix = compiled.script[..layout.start].to_vec();
    let suffix = compiled.script[layout.start + layout.len..].to_vec();
    let hash = Hash::from_bytes(compiled.template_hash());
    TemplateFixture { source, prefix, suffix, hash }
}

fn packed_execution_route_templates(fix: &ExecutionFixture) -> Vec<u8> {
    let player_template = {
        let player_template = compile_player_state(
            player_static_source(),
            PlayerStateArgs {
                league_template: &repeated_hash(0x11),
                player_template: &repeated_hash(0x22),
                mux_template: &fix.mux.hash,
                routes_commitment: &routes_commitment(&vec![0x12u8; 32 * 9]),
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
        Hash::from_bytes(player_template.template_hash())
    };
    let mut out = Vec::with_capacity(32 * 9);
    out.extend_from_slice(&fix.pawn.hash.as_bytes());
    out.extend_from_slice(&fix.knight.hash.as_bytes());
    out.extend_from_slice(&fix.vert.hash.as_bytes());
    out.extend_from_slice(&fix.horiz.hash.as_bytes());
    out.extend_from_slice(&fix.diag.hash.as_bytes());
    out.extend_from_slice(&fix.king.hash.as_bytes());
    out.extend_from_slice(&fix.castle.hash.as_bytes());
    out.extend_from_slice(&fix.castle_challenge.hash.as_bytes());
    out.extend_from_slice(&hash_pair(fix.settle.hash, player_template).as_bytes());
    out
}

fn routes_commitment(route_templates: &[u8]) -> Hash {
    blake2b(route_templates)
}

fn square_idx(x: i64, y: i64) -> i64 {
    y * 8 + x
}

fn worker_actor(worker: WorkerKind) -> &'static str {
    match worker {
        WorkerKind::Pawn => "ChessPawn",
        WorkerKind::Knight => "ChessKnight",
        WorkerKind::Vert => "ChessVert",
        WorkerKind::Horiz => "ChessHoriz",
        WorkerKind::Diag => "ChessDiag",
        WorkerKind::King => "ChessKing",
        WorkerKind::Castle => "ChessCastle",
        WorkerKind::CastleChallenge => "ChessCastleChallengePrep",
    }
}

fn side_from_turn(turn: i64) -> Side {
    if turn == 0 {
        Side::White
    } else {
        Side::Black
    }
}

fn status_from_result(result: GameResult) -> i64 {
    match result {
        GameResult::WhiteWin => 1,
        GameResult::BlackWin => 2,
        GameResult::Draw => 3,
    }
}

fn determine_worker(board: &[u8], mv: MoveSpec) -> Result<WorkerKind, OrchestratorError> {
    if !(0..8).contains(&mv.from_x) || !(0..8).contains(&mv.from_y) || !(0..8).contains(&mv.to_x) || !(0..8).contains(&mv.to_y) {
        return Err(OrchestratorError("move coordinates must stay on board".to_string()));
    }
    let piece = board[square_idx(mv.from_x, mv.from_y) as usize];
    if piece == 0 {
        return Err(OrchestratorError("no piece on source square".to_string()));
    }
    let base = if piece > 8 { piece - 8 } else { piece };
    let dx = mv.to_x - mv.from_x;
    let dy = mv.to_y - mv.from_y;
    match base {
        1 => Ok(WorkerKind::Pawn),
        2 => Ok(WorkerKind::Knight),
        3 => Ok(WorkerKind::Diag),
        4 => {
            if dx == 0 {
                Ok(WorkerKind::Vert)
            } else if dy == 0 {
                Ok(WorkerKind::Horiz)
            } else {
                Err(OrchestratorError("rook move must stay on file or rank".to_string()))
            }
        }
        5 => {
            if dx == 0 {
                Ok(WorkerKind::Vert)
            } else if dy == 0 {
                Ok(WorkerKind::Horiz)
            } else if dx.abs() == dy.abs() {
                Ok(WorkerKind::Diag)
            } else {
                Err(OrchestratorError("queen move must be straight or diagonal".to_string()))
            }
        }
        6 => {
            if dy == 0 && dx.abs() == 2 {
                Ok(WorkerKind::Castle)
            } else {
                Ok(WorkerKind::King)
            }
        }
        _ => Err(OrchestratorError("unknown piece kind".to_string())),
    }
}

fn pending_state_for_move(game: &GameStateData, mv: MoveSpec) -> GameStateData {
    GameStateData {
        white_player: game.white_player,
        black_player: game.black_player,
        board: game.board.clone(),
        turn: game.turn,
        status: game.status,
        move_timeout: game.move_timeout,
        castle_rights: game.castle_rights,
        en_passant_idx: game.en_passant_idx,
        pending_src_idx: square_idx(mv.from_x, mv.from_y),
        pending_dst_idx: square_idx(mv.to_x, mv.to_y),
        pending_promo: mv.promo_piece,
        recent_castle: 0,
        draw_state: game.draw_state,
        move_log: game.move_log.clone(),
    }
}

fn apply_move_to_state(
    game: &GameStateData,
    mv: MoveSpec,
    allow_protocol_nonstandard: bool,
) -> Result<GameStateData, OrchestratorError> {
    let next = if allow_protocol_nonstandard {
        apply_protocol_move(
            &ProtocolState {
                board: game.board.clone(),
                turn: game.turn,
                castle_rights: game.castle_rights,
                en_passant_idx: game.en_passant_idx,
            },
            ProtocolMoveSpec { from_x: mv.from_x, from_y: mv.from_y, to_x: mv.to_x, to_y: mv.to_y, promo_piece: mv.promo_piece },
        )
    } else {
        apply_standard_chess_move(
            &ProtocolState {
                board: game.board.clone(),
                turn: game.turn,
                castle_rights: game.castle_rights,
                en_passant_idx: game.en_passant_idx,
            },
            ProtocolMoveSpec { from_x: mv.from_x, from_y: mv.from_y, to_x: mv.to_x, to_y: mv.to_y, promo_piece: mv.promo_piece },
        )
    }
    .map_err(|err| {
        if allow_protocol_nonstandard {
            OrchestratorError(err.to_string())
        } else {
            OrchestratorError(format!("{err}. Use Force Move to follow the broader protocol path."))
        }
    })?;

    let mut move_log = game.move_log.clone();
    move_log.push(mv.label());
    Ok(GameStateData {
        white_player: game.white_player,
        black_player: game.black_player,
        board: next.board,
        turn: next.turn,
        status: game.status,
        move_timeout: game.move_timeout,
        castle_rights: next.castle_rights,
        en_passant_idx: next.en_passant_idx,
        pending_src_idx: -1,
        pending_dst_idx: -1,
        pending_promo: 0,
        recent_castle: next.recent_castle,
        draw_state: game.draw_state,
        move_log,
    })
}

fn apply_worker_state(
    worker: WorkerKind,
    game: &GameStateData,
    mv: MoveSpec,
    allow_protocol_nonstandard: bool,
) -> Result<GameStateData, OrchestratorError> {
    let mut next = apply_move_to_state(game, mv, allow_protocol_nonstandard)?;
    next.castle_rights = match worker {
        WorkerKind::Pawn | WorkerKind::Knight | WorkerKind::Diag => game.castle_rights,
        WorkerKind::Vert | WorkerKind::Horiz => {
            let mut castle_rights = game.castle_rights;
            let from_idx = square_idx(mv.from_x, mv.from_y);
            let to_idx = square_idx(mv.to_x, mv.to_y);
            if from_idx == 0 || to_idx == 0 {
                castle_rights[1] = 0;
            }
            if from_idx == 7 || to_idx == 7 {
                castle_rights[0] = 0;
            }
            if from_idx == 56 || to_idx == 56 {
                castle_rights[3] = 0;
            }
            if from_idx == 63 || to_idx == 63 {
                castle_rights[2] = 0;
            }
            castle_rights
        }
        WorkerKind::King | WorkerKind::Castle => {
            let mut castle_rights = game.castle_rights;
            let moving_piece = game.board[square_idx(mv.from_x, mv.from_y) as usize];
            let moving_is_black = moving_piece > 8;
            if moving_is_black {
                castle_rights[2] = 0;
                castle_rights[3] = 0;
            } else {
                castle_rights[0] = 0;
                castle_rights[1] = 0;
            }
            castle_rights
        }
        WorkerKind::CastleChallenge => {
            return Err(OrchestratorError("castle challenge apply is not modeled as a direct player move".to_string()));
        }
    };
    if worker == WorkerKind::Castle {
        return Ok(next);
    }

    let target_piece = game.board[square_idx(mv.to_x, mv.to_y) as usize];
    let target_num = i64::from(target_piece);
    let is_draw_claim_mode = game.draw_state < DRAW;
    let effective_turn = if is_draw_claim_mode { 1 - game.turn } else { game.turn };

    let mut next_status = game.status;
    if game.recent_castle != CLEAR {
        next_status = if game.turn == WHITE { WWIN } else { BWIN };
    } else if is_draw_claim_mode {
        if effective_turn == WHITE && target_num == 14 {
            next_status = if game.turn == WHITE { WWIN } else { BWIN };
        }
        if effective_turn == BLACK && target_num == 6 {
            next_status = if game.turn == WHITE { WWIN } else { BWIN };
        }
    } else {
        let moving_piece = game.board[square_idx(mv.from_x, mv.from_y) as usize];
        let moving_is_black = moving_piece > 8;
        if !moving_is_black && target_num == 14 {
            next_status = WWIN;
        }
        if moving_is_black && target_num == 6 {
            next_status = BWIN;
        }
    }

    let mut next_draw_state = game.draw_state;
    if game.draw_state == CLAIMED {
        next_draw_state = DEFENSE;
    } else if game.draw_state == DEFENSE && next_status == LIVE {
        next_status = if game.turn == WHITE { BWIN } else { WWIN };
    }

    next.status = next_status;
    next.draw_state = next_draw_state;
    Ok(next)
}

fn compile_game_state(source: &'static str, fix: &ExecutionFixture, state: &GameStateData) -> CompiledContract<'static> {
    let ctor = vec![
        hash_expr(fix.mux.hash),
        Expr::bytes(packed_execution_route_templates(fix)),
        hash_expr(state.white_player),
        hash_expr(state.black_player),
        Expr::bytes(state.board.clone()),
        Expr::int(state.turn),
        Expr::int(state.status),
        Expr::int(state.move_timeout),
        Expr::bytes(state.castle_rights.to_vec()),
        Expr::int(state.en_passant_idx),
        Expr::int(state.pending_src_idx),
        Expr::int(state.pending_dst_idx),
        Expr::int(state.pending_promo),
        Expr::int(state.recent_castle),
        Expr::int(state.draw_state),
    ];
    compile_contract(source, &ctor, CompileOptions::default()).expect("compile game state")
}

fn compile_player_state(source: &'static str, args: PlayerStateArgs<'_>) -> CompiledContract<'static> {
    let ctor = vec![
        hash_expr(*args.league_template),
        hash_expr(*args.player_template),
        hash_expr(*args.mux_template),
        hash_expr(*args.routes_commitment),
        hash_expr(*args.owner_hash),
        hash_expr(*args.player_id),
        Expr::int(args.open_games),
        Expr::int(args.rating),
        Expr::int(args.games),
        Expr::int(args.wins),
        Expr::int(args.draws),
        Expr::int(args.losses),
    ];
    compile_contract(source, &ctor, CompileOptions::default()).expect("compile player state")
}

fn compile_settle_state(
    source: &'static str,
    player_template: &Hash,
    white_hash: &Hash,
    black_hash: &Hash,
    status: i64,
) -> CompiledContract<'static> {
    let ctor = vec![hash_expr(*player_template), hash_expr(*white_hash), hash_expr(*black_hash), Expr::int(status)];
    compile_contract(source, &ctor, CompileOptions::default()).expect("compile settle state")
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

fn tx_input(previous_outpoint: TransactionOutpoint, signature_script: Vec<u8>, sig_op_count: u8) -> TransactionInput {
    TransactionInput { previous_outpoint, signature_script, sequence: 0, compute_commit: SigopCount(sig_op_count).into() }
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

fn sign_tx_input_schnorr(tx: &Transaction, entries: &[UtxoEntry], input_idx: usize, player: &SigningPlayer) -> Vec<u8> {
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

fn sign_builder_input<T: AsRef<Transaction>>(tx: &MutableTransaction<T>, input_index: usize, keypair: &Keypair) -> Vec<u8> {
    let reused_values = SigHashReusedValuesUnsync::new();
    let sighash = calc_schnorr_signature_hash(&tx.as_verifiable(), input_index, SIG_HASH_ALL, &reused_values);
    let signature = keypair.sign_schnorr(Message::from_digest(sighash.as_bytes()));
    let mut encoded = signature.as_ref().to_vec();
    encoded.push(SIG_HASH_ALL.to_u8());
    encoded
}

fn load_template_family() -> Result<ChessTemplateFamily, OrchestratorError> {
    let mux = compile_template(mux_contract_path(), &mux_constructor_args())?;
    let player = compile_template(player_contract_path(), &player_constructor_args(&mux.hash, &sample_routes_commitment()))?;
    let settle = compile_template(settle_contract_path(), &settle_constructor_args(&player.hash))?;
    let pawn = compile_template(pawn_contract_path(), &worker_constructor_args(&mux.hash))?;
    let knight = compile_template(knight_contract_path(), &worker_constructor_args(&mux.hash))?;
    let vert = compile_template(vert_contract_path(), &worker_constructor_args(&mux.hash))?;
    let horiz = compile_template(horiz_contract_path(), &worker_constructor_args(&mux.hash))?;
    let diag = compile_template(diag_contract_path(), &worker_constructor_args(&mux.hash))?;
    let king = compile_template(king_contract_path(), &worker_constructor_args(&mux.hash))?;
    let castle = compile_template(castle_contract_path(), &worker_constructor_args(&mux.hash))?;
    let castle_challenge = compile_template(castle_challenge_contract_path(), &worker_constructor_args(&mux.hash))?;

    let route_templates =
        packed_route_templates(&player.hash, &settle.hash, [&pawn, &knight, &vert, &horiz, &diag, &king, &castle, &castle_challenge]);
    let routes_commitment = blake2b(&route_templates);
    let league = compile_template(league_contract_path(), &league_constructor_args(&player.hash, &mux.hash, &routes_commitment))?;

    Ok(ChessTemplateFamily {
        league,
        player,
        mux,
        settle,
        pawn,
        knight,
        vert,
        horiz,
        diag,
        king,
        castle,
        castle_challenge,
        route_templates,
        routes_commitment,
    })
}

fn compile_template(path: &str, args: &[Expr<'static>]) -> Result<TemplateWitness, OrchestratorError> {
    let source = load_contract_source(path);
    let compiled = compile_contract(&source, args, CompileOptions::default())
        .map_err(|err| OrchestratorError(format!("failed to compile {path}: {err}")))?;
    let layout = compiled.state_layout;
    let prefix = compiled.script[..layout.start].to_vec();
    let suffix = compiled.script[layout.start + layout.len..].to_vec();
    let hash = Hash::from_bytes(compiled.template_hash());
    Ok(TemplateWitness { prefix, suffix, hash })
}

fn blake2b(data: &[u8]) -> Hash {
    Hash::from_slice(Blake2bParams::new().hash_length(32).to_state().update(data).finalize().as_bytes())
}

fn derive_player_id(nonce: u32, owner_hash: &Hash) -> Hash {
    blake2b([b"LeaguePlayerId".as_slice(), owner_hash.as_bytes().as_slice(), &nonce.to_le_bytes()].concat().as_slice())
}

fn file_char(x: i64) -> char {
    (b'a' + (x as u8)) as char
}

fn rank_char(y: i64) -> char {
    (b'1' + (y as u8)) as char
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
    blake2b(&sample_route_templates())
}

fn worker_constructor_args(mux_template: &Hash) -> Vec<Expr<'static>> {
    vec![
        hash_expr(*mux_template),
        Expr::bytes(sample_route_templates()),
        Expr::bytes(vec![0x21u8; 32]),
        Expr::bytes(vec![0x22u8; 32]),
        Expr::bytes(standard_board()),
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
    vec![
        Expr::bytes(vec![0x11u8; 32]),
        Expr::bytes(sample_route_templates()),
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

fn player_constructor_args(mux_template: &Hash, routes_commitment: &Hash) -> Vec<Expr<'static>> {
    vec![
        Expr::bytes(vec![0x11u8; 32]),
        Expr::bytes(vec![0x22u8; 32]),
        hash_expr(*mux_template),
        hash_expr(*routes_commitment),
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

fn league_constructor_args(player_template: &Hash, mux_template: &Hash, routes_commitment: &Hash) -> Vec<Expr<'static>> {
    vec![
        Expr::bytes(vec![0x11u8; 32]),
        hash_expr(*player_template),
        hash_expr(*mux_template),
        hash_expr(*routes_commitment),
        Expr::int(1200),
        Expr::bytes(vec![0x44u8; 32]),
    ]
}

fn settle_constructor_args(player_template: &Hash) -> Vec<Expr<'static>> {
    vec![hash_expr(*player_template), Expr::bytes(vec![0x21u8; 32]), Expr::bytes(vec![0x22u8; 32]), Expr::int(1)]
}

fn packed_route_templates(player_template: &Hash, settle_template: &Hash, workers: [&TemplateWitness; 8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 * 9);
    for worker in workers {
        out.extend_from_slice(&worker.hash.as_bytes());
    }
    let settle_commitment = hash_pair(*settle_template, *player_template);
    out.extend_from_slice(&settle_commitment.as_bytes());
    out
}

fn approx_expected_score(diff: i64) -> i64 {
    let abs_diff = diff.abs();
    let favored_expected = if abs_diff < 75 {
        500
    } else if abs_diff < 150 {
        600
    } else if abs_diff < 250 {
        700
    } else if abs_diff < 400 {
        820
    } else if abs_diff < 600 {
        910
    } else if abs_diff < 800 {
        970
    } else {
        990
    };

    if diff < 0 {
        favored_expected
    } else if diff > 0 {
        1000 - favored_expected
    } else {
        500
    }
}

fn approx_updated_rating(self_rating: i64, opp_rating: i64, actual_score: i64) -> i64 {
    let expected = approx_expected_score(opp_rating - self_rating);
    self_rating + ((32 * (actual_score - expected)) / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_template_family_with_real_route_commitment() {
        let planner = ChessTxPlanner::load().expect("template family loads");
        assert_eq!(planner.family.route_templates.len(), 32 * 9);
    }

    #[test]
    fn settlement_recipe_uses_unsigned_player_delegates() {
        let planner = ChessTxPlanner::load().expect("template family loads");
        let white_win = planner.settlement_recipe(GameResult::WhiteWin);
        assert_eq!(white_win.settle_step.calls[1].signer, SignerRequirement::None);
        assert_eq!(white_win.settle_step.calls[2].signer, SignerRequirement::None);

        let draw = planner.settlement_recipe(GameResult::Draw);
        assert_eq!(draw.settle_step.calls[1].signer, SignerRequirement::None);
        assert_eq!(draw.settle_step.calls[2].signer, SignerRequirement::None);
    }

    #[test]
    fn local_orchestrators_can_play_settle_and_retire_end_to_end() {
        let planner = ChessTxPlanner::load().expect("planner loads");
        let mut arena = LocalArena::new(planner);
        let mut white = PlayerHandle::new("white", 0x21);
        let mut black = PlayerHandle::new("black", 0x22);

        let reg_white = arena.register_player(&mut white).expect("white registers");
        let reg_black = arena.register_player(&mut black).expect("black registers");
        assert_eq!(reg_white.recipe_name, "register_player");
        assert_eq!(reg_black.recipe_name, "register_player");

        arena.send_game_invite(&white, &black).expect("invite sends");
        let black_mail = arena.drain_inbox(&black);
        assert_eq!(black_mail.len(), 1);
        assert!(matches!(black_mail[0].kind, OffchainMessageKind::GameInvite { .. }));

        let start = arena.start_game(&white, &black).expect("game starts");
        assert_eq!(start.recipe_name, "start_game");
        let game = arena.active_game_snapshot().expect("active game exists");
        assert_eq!(game.turn, Side::White);

        let white_move = arena.submit_move(&white, WorkerKind::Pawn, "e2e4").expect("white move succeeds");
        assert_eq!(white_move.len(), 2);
        let black_move_notice = arena.drain_inbox(&black);
        assert_eq!(black_move_notice.len(), 1);
        assert!(matches!(black_move_notice[0].kind, OffchainMessageKind::MoveNotice { .. }));
        let game = arena.active_game_snapshot().expect("active game exists");
        assert_eq!(game.turn, Side::Black);

        let settle = arena.settle_game(&white, &black, GameResult::WhiteWin).expect("settlement succeeds");
        assert_eq!(settle.len(), 2);
        let white_state = arena.player_account_snapshot(&white).expect("white state exists");
        let black_state = arena.player_account_snapshot(&black).expect("black state exists");
        assert_eq!(white_state.open_games, 0);
        assert_eq!(black_state.open_games, 0);
        assert_eq!(white_state.games, 1);
        assert_eq!(black_state.games, 1);
        assert_eq!(white_state.wins, 1);
        assert_eq!(black_state.losses, 1);
        assert_eq!(white_state.value, 2_000);
        assert_eq!(black_state.value, 1_000);
        assert!(white_state.rating > 1200);
        assert!(black_state.rating < 1200);

        let retire = arena.retire_player(&white).expect("white retires");
        assert_eq!(retire.recipe_name, "retire");
        assert!(arena.player_account_snapshot(&white).is_err());
        assert_eq!(arena.history().len(), 8);
    }

    #[test]
    fn actual_txs_can_play_a_short_game_end_to_end() {
        let shared = TxArena::shared().expect("actual arena builds");
        let mut white = TxOrchestrator::new("white", 0x31, shared.clone());
        let mut black = TxOrchestrator::new("black", 0x32, shared.clone());

        white.register().expect("white register tx passes");
        black.register().expect("black register tx passes");

        white.send_game_invite(&black).expect("white sends invite");
        let invite_mail = black.inbox();
        assert!(matches!(invite_mail.as_slice(), [OffchainMessage { kind: OffchainMessageKind::GameInvite { .. }, .. }]));

        black.accept_game_invite(&white).expect("black accepts invite");
        let accepted_mail = white.inbox();
        assert!(matches!(accepted_mail.as_slice(), [OffchainMessage { kind: OffchainMessageKind::InviteAccepted { .. }, .. }]));

        white.start_game(&black).expect("start game tx passes");
        let started_mail_white = white.inbox();
        let started_mail = black.inbox();
        assert!(matches!(started_mail_white.as_slice(), [OffchainMessage { kind: OffchainMessageKind::GameStarted { .. }, .. }]));
        assert!(matches!(started_mail.as_slice(), [OffchainMessage { kind: OffchainMessageKind::GameStarted { .. }, .. }]));

        white.submit_move(MoveSpec::new(4, 1, 4, 3)).expect("white e2e4 txs pass");
        let move_mail = black.inbox();
        assert!(matches!(
            move_mail.as_slice(),
            [OffchainMessage { kind: OffchainMessageKind::MoveNotice { ref move_label, .. }, .. }] if move_label == "e2e4"
        ));

        black.submit_move(MoveSpec::new(6, 7, 5, 5)).expect("black g8f6 txs pass");
        let reply_mail = white.inbox();
        assert!(matches!(
            reply_mail.as_slice(),
            [OffchainMessage { kind: OffchainMessageKind::MoveNotice { ref move_label, .. }, .. }] if move_label == "g8f6"
        ));

        white.submit_move(MoveSpec::new(5, 0, 2, 3)).expect("white bishop f1c4 txs pass");
        black.inbox();

        black.surrender().expect("black surrender tx passes");
        black.request_settlement(&white, GameResult::WhiteWin).expect("black requests settlement");
        let settlement_request = white.inbox();
        assert!(matches!(
            settlement_request.as_slice(),
            [OffchainMessage { kind: OffchainMessageKind::SettlementRequest { result: GameResult::WhiteWin, .. }, .. }]
        ));

        white.settle(&black, GameResult::WhiteWin).expect("settlement txs pass");
        let settlement_notice = black.inbox();
        assert!(settlement_notice
            .iter()
            .any(|message| { matches!(message.kind, OffchainMessageKind::SettlementNotice { result: GameResult::WhiteWin }) }));

        {
            let arena = shared.borrow();
            let white_state = arena.player_account_snapshot(&white.player).expect("white player remains after settlement");
            let black_state = arena.player_account_snapshot(&black.player).expect("black player remains after settlement");
            assert_eq!(white_state.value, 2_000);
            assert_eq!(black_state.value, 1_000);
        }

        white.retire().expect("retire tx passes");

        let arena = shared.borrow();
        let game = arena.active_game_snapshot();
        assert!(game.is_none());
        let white_state = arena.player_account_snapshot(&white.player);
        let black_state = arena.player_account_snapshot(&black.player).expect("black player remains");
        assert!(white_state.is_err());
        assert_eq!(black_state.open_games, 0);
        assert_eq!(black_state.losses, 1);
        assert_eq!(arena.history().len(), 13);
    }

    #[test]
    fn illegal_move_does_not_leave_the_game_stuck() {
        let shared = TxArena::shared().expect("actual arena builds");
        let mut white = TxOrchestrator::new("white", 0x51, shared.clone());
        let mut black = TxOrchestrator::new("black", 0x52, shared.clone());

        white.register().expect("white register tx passes");
        black.register().expect("black register tx passes");
        white.start_game(&black).expect("start game tx passes");

        let (history_before, txs_before, game_before) = {
            let arena = shared.borrow();
            (arena.history().len(), arena.transactions().len(), arena.active_game_snapshot().expect("active game exists"))
        };

        let err = white.submit_move(MoveSpec::new(4, 1, 4, 4)).expect_err("illegal e2e5 should fail");
        assert!(err.to_string().contains("Use Force Move"), "unexpected error: {err}");

        {
            let arena = shared.borrow();
            let game_after = arena.active_game_snapshot().expect("active game still exists");
            assert_eq!(arena.history().len(), history_before);
            assert_eq!(arena.transactions().len(), txs_before);
            assert_eq!(game_after.turn, game_before.turn);
            assert_eq!(game_after.status, game_before.status);
            assert_eq!(game_after.board, game_before.board);
        }

        white.submit_move(MoveSpec::new(4, 1, 4, 3)).expect("legal e2e4 should still pass");
        {
            let arena = shared.borrow();
            let game_after = arena.active_game_snapshot().expect("active game exists");
            assert_eq!(game_after.turn, Side::Black);
            assert_eq!(arena.history().len(), history_before + 2);
            assert_eq!(arena.transactions().len(), txs_before + 2);
        }
    }

    #[test]
    fn forced_illegal_move_can_be_timed_out_and_settled() {
        let shared = TxArena::shared().expect("actual arena builds");
        let mut white = TxOrchestrator::new("white", 0x61, shared.clone());
        let mut black = TxOrchestrator::new("black", 0x62, shared.clone());

        white.register().expect("white register tx passes");
        black.register().expect("black register tx passes");
        white.start_game(&black).expect("start game tx passes");

        let forced = white.force_move(MoveSpec::new(4, 1, 4, 4)).expect("forced illegal move should route");
        assert_eq!(forced.len(), 1);
        assert_eq!(forced[0].recipe_name, "route");

        let notice = black.inbox();
        assert!(notice.iter().any(|message| {
            matches!(message.kind, OffchainMessageKind::TimeoutClaimAvailable { result: GameResult::BlackWin, .. })
        }));

        {
            let arena = shared.borrow();
            let game = arena.active_game_snapshot().expect("worker transit should be visible");
            assert!(game.phase.starts_with("worker:"));
        }

        black.claim_timeout().expect("black claims timeout");
        let settlement_request = white.inbox();
        assert!(settlement_request
            .iter()
            .any(|message| { matches!(message.kind, OffchainMessageKind::SettlementRequest { result: GameResult::BlackWin, .. }) }));

        white.settle(&black, GameResult::BlackWin).expect("timeout win settles");
        {
            let arena = shared.borrow();
            let white_state = arena.player_account_snapshot(&white.player).expect("white remains");
            let black_state = arena.player_account_snapshot(&black.player).expect("black remains");
            assert_eq!(white_state.losses, 1);
            assert_eq!(black_state.wins, 1);
            assert_eq!(white_state.open_games, 0);
            assert_eq!(black_state.open_games, 0);
            assert_eq!(arena.active_game_snapshot(), None);
        }
    }

    #[test]
    fn actual_txs_can_capture_the_enemy_king() {
        let shared = TxArena::shared().expect("actual arena builds");
        let mut white = TxOrchestrator::new("white", 0x71, shared.clone());
        let mut black = TxOrchestrator::new("black", 0x72, shared.clone());

        white.register().expect("white register tx passes");
        black.register().expect("black register tx passes");
        white.start_game(&black).expect("start game tx passes");

        {
            let mut arena = shared.borrow_mut();
            let game = arena.game.as_mut().expect("active game exists");
            let mut board = vec![0u8; 64];
            board[0] = 0x05;
            board[24] = 0x0e;
            game.board = board;
            game.turn = Side::White as i64;
            game.status = LIVE;
            game.castle_rights = [1, 1, 1, 1];
            game.en_passant_idx = -1;
            game.pending_src_idx = -1;
            game.pending_dst_idx = -1;
            game.pending_promo = 0;
            game.recent_castle = CLEAR;
            game.draw_state = DRAW;
            game.move_log.clear();
        }

        let err = white.submit_move(MoveSpec::new(0, 0, 0, 3)).expect_err("standard submit should reject king capture");
        assert!(err.0.contains("Force Move"), "unexpected error: {}", err.0);

        white.force_move(MoveSpec::new(0, 0, 0, 3)).expect("forced king capture txs pass");

        let arena = shared.borrow();
        let game = arena.active_game_snapshot().expect("active game remains until settlement");
        assert_eq!(game.status, WWIN);
        assert_eq!(game.turn, Side::Black);
        assert_eq!(game.board[24], 0x05);
        assert_eq!(game.board[0], 0x00);
    }

    #[test]
    fn opponent_can_reply_normally_after_castle() {
        let shared = TxArena::shared().expect("actual arena builds");
        let mut white = TxOrchestrator::new("white", 0x81, shared.clone());
        let mut black = TxOrchestrator::new("black", 0x82, shared.clone());

        white.register().expect("white register tx passes");
        black.register().expect("black register tx passes");
        white.start_game(&black).expect("start game tx passes");

        white.submit_move(MoveSpec::new(4, 1, 4, 3)).expect("white e2e4 tx passes");
        black.submit_move(MoveSpec::new(4, 6, 4, 4)).expect("black e7e5 tx passes");
        white.submit_move(MoveSpec::new(6, 0, 5, 2)).expect("white g1f3 tx passes");
        black.submit_move(MoveSpec::new(1, 7, 2, 5)).expect("black b8c6 tx passes");
        white.submit_move(MoveSpec::new(5, 0, 4, 1)).expect("white f1e2 tx passes");
        black.submit_move(MoveSpec::new(6, 7, 5, 5)).expect("black g8f6 tx passes");
        white.submit_move(MoveSpec::new(4, 0, 6, 0)).expect("white castles kingside");

        {
            let arena = shared.borrow();
            let game = arena.active_game_snapshot().expect("active game exists after castle");
            assert_eq!(game.turn, Side::Black);
            assert_eq!(game.board[4], 0x00);
            assert_eq!(game.board[5], 0x04);
            assert_eq!(game.board[6], 0x06);
            assert_eq!(game.board[7], 0x00);
        }

        black.submit_move(MoveSpec::new(0, 6, 0, 5)).expect("black a7a6 reply should pass after castle");

        let arena = shared.borrow();
        let game = arena.active_game_snapshot().expect("active game exists after reply");
        assert_eq!(game.turn, Side::White);
        assert_eq!(game.board[48], 0x00);
        assert_eq!(game.board[40], 0x09);
    }
}

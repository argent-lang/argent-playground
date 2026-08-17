use std::collections::BTreeMap;

use argent_artifact::Artifact;
use argent_runtime::stdlib::core::invocation_uid;
use blake2b_simd::Params as Blake2bParams;
use kaspa_consensus_core::tx::Transaction;
use kaspa_consensus_core::Hash;

use crate::orchestrator::WorkerKind;
use crate::protocol_move::{apply_protocol_move, ProtocolMoveSpec, ProtocolState, OFFBOARD};
use crate::txdecode::{decode_p2sh_call, ContractTemplate, DecodeError, DecodeValue, DecodedCall, DecodedObject};

const WHITE: i64 = 0;
const BLACK: i64 = 1;
const LIVE: i64 = 0;
const WWIN: i64 = 1;
const BWIN: i64 = 2;
const DRAW: i64 = 3;
const CLEAR: i64 = 0;
const OFFER: i64 = 1;
const CLAIM: i64 = 2;
const SURRENDER: i64 = 3;
const PREP: i64 = 7;
const MUX: i64 = 8;
const CLAIMED: i64 = 1;
const DEFENSE: i64 = 2;
const NORMAL: i64 = 3;
const WOFFER: i64 = 4;

fn player_ref(owner: Hash, player_id: Hash) -> Hash {
    hash_pair(owner, player_id)
}

fn hash_pair(left: Hash, right: Hash) -> Hash {
    let left = left.as_bytes();
    let right = right.as_bytes();
    blake2b(&[left.as_slice(), right.as_slice()].concat())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeagueState {
    pub admin: Hash,
    pub league_template: Hash,
    pub player_template: Hash,
    pub mux_template: Hash,
    pub routes_commitment: Hash,
    pub base_rating: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerState {
    pub league_template: Hash,
    pub player_template: Hash,
    pub mux_template: Hash,
    pub routes_commitment: Hash,
    pub owner: Hash,
    pub player_id: Hash,
    pub open_games: i64,
    pub rating: i64,
    pub games: i64,
    pub wins: i64,
    pub draws: i64,
    pub losses: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameState {
    pub mux_template: Hash,
    pub player_template: Hash,
    pub route_templates: Vec<u8>,
    pub white_player: Hash,
    pub black_player: Hash,
    pub board: Vec<u8>,
    pub turn: i64,
    pub status: i64,
    pub move_timeout: i64,
    pub castle_rights: Vec<u8>,
    pub en_passant_idx: i64,
    pub pending_src_idx: i64,
    pub pending_dst_idx: i64,
    pub pending_promo: i64,
    pub recent_castle: i64,
    pub draw_state: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleState {
    pub player_template: Hash,
    pub white_player: Hash,
    pub black_player: Hash,
    pub status: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChessState {
    League(LeagueState),
    Player(PlayerState),
    Game(GameState),
    Settle(SettleState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChessInputKind {
    League,
    Player,
    Mux,
    Settle,
    Worker(WorkerKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedOutput {
    pub output_index: usize,
    pub state: ChessState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedInput {
    pub input_index: usize,
    pub kind: ChessInputKind,
    pub function: String,
    pub input_state: ChessState,
    pub outputs: Vec<ObservedOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObservedTx {
    pub inputs: Vec<ObservedInput>,
    pub events: Vec<ChessEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChessEvent {
    PlayerRegistered { lane_output_index: usize, player_output_index: usize, player_ref: Hash, player_id: Hash, rating: i64 },
    LeagueRebalanced { output_index: usize },
    LeagueForked { left_output_index: usize, right_output_index: usize },
    GameStarted { white_player: Hash, black_player: Hash, move_timeout: i64, game_output_index: usize },
    PlayerRebalanced { output_index: usize, player_ref: Hash },
    PlayerRetired { player_ref: Hash },
    MoveRouted { selector: i64, termination_action: i64, output_index: usize },
    WorkerApplied { worker: WorkerKind, status: i64, next_turn: i64, output_index: usize },
    TimeoutRoutedToSettle { source: ChessInputKind, status: i64, output_index: usize },
    SettleCreated { status: i64, output_index: usize },
    SettlementApplied { status: i64, white_output_index: usize, black_output_index: usize },
}

#[derive(Debug, Clone)]
struct ObserverTemplates {
    contracts: Vec<(ChessInputKind, ContractTemplate)>,
}

#[derive(Debug, Clone)]
struct DecodedInput {
    index: usize,
    kind: ChessInputKind,
    state: ChessState,
    call: DecodedCall,
}

#[derive(Debug, Clone)]
pub struct ChessEventEmitter {
    templates: ObserverTemplates,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct ObserverError(pub String);

impl From<DecodeError> for ObserverError {
    fn from(value: DecodeError) -> Self {
        Self(value.to_string())
    }
}

impl ChessEventEmitter {
    pub fn load() -> Result<Self, ObserverError> {
        Ok(Self { templates: load_templates()? })
    }

    pub fn observe_tx(&self, tx: &Transaction, covenant_id: Hash) -> Result<ObservedTx, ObserverError> {
        let decoded_inputs = self.decode_inputs(tx)?;
        let outputs_by_input = authored_outputs_by_input(tx, covenant_id);

        let mut observed = Vec::new();
        let mut events = Vec::new();
        for decoded in &decoded_inputs {
            let outputs = match (&decoded.kind, decoded.call.function.as_str()) {
                (ChessInputKind::League, "register_player") => {
                    let state = expect_league(&decoded.state)?;
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 2 {
                        return Err(ObserverError(format!(
                            "league register expected 2 authored outputs for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }
                    let owner_pk = hash_arg(&decoded.call, "owner_pk")?;
                    let owner = blake2b(&owner_pk.as_bytes());
                    let source_outpoint = tx.inputs[decoded.index].previous_outpoint;
                    let player_id = invocation_uid(&source_outpoint, b"LeaguePlayerId")
                        .map_err(|err| ObserverError(format!("derive player ID: {err}")))?;
                    let player = PlayerState {
                        league_template: state.league_template,
                        player_template: state.player_template,
                        mux_template: state.mux_template,
                        routes_commitment: state.routes_commitment,
                        owner,
                        player_id,
                        open_games: 0,
                        rating: state.base_rating,
                        games: 0,
                        wins: 0,
                        draws: 0,
                        losses: 0,
                    };
                    let player_ref = player_ref(player.owner, player.player_id);
                    let outputs = vec![
                        ObservedOutput { output_index: output_indexes[0], state: ChessState::League(state.clone()) },
                        ObservedOutput { output_index: output_indexes[1], state: ChessState::Player(player) },
                    ];
                    events.push(ChessEvent::PlayerRegistered {
                        lane_output_index: output_indexes[0],
                        player_output_index: output_indexes[1],
                        player_ref,
                        player_id,
                        rating: state.base_rating,
                    });
                    outputs
                }
                (ChessInputKind::League, "rebalance") => {
                    let outputs =
                        same_output_state(decoded, &outputs_by_input, ChessState::League(expect_league(&decoded.state)?.clone()))?;
                    events.push(ChessEvent::LeagueRebalanced { output_index: outputs[0].output_index });
                    outputs
                }
                (ChessInputKind::League, "fork") => {
                    let state = ChessState::League(expect_league(&decoded.state)?.clone());
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 2 {
                        return Err(ObserverError(format!(
                            "league fork expected 2 authored outputs for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }
                    let outputs = vec![
                        ObservedOutput { output_index: output_indexes[0], state: state.clone() },
                        ObservedOutput { output_index: output_indexes[1], state },
                    ];
                    events.push(ChessEvent::LeagueForked {
                        left_output_index: output_indexes[0],
                        right_output_index: output_indexes[1],
                    });
                    outputs
                }
                (ChessInputKind::Player, "start_game") => {
                    let self_state = expect_player(&decoded.state)?;
                    let other = decoded_inputs
                        .iter()
                        .find(|candidate| {
                            candidate.index != decoded.index
                                && candidate.kind == ChessInputKind::Player
                                && candidate.call.function == "delegate_start_game"
                        })
                        .ok_or_else(|| ObserverError("start_game could not find delegate_start_game peer".to_string()))?;
                    let other_state = expect_player(&other.state)?;
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 3 {
                        return Err(ObserverError(format!(
                            "player start_game expected 3 authored outputs for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }

                    let self_side = byte_arg(&decoded.call, "self_side")?;
                    let route_templates = bytes_arg_any(&decoded.call, &["route_templates", "gen__mux_routes"])?;
                    let move_timeout = int_arg(&decoded.call, "move_timeout")?;

                    let self_ref = player_ref(self_state.owner, self_state.player_id);
                    let other_ref = player_ref(other_state.owner, other_state.player_id);
                    let (white_player, black_player) = if self_side == BLACK { (other_ref, self_ref) } else { (self_ref, other_ref) };

                    let next_self = PlayerState { open_games: self_state.open_games + 1, ..self_state.clone() };
                    let next_other = PlayerState { open_games: other_state.open_games + 1, ..other_state.clone() };
                    let opening_game = GameState {
                        mux_template: self_state.mux_template,
                        player_template: self_state.player_template,
                        route_templates,
                        white_player,
                        black_player,
                        board: opening_board(),
                        turn: WHITE,
                        status: LIVE,
                        move_timeout,
                        castle_rights: vec![1, 1, 1, 1],
                        en_passant_idx: OFFBOARD,
                        pending_src_idx: OFFBOARD,
                        pending_dst_idx: OFFBOARD,
                        pending_promo: CLEAR,
                        recent_castle: CLEAR,
                        draw_state: NORMAL,
                    };
                    let outputs = vec![
                        ObservedOutput { output_index: output_indexes[0], state: ChessState::Player(next_self) },
                        ObservedOutput { output_index: output_indexes[1], state: ChessState::Player(next_other) },
                        ObservedOutput { output_index: output_indexes[2], state: ChessState::Game(opening_game) },
                    ];
                    events.push(ChessEvent::GameStarted {
                        white_player,
                        black_player,
                        move_timeout,
                        game_output_index: output_indexes[2],
                    });
                    outputs
                }
                (ChessInputKind::Player, "delegate_start_game") => Vec::new(),
                (ChessInputKind::Player, "delegate_settle") => Vec::new(),
                (ChessInputKind::Player, "rebalance") => {
                    let player = expect_player(&decoded.state)?;
                    let outputs = same_output_state(decoded, &outputs_by_input, ChessState::Player(player.clone()))?;
                    events.push(ChessEvent::PlayerRebalanced {
                        output_index: outputs[0].output_index,
                        player_ref: player_ref(player.owner, player.player_id),
                    });
                    outputs
                }
                (ChessInputKind::Player, "retire") => {
                    let player = expect_player(&decoded.state)?;
                    events.push(ChessEvent::PlayerRetired { player_ref: player_ref(player.owner, player.player_id) });
                    Vec::new()
                }
                (ChessInputKind::Mux, "route") => {
                    let state = expect_game(&decoded.state)?;
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 1 {
                        return Err(ObserverError(format!(
                            "mux route expected 1 authored output for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }
                    let selector = int_arg_any(&decoded.call, &["selector", "target"])?;
                    let from_x = int_arg(&decoded.call, "from_x")?;
                    let from_y = int_arg(&decoded.call, "from_y")?;
                    let to_x = int_arg(&decoded.call, "to_x")?;
                    let to_y = int_arg(&decoded.call, "to_y")?;
                    let promo_piece = byte_arg(&decoded.call, "promo_piece")?;
                    let termination_action = byte_arg(&decoded.call, "termination_action")?;
                    let next = route_game_state(state, selector, from_x, from_y, to_x, to_y, promo_piece, termination_action)?;
                    let next_state = ChessState::Game(next);
                    let outputs = vec![ObservedOutput { output_index: output_indexes[0], state: next_state }];
                    events.push(ChessEvent::MoveRouted { selector, termination_action, output_index: output_indexes[0] });
                    outputs
                }
                (ChessInputKind::Mux, "terminate") => {
                    let state = expect_game(&decoded.state)?;
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 1 {
                        return Err(ObserverError(format!(
                            "mux terminate expected 1 authored output for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }
                    let termination_action = byte_arg(&decoded.call, "termination_action")?;
                    let next = route_game_state(state, MUX, -1, -1, -1, -1, 0, termination_action)?;
                    let outputs = vec![ObservedOutput { output_index: output_indexes[0], state: ChessState::Game(next) }];
                    events.push(ChessEvent::MoveRouted { selector: MUX, termination_action, output_index: output_indexes[0] });
                    outputs
                }
                (ChessInputKind::Mux, "timeout") => {
                    let state = expect_game(&decoded.state)?;
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 1 {
                        return Err(ObserverError(format!(
                            "mux timeout expected 1 authored output for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }
                    let player_template = optional_hash_arg(&decoded.call, "player_template")?.unwrap_or(state.player_template);
                    let next = SettleState {
                        player_template,
                        white_player: state.white_player,
                        black_player: state.black_player,
                        status: timeout_status(state.turn, state.draw_state),
                    };
                    let outputs = vec![ObservedOutput { output_index: output_indexes[0], state: ChessState::Settle(next.clone()) }];
                    events.push(ChessEvent::TimeoutRoutedToSettle {
                        source: ChessInputKind::Mux,
                        status: next.status,
                        output_index: output_indexes[0],
                    });
                    outputs
                }
                (ChessInputKind::Mux, "settle") => {
                    let state = expect_game(&decoded.state)?;
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 1 {
                        return Err(ObserverError(format!(
                            "mux settle expected 1 authored output for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }
                    let player_template = optional_hash_arg(&decoded.call, "player_template")?.unwrap_or(state.player_template);
                    let next = SettleState {
                        player_template,
                        white_player: state.white_player,
                        black_player: state.black_player,
                        status: state.status,
                    };
                    let outputs = vec![ObservedOutput { output_index: output_indexes[0], state: ChessState::Settle(next.clone()) }];
                    events.push(ChessEvent::SettleCreated { status: next.status, output_index: output_indexes[0] });
                    outputs
                }
                (ChessInputKind::Worker(worker), "apply") => {
                    let state = expect_game(&decoded.state)?;
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 1 {
                        return Err(ObserverError(format!(
                            "worker apply expected 1 authored output for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }
                    let next = apply_worker_state(*worker, state)?;
                    let outputs = vec![ObservedOutput { output_index: output_indexes[0], state: ChessState::Game(next.clone()) }];
                    events.push(ChessEvent::WorkerApplied {
                        worker: *worker,
                        status: next.status,
                        next_turn: next.turn,
                        output_index: output_indexes[0],
                    });
                    outputs
                }
                (ChessInputKind::Worker(worker), "timeout") => {
                    let state = expect_game(&decoded.state)?;
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 1 {
                        return Err(ObserverError(format!(
                            "worker timeout expected 1 authored output for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }
                    let player_template = optional_hash_arg(&decoded.call, "player_template")?.unwrap_or(state.player_template);
                    let next = SettleState {
                        player_template,
                        white_player: state.white_player,
                        black_player: state.black_player,
                        status: timeout_status(state.turn, state.draw_state),
                    };
                    let outputs = vec![ObservedOutput { output_index: output_indexes[0], state: ChessState::Settle(next.clone()) }];
                    events.push(ChessEvent::TimeoutRoutedToSettle {
                        source: ChessInputKind::Worker(*worker),
                        status: next.status,
                        output_index: output_indexes[0],
                    });
                    outputs
                }
                (ChessInputKind::Settle, "settle") => {
                    let state = expect_settle(&decoded.state)?;
                    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
                    if output_indexes.len() != 2 {
                        return Err(ObserverError(format!(
                            "settle expected 2 authored outputs for input {}, got {}",
                            decoded.index,
                            output_indexes.len()
                        )));
                    }
                    let white_in = decoded_inputs
                        .iter()
                        .filter_map(|input| match &input.state {
                            ChessState::Player(player) if player_ref(player.owner, player.player_id) == state.white_player => {
                                Some(player.clone())
                            }
                            _ => None,
                        })
                        .next()
                        .ok_or_else(|| ObserverError("settle could not locate white player input".to_string()))?;
                    let black_in = decoded_inputs
                        .iter()
                        .filter_map(|input| match &input.state {
                            ChessState::Player(player) if player_ref(player.owner, player.player_id) == state.black_player => {
                                Some(player.clone())
                            }
                            _ => None,
                        })
                        .next()
                        .ok_or_else(|| ObserverError("settle could not locate black player input".to_string()))?;

                    let (next_white, next_black) = settle_players(tx, decoded.index, state, &white_in, &black_in)?;
                    let outputs = vec![
                        ObservedOutput { output_index: output_indexes[0], state: ChessState::Player(next_white) },
                        ObservedOutput { output_index: output_indexes[1], state: ChessState::Player(next_black) },
                    ];
                    events.push(ChessEvent::SettlementApplied {
                        status: state.status,
                        white_output_index: output_indexes[0],
                        black_output_index: output_indexes[1],
                    });
                    outputs
                }
                _ => {
                    return Err(ObserverError(format!("unsupported observer path for {:?}.{}", decoded.kind, decoded.call.function)));
                }
            };

            observed.push(ObservedInput {
                input_index: decoded.index,
                kind: decoded.kind,
                function: decoded.call.function.clone(),
                input_state: decoded.state.clone(),
                outputs,
            });
        }

        Ok(ObservedTx { inputs: observed, events })
    }

    fn decode_inputs(&self, tx: &Transaction) -> Result<Vec<DecodedInput>, ObserverError> {
        let mut decoded = Vec::new();
        for (index, input) in tx.inputs.iter().enumerate() {
            let p2sh = match decode_p2sh_call(&input.signature_script) {
                Ok(call) => call,
                Err(_) => continue,
            };
            let (kind, template) = self
                .match_template(&p2sh.redeem_script)
                .ok_or_else(|| ObserverError(format!("no chess template matched redeem script for input {index}")))?;
            let state = template.decode_state(&p2sh.redeem_script)?;
            let call = template.decode_call(&p2sh.stack_items)?;
            let typed_state = match kind {
                ChessInputKind::League => ChessState::League(league_from_decoded(&state)?),
                ChessInputKind::Player => ChessState::Player(player_from_decoded(&state)?),
                ChessInputKind::Mux | ChessInputKind::Worker(_) => ChessState::Game(game_from_decoded(&state)?),
                ChessInputKind::Settle => ChessState::Settle(settle_from_decoded(&state)?),
            };
            decoded.push(DecodedInput { index, kind, state: typed_state, call });
        }
        Ok(decoded)
    }

    fn match_template(&self, redeem_script: &[u8]) -> Option<(ChessInputKind, &ContractTemplate)> {
        self.templates
            .contracts
            .iter()
            .map(|(kind, template)| (*kind, template))
            .find(|(_, template)| template.matches_redeem_script(redeem_script))
    }
}

fn authored_outputs_by_input(tx: &Transaction, covenant_id: Hash) -> BTreeMap<usize, Vec<usize>> {
    let mut out = BTreeMap::<usize, Vec<usize>>::new();
    for (index, output) in tx.outputs.iter().enumerate() {
        let Some(binding) = &output.covenant else {
            continue;
        };
        if binding.covenant_id == covenant_id {
            out.entry(binding.authorizing_input as usize).or_default().push(index);
        }
    }
    out
}

fn same_output_state(
    decoded: &DecodedInput,
    outputs_by_input: &BTreeMap<usize, Vec<usize>>,
    state: ChessState,
) -> Result<Vec<ObservedOutput>, ObserverError> {
    let output_indexes = outputs_by_input.get(&decoded.index).cloned().unwrap_or_default();
    if output_indexes.len() != 1 {
        return Err(ObserverError(format!(
            "{} expected 1 authored output for input {}, got {}",
            decoded.call.function,
            decoded.index,
            output_indexes.len()
        )));
    }
    Ok(vec![ObservedOutput { output_index: output_indexes[0], state }])
}

fn expect_league(state: &ChessState) -> Result<&LeagueState, ObserverError> {
    match state {
        ChessState::League(value) => Ok(value),
        _ => Err(ObserverError("expected league state".to_string())),
    }
}

fn expect_player(state: &ChessState) -> Result<&PlayerState, ObserverError> {
    match state {
        ChessState::Player(value) => Ok(value),
        _ => Err(ObserverError("expected player state".to_string())),
    }
}

fn expect_game(state: &ChessState) -> Result<&GameState, ObserverError> {
    match state {
        ChessState::Game(value) => Ok(value),
        _ => Err(ObserverError("expected game state".to_string())),
    }
}

fn expect_settle(state: &ChessState) -> Result<&SettleState, ObserverError> {
    match state {
        ChessState::Settle(value) => Ok(value),
        _ => Err(ObserverError("expected settle state".to_string())),
    }
}

fn int_arg(call: &DecodedCall, name: &str) -> Result<i64, ObserverError> {
    match call.args.iter().find(|arg| arg.name == name).map(|arg| &arg.value) {
        Some(DecodeValue::Int(value)) => Ok(*value),
        _ => Err(ObserverError(format!("missing int argument {name}"))),
    }
}

fn byte_arg(call: &DecodedCall, name: &str) -> Result<i64, ObserverError> {
    match call.args.iter().find(|arg| arg.name == name).map(|arg| &arg.value) {
        Some(DecodeValue::Byte(value)) => Ok(i64::from(*value)),
        _ => Err(ObserverError(format!("missing byte argument {name}"))),
    }
}

fn int_arg_any(call: &DecodedCall, names: &[&str]) -> Result<i64, ObserverError> {
    names
        .iter()
        .find_map(|name| match call.args.iter().find(|arg| arg.name == *name).map(|arg| &arg.value) {
            Some(DecodeValue::Int(value)) => Some(*value),
            _ => None,
        })
        .ok_or_else(|| ObserverError(format!("missing int argument {}", names.join(" or "))))
}

fn bytes_arg(call: &DecodedCall, name: &str) -> Result<Vec<u8>, ObserverError> {
    match call.args.iter().find(|arg| arg.name == name).map(|arg| &arg.value) {
        Some(DecodeValue::Bytes(value)) => Ok(value.clone()),
        _ => Err(ObserverError(format!("missing byte argument {name}"))),
    }
}

fn bytes_arg_any(call: &DecodedCall, names: &[&str]) -> Result<Vec<u8>, ObserverError> {
    names
        .iter()
        .find_map(|name| match call.args.iter().find(|arg| arg.name == *name).map(|arg| &arg.value) {
            Some(DecodeValue::Bytes(value)) => Some(value.clone()),
            _ => None,
        })
        .ok_or_else(|| ObserverError(format!("missing byte argument {}", names.join(" or "))))
}

fn hash_arg(call: &DecodedCall, name: &str) -> Result<Hash, ObserverError> {
    let bytes = bytes_arg(call, name)?;
    Hash::try_from_slice(&bytes).map_err(|_| ObserverError(format!("argument {name} is not 32 bytes")))
}

fn optional_hash_arg(call: &DecodedCall, name: &str) -> Result<Option<Hash>, ObserverError> {
    let Some(arg) = call.args.iter().find(|arg| arg.name == name) else {
        return Ok(None);
    };
    let DecodeValue::Bytes(bytes) = &arg.value else {
        return Err(ObserverError(format!("argument {name} is not bytes")));
    };
    Hash::try_from_slice(bytes).map(Some).map_err(|_| ObserverError(format!("argument {name} is not 32 bytes")))
}

fn bytes_field(object: &DecodedObject, name: &str) -> Result<Vec<u8>, ObserverError> {
    match object.get(name) {
        Some(DecodeValue::Bytes(value)) => Ok(value.clone()),
        _ => Err(ObserverError(format!("missing bytes field {name}"))),
    }
}

fn bytes_field_any(object: &DecodedObject, names: &[&str]) -> Result<Vec<u8>, ObserverError> {
    names
        .iter()
        .find_map(|name| match object.get(name) {
            Some(DecodeValue::Bytes(value)) => Some(value.clone()),
            _ => None,
        })
        .ok_or_else(|| ObserverError(format!("missing bytes field {}", names.join(" or "))))
}

fn hash_field(object: &DecodedObject, name: &str) -> Result<Hash, ObserverError> {
    let bytes = bytes_field(object, name)?;
    Hash::try_from_slice(&bytes).map_err(|_| ObserverError(format!("field {name} is not 32 bytes")))
}

fn hash_field_any(object: &DecodedObject, names: &[&str]) -> Result<Hash, ObserverError> {
    let bytes = bytes_field_any(object, names)?;
    Hash::try_from_slice(&bytes).map_err(|_| ObserverError(format!("field {} is not 32 bytes", names.join(" or "))))
}

fn optional_hash_field(object: &DecodedObject, name: &str) -> Result<Option<Hash>, ObserverError> {
    let Some(value) = object.get(name) else {
        return Ok(None);
    };
    let DecodeValue::Bytes(bytes) = value else {
        return Err(ObserverError(format!("field {name} is not bytes")));
    };
    Hash::try_from_slice(bytes).map(Some).map_err(|_| ObserverError(format!("field {name} is not 32 bytes")))
}

fn int_field(object: &DecodedObject, name: &str) -> Result<i64, ObserverError> {
    match object.get(name) {
        Some(DecodeValue::Int(value)) => Ok(*value),
        _ => Err(ObserverError(format!("missing int field {name}"))),
    }
}

fn byte_field(object: &DecodedObject, name: &str) -> Result<i64, ObserverError> {
    match object.get(name) {
        Some(DecodeValue::Byte(value)) => Ok(i64::from(*value)),
        _ => Err(ObserverError(format!("missing byte field {name}"))),
    }
}

fn league_from_decoded(object: &DecodedObject) -> Result<LeagueState, ObserverError> {
    Ok(LeagueState {
        admin: hash_field(object, "admin")?,
        league_template: optional_hash_field(object, "league_template")?.unwrap_or_default(),
        player_template: hash_field_any(object, &["player_template", "gen__player_template"])?,
        mux_template: hash_field_any(object, &["mux_template", "gen__mux_template"])?,
        routes_commitment: hash_field_any(object, &["routes_commitment", "gen__mux_routes_digest"])?,
        base_rating: int_field(object, "base_rating")?,
    })
}

fn player_from_decoded(object: &DecodedObject) -> Result<PlayerState, ObserverError> {
    Ok(PlayerState {
        league_template: optional_hash_field(object, "league_template")?.unwrap_or_default(),
        player_template: hash_field_any(object, &["player_template", "gen__player_template"])?,
        mux_template: hash_field_any(object, &["mux_template", "gen__mux_template"])?,
        routes_commitment: hash_field_any(object, &["routes_commitment", "gen__mux_routes_digest"])?,
        owner: hash_field(object, "owner")?,
        player_id: hash_field(object, "player_id")?,
        open_games: int_field(object, "open_games")?,
        rating: int_field(object, "rating")?,
        games: int_field(object, "games")?,
        wins: int_field(object, "wins")?,
        draws: int_field(object, "draws")?,
        losses: int_field(object, "losses")?,
    })
}

fn game_from_decoded(object: &DecodedObject) -> Result<GameState, ObserverError> {
    Ok(GameState {
        mux_template: hash_field_any(object, &["mux_template", "gen__mux_template"])?,
        player_template: optional_hash_field(object, "gen__player_template")?.unwrap_or_default(),
        route_templates: bytes_field_any(object, &["route_templates", "gen__mux_routes"])?,
        white_player: hash_field(object, "white_player")?,
        black_player: hash_field(object, "black_player")?,
        board: bytes_field(object, "board")?,
        turn: byte_field(object, "turn")?,
        status: byte_field(object, "status")?,
        move_timeout: int_field(object, "move_timeout")?,
        castle_rights: bytes_field(object, "castle_rights")?,
        en_passant_idx: byte_field(object, "en_passant_idx")?,
        pending_src_idx: byte_field(object, "pending_src_idx")?,
        pending_dst_idx: byte_field(object, "pending_dst_idx")?,
        pending_promo: byte_field(object, "pending_promo")?,
        recent_castle: byte_field(object, "recent_castle")?,
        draw_state: byte_field(object, "draw_state")?,
    })
}

fn settle_from_decoded(object: &DecodedObject) -> Result<SettleState, ObserverError> {
    Ok(SettleState {
        player_template: hash_field_any(object, &["player_template", "gen__player_template"])?,
        white_player: hash_field(object, "white_player")?,
        black_player: hash_field(object, "black_player")?,
        status: byte_field(object, "status")?,
    })
}

#[allow(clippy::too_many_arguments)]
fn route_game_state(
    state: &GameState,
    selector: i64,
    from_x: i64,
    from_y: i64,
    to_x: i64,
    to_y: i64,
    promo_piece: i64,
    termination_action: i64,
) -> Result<GameState, ObserverError> {
    let mut next = state.clone();
    if selector == MUX {
        next.en_passant_idx = OFFBOARD;
        next.recent_castle = CLEAR;
        if termination_action == CLAIM {
            next.turn = 1 - state.turn;
            next.draw_state = CLAIMED;
        } else if termination_action == SURRENDER {
            next.status = BWIN - state.turn;
            next.draw_state = NORMAL;
        } else {
            next.status = DRAW;
            next.draw_state = NORMAL;
        }
        return Ok(next);
    }

    if state.draw_state > NORMAL {
        next.draw_state = NORMAL;
    }
    if termination_action == OFFER {
        next.draw_state = WOFFER + state.turn;
    }
    next.pending_src_idx = square_idx(from_x, from_y);
    next.pending_dst_idx = square_idx(to_x, to_y);
    next.pending_promo = promo_piece;
    next.en_passant_idx = state.en_passant_idx;
    next.recent_castle = if selector == PREP { state.recent_castle } else { CLEAR };
    Ok(next)
}

fn apply_worker_state(worker: WorkerKind, state: &GameState) -> Result<GameState, ObserverError> {
    if worker == WorkerKind::CastleChallenge {
        return apply_castle_challenge_state(state);
    }

    let pending = MoveSpec {
        from_x: state.pending_src_idx % 8,
        from_y: state.pending_src_idx / 8,
        to_x: state.pending_dst_idx % 8,
        to_y: state.pending_dst_idx / 8,
        promo_piece: state.pending_promo,
    };
    let mut next = apply_move_to_state(state, pending)?;
    next.castle_rights = match worker {
        WorkerKind::Pawn | WorkerKind::Knight | WorkerKind::Diag => state.castle_rights.clone(),
        WorkerKind::Vert | WorkerKind::Horiz => {
            let mut castle_rights = state.castle_rights.clone();
            if state.pending_src_idx == 0 || state.pending_dst_idx == 0 {
                castle_rights[1] = 0;
            }
            if state.pending_src_idx == 7 || state.pending_dst_idx == 7 {
                castle_rights[0] = 0;
            }
            if state.pending_src_idx == 56 || state.pending_dst_idx == 56 {
                castle_rights[3] = 0;
            }
            if state.pending_src_idx == 63 || state.pending_dst_idx == 63 {
                castle_rights[2] = 0;
            }
            castle_rights
        }
        WorkerKind::King | WorkerKind::Castle => {
            let mut castle_rights = state.castle_rights.clone();
            let moving_piece = state.board[state.pending_src_idx as usize];
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
        WorkerKind::CastleChallenge => unreachable!(),
    };
    if worker == WorkerKind::Castle {
        next.status = state.status;
        next.draw_state = state.draw_state;
        return Ok(next);
    }

    let target_piece = state.board[state.pending_dst_idx as usize];
    let target_num = i64::from(target_piece);
    let is_draw_claim_mode = state.draw_state < NORMAL;
    let effective_turn = if is_draw_claim_mode { 1 - state.turn } else { state.turn };

    let mut next_status = state.status;
    if state.recent_castle != CLEAR {
        next_status = if state.turn == WHITE { WWIN } else { BWIN };
    } else if is_draw_claim_mode {
        if effective_turn == WHITE && target_num == 14 {
            next_status = if state.turn == WHITE { WWIN } else { BWIN };
        }
        if effective_turn == BLACK && target_num == 6 {
            next_status = if state.turn == WHITE { WWIN } else { BWIN };
        }
    } else {
        let moving_piece = state.board[state.pending_src_idx as usize];
        let moving_is_black = moving_piece > 8;
        if !moving_is_black && target_num == 14 {
            next_status = WWIN;
        }
        if moving_is_black && target_num == 6 {
            next_status = BWIN;
        }
    }

    let mut next_draw_state = state.draw_state;
    if state.draw_state == CLAIMED {
        next_draw_state = DEFENSE;
    } else if state.draw_state == DEFENSE && next_status == LIVE {
        next_status = if state.turn == WHITE { BWIN } else { WWIN };
    }

    next.status = next_status;
    next.draw_state = next_draw_state;
    Ok(next)
}

fn apply_castle_challenge_state(state: &GameState) -> Result<GameState, ObserverError> {
    let to_idx = state.pending_dst_idx;
    let board = &state.board;
    let recent_castle = state.recent_castle;

    let is_white_castle = recent_castle == 1 || recent_castle == 2;
    let is_king_side = recent_castle == 1 || recent_castle == 3;
    let row_base = if is_white_castle { 0 } else { 56 };
    let king_piece = if is_white_castle { 0x06 } else { 0x0e };
    let rook_piece = if is_white_castle { 0x04 } else { 0x0c };

    let start_idx = row_base + 4;
    let transit_idx = if is_king_side { row_base + 5 } else { row_base + 3 };
    let dest_idx = if is_king_side { row_base + 6 } else { row_base + 2 };

    let phase = if to_idx == start_idx {
        1
    } else if to_idx == transit_idx {
        2
    } else if to_idx == dest_idx {
        3
    } else {
        return Err(ObserverError("castle challenge destination is not on the castle lane".to_string()));
    };

    let mut proof_board = board.clone();
    if is_king_side {
        let (a, b, c, d) = if phase == 1 {
            (king_piece, 0u8, 0u8, rook_piece)
        } else if phase == 2 {
            (0u8, king_piece, 0u8, rook_piece)
        } else {
            (0u8, rook_piece, king_piece, 0u8)
        };
        proof_board[(row_base + 4) as usize] = a;
        proof_board[(row_base + 5) as usize] = b;
        proof_board[(row_base + 6) as usize] = c;
        proof_board[(row_base + 7) as usize] = d;
    } else {
        let (a, b, c, d) = if phase == 1 {
            (rook_piece, 0u8, 0u8, king_piece)
        } else if phase == 2 {
            (rook_piece, 0u8, king_piece, 0u8)
        } else {
            (0u8, king_piece, rook_piece, 0u8)
        };
        proof_board[row_base as usize] = a;
        proof_board[(row_base + 2) as usize] = b;
        proof_board[(row_base + 3) as usize] = c;
        proof_board[(row_base + 4) as usize] = d;
    }

    Ok(GameState { board: proof_board, en_passant_idx: OFFBOARD, pending_promo: CLEAR, ..state.clone() })
}

fn timeout_status(turn: i64, draw_state: i64) -> i64 {
    if draw_state == CLAIMED {
        DRAW
    } else if turn == WHITE {
        BWIN
    } else {
        WWIN
    }
}

fn settle_players(
    tx: &Transaction,
    settle_input_index: usize,
    settle: &SettleState,
    white_in: &PlayerState,
    black_in: &PlayerState,
) -> Result<(PlayerState, PlayerState), ObserverError> {
    let _ = tx;
    let _ = settle_input_index;
    let (mut white_wins, mut white_draws, mut white_losses) = (white_in.wins, white_in.draws, white_in.losses);
    let (mut black_wins, mut black_draws, mut black_losses) = (black_in.wins, black_in.draws, black_in.losses);
    let (mut white_actual, mut black_actual) = (0, 0);
    if settle.status == WWIN {
        white_wins += 1;
        black_losses += 1;
        white_actual = 1000;
    } else if settle.status == BWIN {
        black_wins += 1;
        white_losses += 1;
        black_actual = 1000;
    } else {
        white_draws += 1;
        black_draws += 1;
        white_actual = 500;
        black_actual = 500;
    }

    let diff = black_in.rating - white_in.rating;
    let abs_diff = diff.abs();
    let mut favored_expected = 990;
    if abs_diff < 800 {
        favored_expected = 970;
        if abs_diff < 600 {
            favored_expected = 910;
            if abs_diff < 400 {
                favored_expected = 820;
                if abs_diff < 250 {
                    favored_expected = 700;
                    if abs_diff < 150 {
                        favored_expected = 600;
                        if abs_diff < 75 {
                            favored_expected = 500;
                        }
                    }
                }
            }
        }
    }

    let (mut white_expected, mut black_expected) = (500, 500);
    if diff < 0 {
        white_expected = favored_expected;
        black_expected = 1000 - favored_expected;
    } else if diff > 0 {
        white_expected = 1000 - favored_expected;
        black_expected = favored_expected;
    }

    let white_rating = white_in.rating + ((32 * (white_actual - white_expected)) / 1000);
    let black_rating = black_in.rating + ((32 * (black_actual - black_expected)) / 1000);

    Ok((
        PlayerState {
            open_games: white_in.open_games - 1,
            rating: white_rating,
            games: white_in.games + 1,
            wins: white_wins,
            draws: white_draws,
            losses: white_losses,
            ..white_in.clone()
        },
        PlayerState {
            open_games: black_in.open_games - 1,
            rating: black_rating,
            games: black_in.games + 1,
            wins: black_wins,
            draws: black_draws,
            losses: black_losses,
            ..black_in.clone()
        },
    ))
}

#[derive(Clone, Copy)]
struct MoveSpec {
    from_x: i64,
    from_y: i64,
    to_x: i64,
    to_y: i64,
    promo_piece: i64,
}

fn apply_move_to_state(game: &GameState, mv: MoveSpec) -> Result<GameState, ObserverError> {
    let next = apply_protocol_move(
        &ProtocolState {
            board: game.board.clone(),
            turn: game.turn,
            castle_rights: [game.castle_rights[0], game.castle_rights[1], game.castle_rights[2], game.castle_rights[3]],
            en_passant_idx: game.en_passant_idx,
        },
        ProtocolMoveSpec { from_x: mv.from_x, from_y: mv.from_y, to_x: mv.to_x, to_y: mv.to_y, promo_piece: mv.promo_piece },
    )
    .map_err(|err| ObserverError(err.to_string()))?;

    Ok(GameState {
        board: next.board,
        turn: next.turn,
        castle_rights: next.castle_rights.to_vec(),
        en_passant_idx: next.en_passant_idx,
        pending_src_idx: OFFBOARD,
        pending_dst_idx: OFFBOARD,
        pending_promo: 0,
        recent_castle: next.recent_castle,
        ..game.clone()
    })
}

fn square_idx(x: i64, y: i64) -> i64 {
    y * 8 + x
}

fn blake2b(bytes: &[u8]) -> Hash {
    Hash::from_slice(Blake2bParams::new().hash_length(32).to_state().update(bytes).finalize().as_bytes())
}

fn load_templates() -> Result<ObserverTemplates, ObserverError> {
    let artifact: Artifact =
        serde_json::from_str(include_str!("../../build/artifact.json")).map_err(|err| ObserverError(err.to_string()))?;
    artifact.check_schema_version().map_err(|err| ObserverError(err.to_string()))?;
    artifact.verify_id().map_err(|err| ObserverError(err.to_string()))?;
    let specs = [
        (ChessInputKind::League, "League"),
        (ChessInputKind::Player, "Player"),
        (ChessInputKind::Mux, "Mux"),
        (ChessInputKind::Settle, "Settle"),
        (ChessInputKind::Worker(WorkerKind::Pawn), "Pawn"),
        (ChessInputKind::Worker(WorkerKind::Knight), "Knight"),
        (ChessInputKind::Worker(WorkerKind::Vert), "Vert"),
        (ChessInputKind::Worker(WorkerKind::Horiz), "Horiz"),
        (ChessInputKind::Worker(WorkerKind::Diag), "Diag"),
        (ChessInputKind::Worker(WorkerKind::King), "King"),
        (ChessInputKind::Worker(WorkerKind::Castle), "Castle"),
        (ChessInputKind::Worker(WorkerKind::CastleChallenge), "CastleChallengePrep"),
    ];
    let contracts = specs
        .into_iter()
        .map(|(kind, contract)| {
            ContractTemplate::from_artifact(&artifact, contract).map(|template| (kind, template)).map_err(ObserverError::from)
        })
        .collect::<Result<_, _>>()?;
    Ok(ObserverTemplates { contracts })
}

fn opening_board() -> Vec<u8> {
    vec![
        0x04, 0x02, 0x03, 0x05, 0x06, 0x03, 0x02, 0x04, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x0c, 0x0a, 0x0b, 0x0d, 0x0e, 0x0b, 0x0a,
        0x0c,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::{GameResult, MoveSpec, SigningPlayer, TxArena, WorkerKind};
    use argent_runtime::{state, TxBuilder, TxContext};
    use kaspa_consensus_core::tx::{CovenantBinding, TransactionId, TransactionOutpoint};

    #[test]
    fn observer_decodes_generated_argent_transaction() {
        let artifact: Artifact =
            serde_json::from_str(include_str!("../../build/artifact.json")).expect("pinned chess artifact deserializes");
        let builder = TxBuilder::new(&artifact).expect("pinned chess artifact is valid");
        let covenant_id = Hash::from_bytes([0x91; 32]);
        let white_player = Hash::from_bytes([0x92; 32]);
        let black_player = Hash::from_bytes([0x93; 32]);
        let game_state = state! {
            white_player: white_player,
            black_player: black_player,
            board: opening_board(),
            turn: WHITE as u8,
            status: WWIN as u8,
            move_timeout: 600i64,
            castle_rights: [1u8; 4],
            en_passant_idx: OFFBOARD as u8,
            pending_src_idx: OFFBOARD as u8,
            pending_dst_idx: OFFBOARD as u8,
            pending_promo: CLEAR as u8,
            recent_castle: CLEAR as u8,
            draw_state: NORMAL as u8,
        };
        let outpoint = TransactionOutpoint::new(TransactionId::from_bytes([0x94; 32]), 0);
        let utxo =
            builder.covenant_utxo("Mux", game_state.clone(), 1_000, 0, false, Some(covenant_id)).expect("terminal mux UTXO builds");
        let context = TxContext::new().actor_input("Mux", game_state, "settle", outpoint, utxo, 0).actor_output(
            "Settle",
            state! {
                white_player: white_player,
                black_player: black_player,
                status: WWIN as u8,
            },
            CovenantBinding::new(0, covenant_id),
            1_000,
        );
        let tx = builder.build(&context).expect("terminal mux routes to settlement");

        let emitter = ChessEventEmitter::load().expect("observer loads");
        let observed = emitter.observe_tx(&tx, covenant_id).expect("generated Argent transaction decodes");
        assert_eq!(observed.inputs[0].function, "settle");
        assert!(matches!(observed.events.as_slice(), [ChessEvent::SettleCreated { status: WWIN, .. }]));
        match &observed.inputs[0].input_state {
            ChessState::Game(state) => assert_ne!(state.player_template, Hash::default()),
            other => panic!("expected game state, got {other:?}"),
        }
    }

    #[test]
    fn observer_decodes_real_arena_transactions_end_to_end() {
        let mut arena = TxArena::new().expect("tx arena");
        let mut white = SigningPlayer::from_seed("white", 1);
        let mut black = SigningPlayer::from_seed("black", 2);

        arena.register_player(&mut white).expect("register white");
        arena.register_player(&mut black).expect("register black");
        arena.start_game(&white, &black).expect("start game");
        arena.submit_move(&white, MoveSpec::new(4, 1, 4, 3)).expect("submit e2e4");
        arena.surrender(&black).expect("black surrender");
        arena.settle_game(&white, &black, GameResult::WhiteWin).expect("settle");
        arena.retire_player(&white).expect("retire");

        let emitter = ChessEventEmitter::load().expect("observer");
        let covenant_id = arena.covenant_id();
        let txs = arena.transactions().to_vec();
        assert_eq!(txs.len(), 9, "expected register/register/start/route/apply/surrender/mux_settle/settle/retire");

        let register_white = emitter.observe_tx(&txs[0], covenant_id).expect("observe white register");
        assert_eq!(register_white.inputs.len(), 1);
        assert_eq!(register_white.inputs[0].function, "register_player");
        assert_eq!(register_white.inputs[0].outputs.len(), 2);
        assert!(matches!(register_white.events.as_slice(), [ChessEvent::PlayerRegistered { rating: 1200, .. }]));
        match &register_white.inputs[0].outputs[1].state {
            ChessState::Player(player) => {
                assert_eq!(player.open_games, 0);
                assert_eq!(player.rating, 1200);
            }
            other => panic!("expected player output, got {other:?}"),
        }

        let start = emitter.observe_tx(&txs[2], covenant_id).expect("observe start");
        assert_eq!(start.inputs.len(), 2);
        let leader = start.inputs.iter().find(|input| input.function == "start_game").expect("start leader");
        assert_eq!(leader.outputs.len(), 3);
        assert!(matches!(start.events.as_slice(), [ChessEvent::GameStarted { move_timeout: 600, .. }]));
        match &leader.outputs[2].state {
            ChessState::Game(game) => {
                assert_eq!(game.turn, WHITE);
                assert_eq!(game.move_timeout, 600);
                assert_eq!(game.status, LIVE);
            }
            other => panic!("expected opening game, got {other:?}"),
        }

        let route = emitter.observe_tx(&txs[3], covenant_id).expect("observe route");
        assert_eq!(route.inputs.len(), 1);
        assert_eq!(route.inputs[0].function, "route");
        assert_eq!(route.inputs[0].outputs.len(), 1);

        let apply = emitter.observe_tx(&txs[4], covenant_id).expect("observe apply");
        assert_eq!(apply.inputs.len(), 1);
        assert_eq!(apply.inputs[0].kind, ChessInputKind::Worker(WorkerKind::Pawn));
        assert!(matches!(apply.events.as_slice(), [ChessEvent::WorkerApplied { worker: WorkerKind::Pawn, .. }]));
        match &apply.inputs[0].outputs[0].state {
            ChessState::Game(game) => {
                assert_eq!(game.turn, BLACK);
                assert_eq!(game.pending_src_idx, OFFBOARD);
                assert_eq!(game.pending_dst_idx, OFFBOARD);
            }
            other => panic!("expected mux game after apply, got {other:?}"),
        }

        let surrender = emitter.observe_tx(&txs[5], covenant_id).expect("observe surrender");
        assert_eq!(surrender.inputs[0].function, "terminate");
        match &surrender.inputs[0].outputs[0].state {
            ChessState::Game(game) => assert_eq!(game.status, WWIN),
            other => panic!("expected terminal mux, got {other:?}"),
        }

        let mux_settle = emitter.observe_tx(&txs[6], covenant_id).expect("observe mux settle");
        assert!(matches!(mux_settle.events.as_slice(), [ChessEvent::SettleCreated { status: WWIN, .. }]));
        match &mux_settle.inputs[0].outputs[0].state {
            ChessState::Settle(settle) => assert_eq!(settle.status, WWIN),
            other => panic!("expected settle state, got {other:?}"),
        }

        let settle = emitter.observe_tx(&txs[7], covenant_id).expect("observe settle");
        assert_eq!(settle.inputs.len(), 3);
        assert!(matches!(settle.events.as_slice(), [ChessEvent::SettlementApplied { status: WWIN, .. }]));
        let settle_leader = settle
            .inputs
            .iter()
            .find(|input| input.function == "settle" && input.kind == ChessInputKind::Settle)
            .expect("settle leader");
        assert_eq!(settle_leader.outputs.len(), 2);
        match &settle_leader.outputs[0].state {
            ChessState::Player(player) => {
                assert_eq!(player.open_games, 0);
                assert_eq!(player.games, 1);
                assert_eq!(player.wins, 1);
            }
            other => panic!("expected settled white player, got {other:?}"),
        }

        let retire = emitter.observe_tx(&txs[8], covenant_id).expect("observe retire");
        assert_eq!(retire.inputs.len(), 1);
        assert_eq!(retire.inputs[0].function, "retire");
        assert!(retire.inputs[0].outputs.is_empty());
        assert!(matches!(retire.events.as_slice(), [ChessEvent::PlayerRetired { .. }]));
    }
}

use std::collections::BTreeMap;

use kaspa_consensus_core::tx::{Transaction, TransactionId};
use kaspa_consensus_core::Hash;

use crate::observer::{
    ChessEventEmitter, ChessInputKind, ChessState, GameState, LeagueState, ObservedTx, ObserverError, PlayerState, SettleState,
};

#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    #[error(transparent)]
    Observe(#[from] ObserverError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlayerPair {
    pub white_player: Hash,
    pub black_player: Hash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutputRef {
    pub txid: TransactionId,
    pub output_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedPlayer {
    pub player_ref: Hash,
    pub txid: TransactionId,
    pub output_index: usize,
    pub value: u64,
    pub state: PlayerState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveEntry<T> {
    pub outpoint: OutputRef,
    pub pair: PlayerPair,
    pub value: u64,
    pub state: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedTransaction {
    pub txid: TransactionId,
    pub observed: ObservedTx,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChessChainIndex {
    pub league_lane_count: usize,
    pub latest_league: Option<LeagueState>,
    pub players: Vec<IndexedPlayer>,
    pub active_games: Vec<ActiveEntry<GameState>>,
    pub active_settles: Vec<ActiveEntry<SettleState>>,
    pub transactions: Vec<IndexedTransaction>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ChessIndexer {
    emitter: ChessEventEmitter,
}

impl ChessIndexer {
    pub fn load() -> Result<Self, IndexerError> {
        Ok(Self { emitter: ChessEventEmitter::load()? })
    }

    pub fn index_transactions(&self, txs: &[Transaction], covenant_id: Hash) -> Result<ChessChainIndex, IndexerError> {
        let mut bootstrapped_league = false;
        let mut league_lane_count = 0usize;
        let mut latest_league = None::<LeagueState>;
        let mut players = BTreeMap::<Hash, IndexedPlayer>::new();
        let mut active_games = BTreeMap::<OutputRef, ActiveEntry<GameState>>::new();
        let mut active_settles = BTreeMap::<OutputRef, ActiveEntry<SettleState>>::new();
        let mut transactions = Vec::with_capacity(txs.len());
        let mut warnings = Vec::new();

        for tx in txs {
            let txid = tx.id();
            let observed = self.emitter.observe_tx(tx, covenant_id)?;

            let league_inputs = observed.inputs.iter().filter(|input| input.kind == ChessInputKind::League).count();
            let league_outputs = observed
                .inputs
                .iter()
                .flat_map(|input| &input.outputs)
                .filter(|output| matches!(output.state, ChessState::League(_)))
                .count();
            if league_outputs > 0 {
                if !bootstrapped_league {
                    league_lane_count = league_outputs;
                    bootstrapped_league = true;
                } else {
                    league_lane_count = league_lane_count + league_outputs - league_inputs;
                }
            }

            for input in &observed.inputs {
                match (&input.kind, input.function.as_str()) {
                    (ChessInputKind::League, "register_player")
                    | (ChessInputKind::League, "rebalance")
                    | (ChessInputKind::League, "fork") => {
                        if let Some(output) = input.outputs.iter().find_map(|output| match &output.state {
                            ChessState::League(league) => Some(league.clone()),
                            _ => None,
                        }) {
                            latest_league = Some(output);
                        }
                    }
                    (ChessInputKind::Player, "start_game") => {
                        for output in &input.outputs {
                            match &output.state {
                                ChessState::Player(player) => update_player(&mut players, tx, output.output_index, player.clone()),
                                ChessState::Game(game) => insert_active_game(&mut active_games, tx, output.output_index, game.clone()),
                                _ => {}
                            }
                        }
                    }
                    (ChessInputKind::Player, "rebalance") => {
                        if let Some(player) = input.outputs.iter().find_map(|output| match &output.state {
                            ChessState::Player(player) => Some((output.output_index, player.clone())),
                            _ => None,
                        }) {
                            update_player(&mut players, tx, player.0, player.1);
                        }
                    }
                    (ChessInputKind::Player, "retire") => {
                        let ChessState::Player(player) = &input.input_state else {
                            continue;
                        };
                        players.remove(&player_ref(player));
                    }
                    (ChessInputKind::Mux, "route") | (ChessInputKind::Worker(_), "apply") => {
                        let ChessState::Game(_) = &input.input_state else {
                            continue;
                        };
                        let spent = input_outpoint(tx, input.input_index);
                        if let Some((output_index, ChessState::Game(next_game))) = input
                            .outputs
                            .iter()
                            .map(|output| (output.output_index, output.state.clone()))
                            .find(|(_, state)| matches!(state, ChessState::Game(_)))
                        {
                            replace_active_game(
                                &mut active_games,
                                spent,
                                tx,
                                output_index,
                                next_game,
                                &mut warnings,
                                input.function.as_str(),
                            );
                        }
                    }
                    (ChessInputKind::Mux, "timeout") | (ChessInputKind::Mux, "settle") | (ChessInputKind::Worker(_), "timeout") => {
                        let ChessState::Game(_) = &input.input_state else {
                            continue;
                        };
                        let spent = input_outpoint(tx, input.input_index);
                        consume_active_game(&mut active_games, spent, &mut warnings, input.function.as_str());
                        if let Some((output_index, ChessState::Settle(settle))) = input
                            .outputs
                            .iter()
                            .map(|output| (output.output_index, output.state.clone()))
                            .find(|(_, state)| matches!(state, ChessState::Settle(_)))
                        {
                            insert_active_settle(&mut active_settles, tx, output_index, settle);
                        }
                    }
                    (ChessInputKind::Settle, "settle") => {
                        let ChessState::Settle(_) = &input.input_state else {
                            continue;
                        };
                        let spent = input_outpoint(tx, input.input_index);
                        consume_active_settle(&mut active_settles, spent, &mut warnings, input.function.as_str());
                        for output in &input.outputs {
                            if let ChessState::Player(player) = &output.state {
                                update_player(&mut players, tx, output.output_index, player.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }

            transactions.push(IndexedTransaction { txid, observed });
        }

        Ok(ChessChainIndex {
            league_lane_count,
            latest_league,
            players: players.into_values().collect(),
            active_games: active_games.into_values().collect(),
            active_settles: active_settles.into_values().collect(),
            transactions,
            warnings,
        })
    }
}

fn update_player(players: &mut BTreeMap<Hash, IndexedPlayer>, tx: &Transaction, output_index: usize, player: PlayerState) {
    let player_ref = player_ref(&player);
    players.insert(
        player_ref,
        IndexedPlayer { player_ref, txid: tx.id(), output_index, value: tx.outputs[output_index].value, state: player },
    );
}

fn insert_active_game(
    active_games: &mut BTreeMap<OutputRef, ActiveEntry<GameState>>,
    tx: &Transaction,
    output_index: usize,
    game: GameState,
) {
    let pair = pair_key(game.white_player, game.black_player);
    let outpoint = created_outpoint(tx, output_index);
    active_games.insert(outpoint, ActiveEntry { outpoint, pair, value: tx.outputs[output_index].value, state: game });
}

fn replace_active_game(
    active_games: &mut BTreeMap<OutputRef, ActiveEntry<GameState>>,
    spent: OutputRef,
    tx: &Transaction,
    output_index: usize,
    game: GameState,
    warnings: &mut Vec<String>,
    label: &str,
) {
    if active_games.remove(&spent).is_none() {
        warnings.push(format!("{label} updated unseen game outpoint {}", short_outpoint(spent)));
    }
    insert_active_game(active_games, tx, output_index, game);
}

fn consume_active_game(
    active_games: &mut BTreeMap<OutputRef, ActiveEntry<GameState>>,
    spent: OutputRef,
    warnings: &mut Vec<String>,
    label: &str,
) {
    if active_games.remove(&spent).is_none() {
        warnings.push(format!("{label} consumed unseen game outpoint {}", short_outpoint(spent)));
    }
}

fn insert_active_settle(
    active_settles: &mut BTreeMap<OutputRef, ActiveEntry<SettleState>>,
    tx: &Transaction,
    output_index: usize,
    settle: SettleState,
) {
    let pair = pair_key(settle.white_player, settle.black_player);
    let outpoint = created_outpoint(tx, output_index);
    active_settles.insert(outpoint, ActiveEntry { outpoint, pair, value: tx.outputs[output_index].value, state: settle });
}

fn consume_active_settle(
    active_settles: &mut BTreeMap<OutputRef, ActiveEntry<SettleState>>,
    spent: OutputRef,
    warnings: &mut Vec<String>,
    label: &str,
) {
    if active_settles.remove(&spent).is_none() {
        warnings.push(format!("{label} consumed unseen settle outpoint {}", short_outpoint(spent)));
    }
}

fn input_outpoint(tx: &Transaction, input_index: usize) -> OutputRef {
    let prev = &tx.inputs[input_index].previous_outpoint;
    OutputRef { txid: prev.transaction_id, output_index: prev.index as usize }
}

fn created_outpoint(tx: &Transaction, output_index: usize) -> OutputRef {
    OutputRef { txid: tx.id(), output_index }
}

fn pair_key(white_player: Hash, black_player: Hash) -> PlayerPair {
    PlayerPair { white_player, black_player }
}

fn player_ref(player: &PlayerState) -> Hash {
    let owner = player.owner.as_bytes();
    let player_id = player.player_id.as_bytes();
    Hash::from_slice(
        blake2b_simd::Params::new()
            .hash_length(32)
            .to_state()
            .update(&[owner.as_slice(), player_id.as_slice()].concat())
            .finalize()
            .as_bytes(),
    )
}

fn short_hash(hash: Hash) -> String {
    let full = hash.to_string();
    full.chars().take(8).collect()
}

fn short_outpoint(outpoint: OutputRef) -> String {
    format!("{}:{}", short_hash(outpoint.txid), outpoint.output_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observer::ChessEvent;
    use crate::orchestrator::{GameResult, MoveSpec, SigningPlayer, TxArena};

    #[test]
    fn indexer_tracks_real_arena_transactions_end_to_end() {
        let mut arena = TxArena::new().expect("tx arena");
        let mut white = SigningPlayer::from_seed("white", 1);
        let mut black = SigningPlayer::from_seed("black", 2);

        arena.register_player(&mut white).expect("register white");
        arena.register_player(&mut black).expect("register black");
        arena.start_game(&white, &black).expect("start game");
        arena.submit_move(&white, MoveSpec::new(4, 1, 4, 3)).expect("submit e2e4");
        arena.surrender(&black).expect("black surrender");
        arena.settle_game(&white, &black, GameResult::WhiteWin).expect("settle");

        let indexer = ChessIndexer::load().expect("indexer");
        let chain = indexer.index_transactions(arena.transactions(), arena.covenant_id()).expect("chain");

        assert_eq!(chain.league_lane_count, 1);
        assert_eq!(chain.players.len(), 2);
        assert!(chain.active_games.is_empty());
        assert!(chain.active_settles.is_empty());
        assert_eq!(chain.transactions.len(), 8);

        let white_ref = white.player_ref.expect("white ref");
        let white_player = chain.players.iter().find(|player| player.player_ref == white_ref).expect("white indexed player");
        assert_eq!(white_player.state.open_games, 0);
        assert_eq!(white_player.state.games, 1);
        assert_eq!(white_player.state.wins, 1);
        assert_eq!(white_player.value, 2_000);

        assert!(
            chain
                .transactions
                .iter()
                .flat_map(|tx| &tx.observed.events)
                .any(|event| matches!(event, ChessEvent::SettlementApplied { .. })),
            "expected settlement event in indexed tx stream"
        );
        assert!(chain.warnings.is_empty(), "unexpected indexer warnings: {:?}", chain.warnings);
    }
}

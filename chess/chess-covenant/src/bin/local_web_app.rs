use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;

use chess_covenant::indexer::{ActiveEntry, ChessChainIndex, ChessIndexer, IndexedPlayer, IndexedTransaction, OutputRef};
use chess_covenant::observer::{ChessEvent, ChessInputKind, ChessState, GameState, SettleState};
use chess_covenant::orchestrator::{
    ActualGameSnapshot, GameResult, MoveSpec, OffchainMessage, OffchainMessageKind, SigningPlayer, SubmittedTx, TxArena,
    TxOrchestrator,
};
use kaspa_consensus_core::{tx::TransactionId, Hash};
use serde::{Deserialize, Serialize};

const ADDRESS: &str = "127.0.0.1:8080";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(ADDRESS)?;
    let mut app = LocalWebController::new()?;
    println!("Chess local web app listening on http://{ADDRESS}");

    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Err(err) = handle_connection(&mut stream, &mut app) {
            let _ = write_response(&mut stream, 500, "text/plain; charset=utf-8", err.to_string().as_bytes());
        }
    }

    Ok(())
}

fn handle_connection(stream: &mut TcpStream, app: &mut LocalWebController) -> Result<(), Box<dyn std::error::Error>> {
    let request = read_http_request(stream)?;
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => write_response(stream, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes())?,
        ("GET", "/api/state") => {
            let body = serde_json::to_vec(&ApiResponse::ok(app.snapshot()))?;
            write_response(stream, 200, "application/json", &body)?;
        }
        ("POST", "/api/action") => {
            let action: ActionRequest = serde_json::from_slice(&request.body)?;
            let response = match app.handle_action(action) {
                Ok(()) => ApiResponse::ok(app.snapshot()),
                Err(err) => ApiResponse::err(err.to_string(), app.snapshot()),
            };
            let body = serde_json::to_vec(&response)?;
            write_response(stream, 200, "application/json", &body)?;
        }
        _ => write_response(stream, 404, "text/plain; charset=utf-8", b"not found")?,
    }
    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, Box<dyn std::error::Error>> {
    stream.set_read_timeout(Some(std::time::Duration::from_millis(250)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;

    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);

        if header_end.is_none() {
            header_end = find_subslice(&buf, b"\r\n\r\n");
            if let Some(end) = header_end {
                let header_text = String::from_utf8_lossy(&buf[..end]);
                for line in header_text.lines() {
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        content_length = value.trim().parse::<usize>()?;
                    }
                }
                if buf.len() >= end + 4 + content_length {
                    break;
                }
            }
        } else if let Some(end) = header_end {
            if buf.len() >= end + 4 + content_length {
                break;
            }
        }
    }

    let header_end = header_end.ok_or("malformed request: missing header terminator")?;
    let header = String::from_utf8(buf[..header_end].to_vec())?;
    let mut lines = header.lines();
    let request_line = lines.next().ok_or("malformed request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let path = parts.next().ok_or("missing path")?.to_string();
    let body_start = header_end + 4;
    let body = buf[body_start..body_start + content_length].to_vec();

    Ok(HttpRequest { method, path, body })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let status_text = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

struct LocalWebController {
    arena: Rc<RefCell<TxArena>>,
    indexer: ChessIndexer,
    white: TxOrchestrator,
    black: TxOrchestrator,
    notices: Vec<String>,
}

impl LocalWebController {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let arena = TxArena::shared()?;
        let indexer = ChessIndexer::load()?;
        let white = TxOrchestrator::new("white", 0x41, arena.clone());
        let black = TxOrchestrator::new("black", 0x42, arena.clone());
        Ok(Self { arena, indexer, white, black, notices: Vec::new() })
    }

    fn handle_action(&mut self, action: ActionRequest) -> Result<(), Box<dyn std::error::Error>> {
        match action.action.as_str() {
            "reset_session" => {
                *self = Self::new()?;
            }
            "register" => self.player_mut(action.actor.as_deref().ok_or("missing actor")?)?.register()?,
            "invite" => self.white.send_game_invite(&self.black)?,
            "accept_invite" => self.black.accept_game_invite(&self.white)?,
            "start_game" => self.white.start_game(&self.black)?,
            "move" => {
                let mv = parse_move_label(action.move_label.as_deref().ok_or("missing move label")?)?;
                self.player(action.actor.as_deref().ok_or("missing actor")?)?.submit_move(mv)?;
            }
            "force_move" => {
                let mv = parse_move_label(action.move_label.as_deref().ok_or("missing move label")?)?;
                self.player(action.actor.as_deref().ok_or("missing actor")?)?.force_move(mv)?;
            }
            "surrender" => self.player(action.actor.as_deref().ok_or("missing actor")?)?.surrender()?,
            "claim_timeout" => self.player(action.actor.as_deref().ok_or("missing actor")?)?.claim_timeout()?,
            "request_settlement" => {
                let result = parse_result(action.result.as_deref().ok_or("missing result")?)?;
                let actor = action.actor.as_deref().ok_or("missing actor")?;
                self.player(actor)?.request_settlement(self.opponent(actor)?, result)?;
            }
            "settle" => {
                let result = parse_result(action.result.as_deref().ok_or("missing result")?)?;
                self.white.settle(&self.black, result)?;
            }
            "retire" => self.player(action.actor.as_deref().ok_or("missing actor")?)?.retire()?,
            other => return Err(format!("unknown action: {other}").into()),
        }
        self.flush_notices();
        Ok(())
    }

    fn snapshot(&self) -> AppSnapshot {
        let arena = self.arena.borrow();
        let observed = match self.indexer.index_transactions(arena.transactions(), arena.covenant_id()) {
            Ok(chain) => observer_view(&chain, &self.white.player, &self.black.player),
            Err(err) => ObserverView { error: Some(err.to_string()), ..ObserverView::default() },
        };
        AppSnapshot {
            players: if observed.error.is_none() && !observed.players.is_empty() {
                observed.players.clone()
            } else {
                vec![player_view(&arena, &self.white.player, "white"), player_view(&arena, &self.black.player, "black")]
            },
            game: arena.active_game_snapshot().map(game_view),
            history: arena.history().iter().map(history_view).collect(),
            notices: self.notices.clone(),
            observer: observed,
        }
    }

    fn flush_notices(&mut self) {
        let mut white_msgs = self.white.inbox();
        let mut black_msgs = self.black.inbox();
        self.notices.extend(white_msgs.drain(..).map(|msg| format_message("white", &msg)));
        self.notices.extend(black_msgs.drain(..).map(|msg| format_message("black", &msg)));
    }

    fn player(&self, actor: &str) -> Result<&TxOrchestrator, Box<dyn std::error::Error>> {
        match actor {
            "white" => Ok(&self.white),
            "black" => Ok(&self.black),
            _ => Err(format!("unknown actor: {actor}").into()),
        }
    }

    fn player_mut(&mut self, actor: &str) -> Result<&mut TxOrchestrator, Box<dyn std::error::Error>> {
        match actor {
            "white" => Ok(&mut self.white),
            "black" => Ok(&mut self.black),
            _ => Err(format!("unknown actor: {actor}").into()),
        }
    }

    fn opponent(&self, actor: &str) -> Result<&TxOrchestrator, Box<dyn std::error::Error>> {
        match actor {
            "white" => Ok(&self.black),
            "black" => Ok(&self.white),
            _ => Err(format!("unknown actor: {actor}").into()),
        }
    }
}

fn player_view(arena: &TxArena, player: &SigningPlayer, role: &str) -> PlayerView {
    match arena.player_account_snapshot(player) {
        Ok(account) => PlayerView {
            role: role.to_string(),
            player_ref: short_hash(account.player_ref),
            value: Some(account.value),
            registered: true,
            open_games: account.open_games,
            rating: account.rating,
            games: account.games,
            wins: account.wins,
            draws: account.draws,
            losses: account.losses,
        },
        Err(_) => PlayerView {
            role: role.to_string(),
            player_ref: "unregistered".to_string(),
            value: None,
            registered: false,
            open_games: 0,
            rating: 0,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
        },
    }
}

fn game_view(game: ActualGameSnapshot) -> GameView {
    GameView {
        phase: game.phase,
        turn: match game.turn {
            chess_covenant::orchestrator::Side::White => "white".to_string(),
            chess_covenant::orchestrator::Side::Black => "black".to_string(),
        },
        status: match game.status {
            0 => "live".to_string(),
            1 => "white_win".to_string(),
            2 => "black_win".to_string(),
            3 => "draw".to_string(),
            other => format!("status_{other}"),
        },
        value: None,
        move_timeout: None,
        board_rows: board_rows(&game.board),
        move_log: game.move_log,
    }
}

fn observer_view(chain: &ChessChainIndex, white: &SigningPlayer, black: &SigningPlayer) -> ObserverView {
    ObserverView {
        error: None,
        league_lane_count: chain.league_lane_count,
        latest_league_rating: chain.latest_league.as_ref().map(|league| league.base_rating),
        players: chain.players.iter().map(|player| indexed_player_view(player, white, black)).collect(),
        active_games: chain.active_games.iter().map(observed_game_view).collect(),
        active_settles: chain.active_settles.iter().map(observed_settle_view).collect(),
        transactions: chain.transactions.iter().map(observed_tx_view).collect(),
        warnings: chain.warnings.clone(),
    }
}

fn indexed_player_view(player: &IndexedPlayer, white: &SigningPlayer, black: &SigningPlayer) -> PlayerView {
    let role = if white.player_ref == Some(player.player_ref) {
        "white".to_string()
    } else if black.player_ref == Some(player.player_ref) {
        "black".to_string()
    } else {
        short_hash(player.player_ref)
    };
    PlayerView {
        role,
        player_ref: short_hash(player.player_ref),
        value: Some(player.value),
        registered: true,
        open_games: player.state.open_games,
        rating: player.state.rating,
        games: player.state.games,
        wins: player.state.wins,
        draws: player.state.draws,
        losses: player.state.losses,
    }
}

fn observed_game_view(game: &ActiveEntry<GameState>) -> ObservedGameView {
    ObservedGameView {
        outpoint: short_outpoint(game.outpoint),
        pair: format!("{} vs {}", short_hash(game.pair.white_player), short_hash(game.pair.black_player)),
        value: game.value,
        turn: match game.state.turn {
            0 => "white".to_string(),
            1 => "black".to_string(),
            other => format!("turn_{other}"),
        },
        status: status_label(game.state.status),
        move_timeout: game.state.move_timeout,
        board_rows: board_rows(&game.state.board),
        move_log: Vec::new(),
    }
}

fn observed_settle_view(settle: &ActiveEntry<SettleState>) -> ObservedSettleView {
    ObservedSettleView {
        outpoint: short_outpoint(settle.outpoint),
        pair: format!("{} vs {}", short_hash(settle.pair.white_player), short_hash(settle.pair.black_player)),
        value: settle.value,
        status: status_label(settle.state.status),
    }
}

fn observed_tx_view(tx: &IndexedTransaction) -> ObservedTxView {
    ObservedTxView {
        txid: short_txid(tx.txid),
        input_lines: tx.observed.inputs.iter().map(observed_input_line).collect(),
        event_lines: tx.observed.events.iter().map(observed_event_line).collect(),
    }
}

fn observed_input_line(input: &chess_covenant::observer::ObservedInput) -> String {
    let outputs = input
        .outputs
        .iter()
        .map(|output| format!("{}@{}", observed_state_kind(&output.state), output.output_index))
        .collect::<Vec<_>>()
        .join(", ");
    format!("in{} {}.{} -> {}", input.input_index, observed_kind_label(input.kind), input.function, outputs)
}

fn observed_kind_label(kind: ChessInputKind) -> &'static str {
    match kind {
        ChessInputKind::League => "league",
        ChessInputKind::Player => "player",
        ChessInputKind::Mux => "mux",
        ChessInputKind::Settle => "settle",
        ChessInputKind::Worker(_) => "worker",
    }
}

fn observed_state_kind(state: &ChessState) -> &'static str {
    match state {
        ChessState::League(_) => "league",
        ChessState::Player(_) => "player",
        ChessState::Game(_) => "game",
        ChessState::Settle(_) => "settle",
    }
}

fn observed_event_line(event: &ChessEvent) -> String {
    match event {
        ChessEvent::PlayerRegistered { player_ref, rating, .. } => {
            format!("player registered {} rating={rating}", short_hash(*player_ref))
        }
        ChessEvent::LeagueRebalanced { output_index } => format!("league rebalanced -> output {output_index}"),
        ChessEvent::LeagueForked { left_output_index, right_output_index } => {
            format!("league forked -> outputs {left_output_index}, {right_output_index}")
        }
        ChessEvent::GameStarted { white_player, black_player, move_timeout, .. } => {
            format!("game started {} vs {} timeout={move_timeout}", short_hash(*white_player), short_hash(*black_player))
        }
        ChessEvent::PlayerRebalanced { player_ref, output_index } => {
            format!("player rebalanced {} -> output {output_index}", short_hash(*player_ref))
        }
        ChessEvent::PlayerRetired { player_ref } => format!("player retired {}", short_hash(*player_ref)),
        ChessEvent::MoveRouted { selector, termination_action, output_index } => {
            format!("move routed selector={selector} term={termination_action} -> output {output_index}")
        }
        ChessEvent::WorkerApplied { worker, status, next_turn, output_index } => {
            format!("worker {:?} applied status={} next_turn={} -> output {}", worker, status, next_turn, output_index)
        }
        ChessEvent::TimeoutRoutedToSettle { source, status, output_index } => {
            format!("timeout {:?} -> settle status={} output {}", source, status, output_index)
        }
        ChessEvent::SettleCreated { status, output_index } => {
            format!("settle created status={} -> output {}", status_label(*status), output_index)
        }
        ChessEvent::SettlementApplied { status, white_output_index, black_output_index } => {
            format!("settlement applied status={} -> outputs {}, {}", status_label(*status), white_output_index, black_output_index)
        }
    }
}

fn short_hash(hash: Hash) -> String {
    hash.to_string().chars().take(10).collect()
}

fn short_txid(txid: TransactionId) -> String {
    txid.to_string().chars().take(12).collect()
}

fn short_outpoint(outpoint: OutputRef) -> String {
    format!("{}:{}", short_hash(outpoint.txid), outpoint.output_index)
}

fn status_label(status: i64) -> String {
    match status {
        0 => "live".to_string(),
        1 => "white_win".to_string(),
        2 => "black_win".to_string(),
        3 => "draw".to_string(),
        other => format!("status_{other}"),
    }
}

fn board_rows(board: &[u8]) -> Vec<String> {
    (0..8).rev().map(|y| (0..8).map(|x| piece_char(board[(y * 8 + x) as usize])).collect::<String>()).collect()
}

fn piece_char(piece: u8) -> char {
    match piece {
        0 => '.',
        1 => 'P',
        2 => 'N',
        3 => 'B',
        4 => 'R',
        5 => 'Q',
        6 => 'K',
        9 => 'p',
        10 => 'n',
        11 => 'b',
        12 => 'r',
        13 => 'q',
        14 => 'k',
        _ => '?',
    }
}

fn history_view(tx: &SubmittedTx) -> HistoryView {
    HistoryView { recipe_name: tx.recipe_name.to_string(), signer_names: tx.signer_names.clone() }
}

fn format_message(owner: &str, message: &OffchainMessage) -> String {
    match &message.kind {
        OffchainMessageKind::GameInvite { proposed_white, proposed_black } => {
            format!("{owner} inbox: invite {proposed_white} vs {proposed_black}")
        }
        OffchainMessageKind::InviteAccepted { white, black } => format!("{owner} inbox: invite accepted for {white} vs {black}"),
        OffchainMessageKind::GameStarted { white, black } => format!("{owner} inbox: game started for {white} vs {black}"),
        OffchainMessageKind::MoveNotice { actor, move_label, .. } => format!("{owner} inbox: {actor} played {move_label}"),
        OffchainMessageKind::TimeoutClaimAvailable { result, worker, move_label } => {
            format!("{owner} inbox: {move_label} entered {:?}; timeout win {:?} can now be claimed", worker, result)
        }
        OffchainMessageKind::SettlementRequest { result } => format!("{owner} inbox: settlement request {:?}", result),
        OffchainMessageKind::SettlementNotice { result } => format!("{owner} inbox: settlement complete {:?}", result),
    }
}

fn parse_move_label(label: &str) -> Result<MoveSpec, Box<dyn std::error::Error>> {
    let bytes = label.as_bytes();
    if bytes.len() != 4 && bytes.len() != 5 {
        return Err("move label must look like e2e4 or e7e8q".into());
    }
    let from_x = parse_file(bytes[0])?;
    let from_y = parse_rank(bytes[1])?;
    let to_x = parse_file(bytes[2])?;
    let to_y = parse_rank(bytes[3])?;
    let mut mv = MoveSpec::new(from_x, from_y, to_x, to_y);
    if bytes.len() == 5 {
        mv = MoveSpec::with_promotion(from_x, from_y, to_x, to_y, parse_promotion(bytes[4])?);
    }
    Ok(mv)
}

fn parse_file(ch: u8) -> Result<i64, Box<dyn std::error::Error>> {
    match ch {
        b'a'..=b'h' => Ok((ch - b'a') as i64),
        _ => Err("file must be a-h".into()),
    }
}

fn parse_rank(ch: u8) -> Result<i64, Box<dyn std::error::Error>> {
    match ch {
        b'1'..=b'8' => Ok((ch - b'1') as i64),
        _ => Err("rank must be 1-8".into()),
    }
}

fn parse_promotion(ch: u8) -> Result<i64, Box<dyn std::error::Error>> {
    match ch {
        b'n' | b'N' => Ok(2),
        b'b' | b'B' => Ok(3),
        b'r' | b'R' => Ok(4),
        b'q' | b'Q' => Ok(5),
        _ => Err("promotion must be n, b, r, or q".into()),
    }
}

fn parse_result(value: &str) -> Result<GameResult, Box<dyn std::error::Error>> {
    match value {
        "white_win" => Ok(GameResult::WhiteWin),
        "black_win" => Ok(GameResult::BlackWin),
        "draw" => Ok(GameResult::Draw),
        _ => Err("result must be white_win, black_win, or draw".into()),
    }
}

#[derive(Deserialize)]
struct ActionRequest {
    action: String,
    actor: Option<String>,
    move_label: Option<String>,
    result: Option<String>,
}

#[derive(Serialize)]
struct ApiResponse {
    ok: bool,
    error: Option<String>,
    state: AppSnapshot,
}

impl ApiResponse {
    fn ok(state: AppSnapshot) -> Self {
        Self { ok: true, error: None, state }
    }

    fn err(error: String, state: AppSnapshot) -> Self {
        Self { ok: false, error: Some(error), state }
    }
}

#[derive(Serialize)]
struct AppSnapshot {
    players: Vec<PlayerView>,
    game: Option<GameView>,
    history: Vec<HistoryView>,
    notices: Vec<String>,
    observer: ObserverView,
}

#[derive(Serialize, Clone)]
struct PlayerView {
    role: String,
    player_ref: String,
    value: Option<u64>,
    registered: bool,
    open_games: i64,
    rating: i64,
    games: i64,
    wins: i64,
    draws: i64,
    losses: i64,
}

#[derive(Serialize)]
struct GameView {
    phase: String,
    turn: String,
    status: String,
    value: Option<u64>,
    move_timeout: Option<i64>,
    board_rows: Vec<String>,
    move_log: Vec<String>,
}

#[derive(Serialize)]
struct HistoryView {
    recipe_name: String,
    signer_names: Vec<String>,
}

#[derive(Serialize, Clone, Default)]
struct ObserverView {
    error: Option<String>,
    league_lane_count: usize,
    latest_league_rating: Option<i64>,
    players: Vec<PlayerView>,
    active_games: Vec<ObservedGameView>,
    active_settles: Vec<ObservedSettleView>,
    transactions: Vec<ObservedTxView>,
    warnings: Vec<String>,
}

#[derive(Serialize, Clone)]
struct ObservedGameView {
    outpoint: String,
    pair: String,
    value: u64,
    turn: String,
    status: String,
    move_timeout: i64,
    board_rows: Vec<String>,
    move_log: Vec<String>,
}

#[derive(Serialize, Clone)]
struct ObservedSettleView {
    outpoint: String,
    pair: String,
    value: u64,
    status: String,
}

#[derive(Serialize, Clone)]
struct ObservedTxView {
    txid: String,
    input_lines: Vec<String>,
    event_lines: Vec<String>,
}

const INDEX_HTML: &str = r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <title>Chess Covenant Local Arena</title>
  <style>
    :root {
      --paper: #f5f0e6;
      --panel: #fffaf2;
      --ink: #211916;
      --muted: #6e5b47;
      --line: #d8c8ae;
      --accent: #1f6f59;
      --accent-soft: #d7ece5;
      --alert: #9d1c1c;
      --dark-square: #a56d3f;
      --light-square: #f3e3c8;
      --white-piece: #fffaf2;
      --black-piece: #24160d;
    }
    * { box-sizing: border-box; }
    body { font-family: Georgia, serif; margin: 0; background: linear-gradient(180deg, #efe6d6 0%, var(--paper) 26%, #efe9dd 100%); color: var(--ink); }
    .shell { max-width: 1440px; margin: 0 auto; padding: 24px; }
    h1, h2, h3 { margin: 0; }
    p { margin: 0; color: var(--muted); }
    .header {
      display: flex;
      justify-content: space-between;
      align-items: flex-end;
      gap: 16px;
      margin-bottom: 20px;
    }
    .header-actions { display: flex; gap: 10px; align-items: center; }
    .layout {
      display: grid;
      grid-template-columns: 320px minmax(0, 1fr) 360px;
      gap: 20px;
      align-items: start;
    }
    .stack { display: grid; gap: 16px; }
    .card {
      background: color-mix(in srgb, var(--panel) 92%, white 8%);
      border: 1px solid var(--line);
      padding: 16px;
      border-radius: 16px;
      box-shadow: 0 10px 30px rgba(57, 39, 21, 0.06);
      min-width: 0;
    }
    .card-header { display: flex; justify-content: space-between; align-items: baseline; gap: 12px; margin-bottom: 12px; }
    .card-kicker { font-size: 12px; letter-spacing: 0.08em; text-transform: uppercase; color: var(--muted); }
    .toolbar { display: flex; flex-wrap: wrap; gap: 8px; }
    .form-grid { display: grid; gap: 10px; }
    .form-row { display: grid; grid-template-columns: 1fr 1fr; gap: 8px; }
    .button-row { display: flex; flex-wrap: wrap; gap: 8px; }
    button, select, input {
      padding: 9px 11px;
      margin: 0;
      font: inherit;
      border-radius: 10px;
      border: 1px solid #cdb897;
      background: white;
    }
    button {
      cursor: pointer;
      background: #f8efe0;
    }
    button.primary {
      background: var(--accent);
      color: white;
      border-color: #1b5e4c;
    }
    button.secondary {
      background: #efe5d3;
    }
    button.warn {
      background: #8f2b21;
      color: white;
      border-color: #7d2219;
    }
    ul { margin: 0; padding-left: 18px; }
    .status-bar {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 12px;
      margin-bottom: 20px;
    }
    .error, .ok {
      min-height: 42px;
      padding: 10px 12px;
      border-radius: 12px;
      border: 1px solid var(--line);
      background: rgba(255,255,255,0.55);
    }
    .error { color: var(--alert); }
    .ok { color: #245b2a; }
    .board-card { position: relative; }
    .board-wrap { display: inline-block; border: 1px solid #8f7a5b; border-radius: 14px; overflow: hidden; background: #cfb28f; }
    .board-grid { display: grid; grid-template-columns: repeat(8, 54px); grid-template-rows: repeat(8, 54px); }
    .square {
      width: 54px;
      height: 54px;
      display: flex;
      align-items: center;
      justify-content: center;
      font-size: 34px;
      line-height: 1;
      user-select: none;
      cursor: pointer;
    }
    .light { background: var(--light-square); }
    .dark { background: var(--dark-square); }
    .piece-white { color: var(--white-piece); text-shadow: 0 1px 0 rgba(0,0,0,0.5), 0 0 8px rgba(255,248,231,0.2); }
    .piece-black { color: var(--black-piece); }
    .selected-square { box-shadow: inset 0 0 0 4px #1f6f59; }
    .target-square { box-shadow: inset 0 0 0 4px #b33b2e; }
    .board-empty {
      min-width: 446px;
      min-height: 446px;
      display: flex;
      align-items: center;
      justify-content: center;
      color: var(--muted);
      background: #f7ecda;
      border-radius: 14px;
      border: 1px dashed #cdb897;
    }
    .board-files, .board-ranks {
      display: grid;
      color: var(--muted);
      font-size: 12px;
      letter-spacing: 0.06em;
      text-transform: uppercase;
    }
    .board-files { grid-template-columns: repeat(8, 54px); margin-top: 6px; }
    .board-files span, .board-ranks span { display: flex; align-items: center; justify-content: center; }
    .board-shell { display: flex; gap: 8px; align-items: stretch; }
    .board-ranks { grid-template-rows: repeat(8, 54px); }
    .meta-grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
      gap: 10px;
      margin-top: 12px;
    }
    .meta-chip {
      background: #f4ebdd;
      border: 1px solid #e3d4bb;
      border-radius: 12px;
      padding: 10px;
    }
    .meta-chip strong { display: block; font-size: 12px; letter-spacing: 0.06em; text-transform: uppercase; color: var(--muted); margin-bottom: 4px; }
    .list { display: grid; gap: 8px; }
    .list-item {
      padding: 10px 12px;
      border-radius: 12px;
      border: 1px solid #e3d4bb;
      background: #fcf6ea;
    }
    .list-item.active {
      border-color: var(--accent);
      background: var(--accent-soft);
    }
    .list-item button {
      width: 100%;
      text-align: left;
      background: transparent;
      border: none;
      padding: 0;
    }
    .list-title { font-weight: 700; }
    .list-sub { color: var(--muted); font-size: 14px; margin-top: 3px; }
    .list-meta { color: var(--muted); font-size: 13px; margin-top: 4px; }
    .scroll {
      max-height: 360px;
      overflow: auto;
      padding-right: 4px;
    }
    .tx-block {
      padding: 10px 12px;
      border-radius: 12px;
      border: 1px solid #eadbc3;
      background: #fdf9f2;
    }
    .tx-block + .tx-block { margin-top: 8px; }
    .tx-events { margin-top: 6px; display: grid; gap: 4px; }
    .tx-inputs { margin-top: 8px; color: var(--muted); font-size: 13px; }
    .muted { color: var(--muted); }
    .pill {
      display: inline-flex;
      padding: 4px 8px;
      border-radius: 999px;
      background: #efe4d1;
      color: #654f39;
      font-size: 12px;
      letter-spacing: 0.04em;
      text-transform: uppercase;
    }
    @media (max-width: 1240px) {
      .layout { grid-template-columns: 1fr; }
      .board-card { position: relative; }
      .board-empty { min-width: 100%; }
    }
  </style>
</head>
<body>
  <div class="shell">
    <div class="header">
      <div>
        <h1>Chess Covenant Local Arena</h1>
        <p>Interactive local game UI over the shared tx-driven orchestrator and observer index.</p>
      </div>
      <div class="header-actions">
        <span class="pill">local only</span>
        <button class="secondary" onclick="resetSession()">New Session</button>
      </div>
    </div>
    <div class="status-bar">
      <div id="status" class="ok"></div>
      <div id="error" class="error"></div>
    </div>
    <div class="layout">
      <div class="stack">
        <div class="card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Setup</div>
              <h2>Session Controls</h2>
            </div>
          </div>
          <div class="button-row">
            <button onclick="act({action:'register', actor:'white'})">Register White</button>
            <button onclick="act({action:'register', actor:'black'})">Register Black</button>
          </div>
          <div class="button-row" style="margin-top: 8px;">
            <button onclick="act({action:'invite'})">Send Invite</button>
            <button onclick="act({action:'accept_invite'})">Accept Invite</button>
            <button class="primary" onclick="act({action:'start_game'})">Start Game</button>
          </div>
        </div>
        <div class="card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Moves</div>
              <h2>Play</h2>
            </div>
            <div class="muted">Click or drag pieces on the board</div>
          </div>
          <div class="form-grid">
            <div class="form-row">
              <select id="moveActor">
                <option value="white">white</option>
                <option value="black">black</option>
              </select>
              <input id="moveLabel" value="e2e4" />
            </div>
            <div class="button-row">
              <button class="primary" onclick="submitMove()">Submit Move</button>
              <button onclick="forceMove()">Force Protocol Move</button>
            </div>
            <div class="form-row">
              <select id="surrenderActor">
                <option value="white">white</option>
                <option value="black">black</option>
              </select>
              <button class="warn" onclick="submitSurrender()">Surrender</button>
            </div>
          </div>
        </div>
        <div class="card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Settlement</div>
              <h2>Resolve</h2>
            </div>
          </div>
          <div class="form-grid">
            <div class="form-row">
              <select id="settlementActor">
                <option value="white">white</option>
                <option value="black">black</option>
              </select>
              <select id="settlementResult">
                <option value="white_win">white win</option>
                <option value="black_win">black win</option>
                <option value="draw">draw</option>
              </select>
            </div>
            <div class="button-row">
              <button onclick="requestSettlement()">Request Settlement</button>
              <button class="primary" onclick="settle()">Settle</button>
              <button onclick="claimTimeout()">Claim Timeout</button>
            </div>
            <div id="settlementHint" class="muted"></div>
            <div class="form-row">
              <select id="retireActor">
                <option value="white">white</option>
                <option value="black">black</option>
              </select>
              <button onclick="retirePlayer()">Retire</button>
            </div>
          </div>
        </div>
        <div class="card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Players</div>
              <h2>Registered Players</h2>
            </div>
          </div>
          <div id="players" class="list"></div>
        </div>
      </div>

      <div class="stack">
        <div class="card board-card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Board</div>
              <h2>Current Position</h2>
            </div>
            <div id="boardSource" class="muted"></div>
          </div>
          <div id="board" class="board-empty">No active game</div>
          <div id="gameMeta" class="meta-grid"></div>
        </div>
        <div class="card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Games</div>
              <h2>Active Games</h2>
            </div>
          </div>
          <div id="observerMeta" class="muted" style="margin-bottom: 12px;"></div>
          <div id="observerGames" class="list"></div>
          <div id="observerWarnings" class="list" style="margin-top: 12px;"></div>
        </div>
      </div>

      <div class="stack">
        <div class="card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Messages</div>
              <h2>Notices</h2>
            </div>
          </div>
          <div id="notices" class="list scroll"></div>
        </div>
        <div class="card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Transactions</div>
              <h2>Tx History</h2>
            </div>
          </div>
          <div id="history" class="list scroll"></div>
        </div>
        <div class="card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Observed</div>
              <h2>Settles</h2>
            </div>
          </div>
          <div id="observerSettles" class="list"></div>
        </div>
        <div class="card">
          <div class="card-header">
            <div>
              <div class="card-kicker">Observed</div>
              <h2>Tx Events</h2>
            </div>
          </div>
          <div id="observerTxs" class="scroll"></div>
        </div>
      </div>
    </div>
  </div>
  <script>
    let latestState = null;
    let selectedSource = null;
    let hoveredTarget = null;
    let focusedGameOutpoint = null;

    async function fetchState() {
      const res = await fetch('/api/state');
      const data = await res.json();
      render(data.state);
    }
    async function act(payload) {
      document.getElementById('error').textContent = '';
      const res = await fetch('/api/action', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify(payload),
      });
      const data = await res.json();
      render(data.state);
      if (data.ok) {
        document.getElementById('status').textContent = 'Action applied';
      } else {
        document.getElementById('status').textContent = '';
        document.getElementById('error').textContent = data.error;
      }
    }
    function resetSession() {
      selectedSource = null;
      hoveredTarget = null;
      focusedGameOutpoint = null;
      act({action: 'reset_session'});
    }
    function submitMove() {
      act({
        action: 'move',
        actor: document.getElementById('moveActor').value,
        move_label: document.getElementById('moveLabel').value
      });
    }
    function forceMove() {
      act({
        action: 'force_move',
        actor: document.getElementById('moveActor').value,
        move_label: document.getElementById('moveLabel').value
      });
    }
    function submitSurrender() {
      act({
        action: 'surrender',
        actor: document.getElementById('surrenderActor').value
      });
    }
    function requestSettlement() {
      const result = currentSettlementResult(latestState) || document.getElementById('settlementResult').value;
      act({
        action: 'request_settlement',
        actor: document.getElementById('settlementActor').value,
        result
      });
    }
    function settle() {
      const result = currentSettlementResult(latestState) || document.getElementById('settlementResult').value;
      act({
        action: 'settle',
        result
      });
    }
    function claimTimeout() {
      act({
        action: 'claim_timeout',
        actor: document.getElementById('settlementActor').value
      });
    }
    function retirePlayer() {
      act({
        action: 'retire',
        actor: document.getElementById('retireActor').value
      });
    }
    function pieceAt(boardRows, square) {
      const files = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'];
      const file = files.indexOf(square[0]);
      const rank = parseInt(square[1], 10);
      if (file < 0 || !(rank >= 1 && rank <= 8)) return '.';
      return boardRows[8 - rank][file];
    }
    function pieceSide(piece) {
      if (!piece || piece === '.') return null;
      return piece === piece.toUpperCase() ? 'white' : 'black';
    }
    function isPromotionMove(boardRows, source, target) {
      const piece = pieceAt(boardRows, source);
      return (piece === 'P' && target[1] === '8') || (piece === 'p' && target[1] === '1');
    }
    function clearSelection() {
      selectedSource = null;
      hoveredTarget = null;
    }
    function beginSourceSelection(square) {
      const game = displayedGame(latestState);
      if (!game) return;
      const piece = pieceAt(game.board_rows, square);
      const side = pieceSide(piece);
      if (!side) {
        document.getElementById('status').textContent = 'Choose a source square with a piece.';
        return;
      }
      if (side !== game.turn) {
        document.getElementById('status').textContent = `It is ${game.turn}'s turn.`;
        return;
      }
      selectedSource = square;
      document.getElementById('moveActor').value = game.turn;
      document.getElementById('status').textContent = `Selected ${square}. Choose a destination square.`;
      render(latestState);
    }
    function finishSquareMove(source, target) {
      const game = displayedGame(latestState);
      if (!game) return;
      if (source === target) {
        clearSelection();
        document.getElementById('status').textContent = 'Selection cleared.';
        render(latestState);
        return;
      }
      const targetPiece = pieceAt(game.board_rows, target);
      if (pieceSide(targetPiece) === game.turn) {
        beginSourceSelection(target);
        return;
      }
      document.getElementById('moveActor').value = game.turn;
      document.getElementById('moveLabel').value = `${source}${target}`;
      clearSelection();
      render(latestState);
      if (isPromotionMove(game.board_rows, source, target)) {
        document.getElementById('status').textContent = `Move filled as ${source}${target}. Append promotion piece like q/n/r/b if needed.`;
        return;
      }
      submitMove();
    }
    function handleSquareClick(square) {
      if (!displayedGame(latestState)) return;
      if (!selectedSource) {
        beginSourceSelection(square);
        return;
      }
      finishSquareMove(selectedSource, square);
    }
    function handleSquareDragStart(event, square) {
      const game = displayedGame(latestState);
      if (!game) return;
      const piece = pieceAt(game.board_rows, square);
      if (pieceSide(piece) !== game.turn) {
        event.preventDefault();
        return;
      }
      selectedSource = square;
      hoveredTarget = null;
      event.dataTransfer.setData('text/plain', square);
      event.dataTransfer.effectAllowed = 'move';
      document.getElementById('moveActor').value = game.turn;
      document.getElementById('status').textContent = `Dragging from ${square}. Drop on a destination square.`;
    }
    function handleSquareDragOver(event, square) {
      event.preventDefault();
      event.dataTransfer.dropEffect = 'move';
      if (hoveredTarget !== square) {
        hoveredTarget = square;
        render(latestState);
      }
    }
    function handleSquareDrop(event, square) {
      event.preventDefault();
      const source = event.dataTransfer.getData('text/plain') || selectedSource;
      if (!source) return;
       hoveredTarget = null;
      finishSquareMove(source, square);
    }
    function handleSquareDragEnd() {
      hoveredTarget = null;
      render(latestState);
    }
    function selectObservedGame(outpoint) {
      focusedGameOutpoint = outpoint;
      clearSelection();
      render(latestState);
    }
    function displayedGame(state) {
      if (!state) return null;
      if (focusedGameOutpoint) {
        const selected = state.observer.active_games.find(g => g.outpoint === focusedGameOutpoint);
        if (selected) {
          return {
            source: `observed game ${selected.outpoint}`,
            phase: 'observed',
            turn: selected.turn,
            status: selected.status,
            value: selected.value,
            move_timeout: selected.move_timeout,
            board_rows: selected.board_rows,
            move_log: selected.move_log,
          };
        }
      }
      if (state.game) {
        return {
          source: 'current local game',
          phase: state.game.phase,
          turn: state.game.turn,
          status: state.game.status,
          value: state.game.value,
          move_timeout: state.game.move_timeout,
          board_rows: state.game.board_rows,
          move_log: state.game.move_log,
        };
      }
      if (state.observer.active_games.length > 0) {
        const selected = state.observer.active_games[state.observer.active_games.length - 1];
        focusedGameOutpoint = selected.outpoint;
        return {
          source: `observed game ${selected.outpoint}`,
          phase: 'observed',
          turn: selected.turn,
          status: selected.status,
          value: selected.value,
          move_timeout: selected.move_timeout,
          board_rows: selected.board_rows,
          move_log: selected.move_log,
        };
      }
      focusedGameOutpoint = null;
      return null;
    }
    function currentSettlementResult(state) {
      if (!state) return null;
      if (state.game) {
        if (state.game.status === 'white_win') return 'white_win';
        if (state.game.status === 'black_win') return 'black_win';
        if (state.game.status === 'draw') return 'draw';
      }
      const focusedSettle = focusedGameOutpoint
        ? state.observer.active_settles.find(s => s.outpoint === focusedGameOutpoint)
        : null;
      const settle = focusedSettle || state.observer.active_settles[0];
      if (!settle) return null;
      if (settle.status === 'white_win') return 'white_win';
      if (settle.status === 'black_win') return 'black_win';
      if (settle.status === 'draw') return 'draw';
      return null;
    }
    function renderBoard(boardRows) {
      const files = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'];
      const pieceGlyph = {
        'P': ['♙', 'piece-white'],
        'N': ['♘', 'piece-white'],
        'B': ['♗', 'piece-white'],
        'R': ['♖', 'piece-white'],
        'Q': ['♕', 'piece-white'],
        'K': ['♔', 'piece-white'],
        'p': ['♟', 'piece-black'],
        'n': ['♞', 'piece-black'],
        'b': ['♝', 'piece-black'],
        'r': ['♜', 'piece-black'],
        'q': ['♛', 'piece-black'],
        'k': ['♚', 'piece-black'],
      };
      const squares = [];
      for (let row = 0; row < 8; row++) {
        const rank = 8 - row;
        for (let col = 0; col < 8; col++) {
          const piece = boardRows[row][col];
          const square = `${files[col]}${rank}`;
          const tone = ((row + col) % 2 === 0) ? 'light' : 'dark';
          const glyphInfo = pieceGlyph[piece] ?? ['',''];
          const selectionClass = square === selectedSource ? 'selected-square' : '';
          const targetClass = square === hoveredTarget ? 'target-square' : '';
          squares.push(
            `<div class="square ${tone} ${glyphInfo[1]} ${selectionClass} ${targetClass}" title="${square}" draggable="${piece !== '.'}"
              onclick="handleSquareClick('${square}')"
              ondragstart="handleSquareDragStart(event, '${square}')"
              ondragend="handleSquareDragEnd()"
              ondragover="handleSquareDragOver(event, '${square}')"
              ondrop="handleSquareDrop(event, '${square}')">${glyphInfo[0]}</div>`
          );
        }
      }
      const ranks = [8,7,6,5,4,3,2,1].map(rank => `<span>${rank}</span>`).join('');
      const fileLabels = files.map(file => `<span>${file}</span>`).join('');
      return `
        <div class="board-shell">
          <div class="board-ranks">${ranks}</div>
          <div>
            <div class="board-wrap"><div class="board-grid">${squares.join('')}</div></div>
            <div class="board-files">${fileLabels}</div>
          </div>
        </div>
      `;
    }
    function render(state) {
      latestState = state;
      const game = displayedGame(state);
      document.getElementById('players').innerHTML = state.players.map(p =>
        `<div class="list-item">
          <div class="list-title">${p.role}</div>
          <div class="list-sub">${p.player_ref}</div>
          <div class="list-meta">value=${p.value ?? 'n/a'} · rating=${p.rating} · open=${p.open_games} · W/D/L=${p.wins}/${p.draws}/${p.losses}</div>
        </div>`
      ).join('');
      if (game) {
        document.getElementById('board').className = '';
        document.getElementById('board').innerHTML = renderBoard(game.board_rows);
        document.getElementById('boardSource').textContent = game.source;
        document.getElementById('gameMeta').innerHTML = [
          ['phase', game.phase],
          ['turn', game.turn],
          ['status', game.status],
          ['value', game.value ?? 'n/a'],
          ['timeout', game.move_timeout ?? 'n/a'],
          ['moves', game.move_log.length ? game.move_log.join(', ') : 'none yet'],
        ].map(([label, value]) => `<div class="meta-chip"><strong>${label}</strong>${value}</div>`).join('');
      } else {
        clearSelection();
        document.getElementById('board').className = 'board-empty';
        document.getElementById('board').textContent = 'No active game';
        document.getElementById('boardSource').textContent = '';
        document.getElementById('gameMeta').innerHTML = '';
      }
      document.getElementById('notices').innerHTML = state.notices.length
        ? state.notices.map(n => `<div class="list-item"><div class="list-sub">${n}</div></div>`).join('')
        : `<div class="list-item"><div class="list-sub">No notices yet.</div></div>`;
      document.getElementById('history').innerHTML = state.history.length
        ? state.history.map(h => `<div class="list-item"><div class="list-title">${h.recipe_name}</div><div class="list-sub">${h.signer_names.join(', ') || 'unsigned'}</div></div>`).join('')
        : `<div class="list-item"><div class="list-sub">No submitted transactions yet.</div></div>`;
      const observer = state.observer;
      const suggestedResult = currentSettlementResult(state);
      if (suggestedResult) {
        document.getElementById('settlementResult').value = suggestedResult;
        document.getElementById('settlementHint').textContent = `Current terminal result: ${suggestedResult.replace('_', ' ')}. Settle will use it by default.`;
      } else {
        document.getElementById('settlementHint').textContent = 'No terminal result yet. Standard play rejects illegal moves; use Force Protocol Move only if you want to push the broader protocol path.';
      }
      document.getElementById('observerMeta').textContent = observer.error
        ? `observer error: ${observer.error}`
        : `league lanes=${observer.league_lane_count}, base rating=${observer.latest_league_rating ?? 'n/a'}, active games=${observer.active_games.length}, active settles=${observer.active_settles.length}`;
      document.getElementById('observerGames').innerHTML = observer.active_games.length
        ? observer.active_games.map(g =>
            `<div class="list-item ${g.outpoint === focusedGameOutpoint ? 'active' : ''}">
              <button onclick="selectObservedGame('${g.outpoint}')">
                <div class="list-title">${g.pair}</div>
                <div class="list-sub">${g.outpoint}</div>
                <div class="list-meta">value=${g.value} · turn=${g.turn} · status=${g.status} · timeout=${g.move_timeout}</div>
              </button>
            </div>`
          ).join('')
        : `<div class="list-item"><div class="list-sub">No active games.</div></div>`;
      document.getElementById('observerWarnings').innerHTML = observer.warnings.length
        ? observer.warnings.map(w => `<div class="list-item"><div class="list-sub">${w}</div></div>`).join('')
        : '';
      document.getElementById('observerSettles').innerHTML = observer.active_settles.length
        ? observer.active_settles.map(s =>
            `<div class="list-item">
              <div class="list-title">${s.pair}</div>
              <div class="list-sub">${s.outpoint}</div>
              <div class="list-meta">value=${s.value} · status=${s.status}</div>
            </div>`
          ).join('')
        : `<div class="list-item"><div class="list-sub">No active settles.</div></div>`;
      document.getElementById('observerTxs').innerHTML = observer.transactions.length
        ? observer.transactions.map(tx => {
            const events = tx.event_lines.length
              ? tx.event_lines.map(line => `<div>${line}</div>`).join('')
              : '<div class="muted">no high-level events</div>';
            const inputs = tx.input_lines.length
              ? tx.input_lines.map(line => `<div>${line}</div>`).join('')
              : '<div class="muted">no observed inputs</div>';
            return `<div class="tx-block"><div class="list-title">${tx.txid}</div><div class="tx-events">${events}</div><div class="tx-inputs">${inputs}</div></div>`;
          }).join('')
        : `<div class="list-item"><div class="list-sub">No observed transactions yet.</div></div>`;
    }
    fetchState();
  </script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_can_drive_a_short_local_game() {
        let mut app = LocalWebController::new().expect("controller builds");
        app.handle_action(ActionRequest { action: "register".into(), actor: Some("white".into()), move_label: None, result: None })
            .expect("white registers");
        app.handle_action(ActionRequest { action: "register".into(), actor: Some("black".into()), move_label: None, result: None })
            .expect("black registers");
        app.handle_action(ActionRequest { action: "invite".into(), actor: None, move_label: None, result: None })
            .expect("invite works");
        app.handle_action(ActionRequest { action: "accept_invite".into(), actor: None, move_label: None, result: None })
            .expect("accept works");
        app.handle_action(ActionRequest { action: "start_game".into(), actor: None, move_label: None, result: None })
            .expect("start works");
        app.handle_action(ActionRequest {
            action: "move".into(),
            actor: Some("white".into()),
            move_label: Some("e2e4".into()),
            result: None,
        })
        .expect("white move works");
        app.handle_action(ActionRequest {
            action: "move".into(),
            actor: Some("black".into()),
            move_label: Some("g8f6".into()),
            result: None,
        })
        .expect("black move works");
        app.handle_action(ActionRequest {
            action: "move".into(),
            actor: Some("white".into()),
            move_label: Some("f1c4".into()),
            result: None,
        })
        .expect("white bishop move works");
        app.handle_action(ActionRequest { action: "surrender".into(), actor: Some("black".into()), move_label: None, result: None })
            .expect("surrender works");
        app.handle_action(ActionRequest {
            action: "request_settlement".into(),
            actor: Some("black".into()),
            move_label: None,
            result: Some("white_win".into()),
        })
        .expect("request works");
        app.handle_action(ActionRequest { action: "settle".into(), actor: None, move_label: None, result: Some("white_win".into()) })
            .expect("settle works");

        let snapshot = app.snapshot();
        assert!(snapshot.game.is_none());
        assert!(snapshot.notices.iter().any(|n| n.contains("e2e4")));
        assert!(snapshot.notices.iter().any(|n| n.contains("settlement complete")));
        assert!(snapshot.observer.error.is_none());
        assert_eq!(snapshot.observer.league_lane_count, 1);
        assert!(snapshot.observer.transactions.iter().any(|tx| tx.event_lines.iter().any(|line| line.contains("settlement applied"))));
    }

    #[test]
    fn parses_promotion_move_label() {
        let mv = parse_move_label("e7e8q").expect("promotion parses");
        assert_eq!(mv.from_x, 4);
        assert_eq!(mv.from_y, 6);
        assert_eq!(mv.to_x, 4);
        assert_eq!(mv.to_y, 7);
        assert_eq!(mv.promo_piece, 5);
    }

    #[test]
    fn reset_session_clears_local_state() {
        let mut app = LocalWebController::new().expect("controller builds");
        app.handle_action(ActionRequest { action: "register".into(), actor: Some("white".into()), move_label: None, result: None })
            .expect("white registers");
        app.handle_action(ActionRequest { action: "register".into(), actor: Some("black".into()), move_label: None, result: None })
            .expect("black registers");
        app.handle_action(ActionRequest { action: "start_game".into(), actor: None, move_label: None, result: None })
            .expect("game starts");

        app.handle_action(ActionRequest { action: "reset_session".into(), actor: None, move_label: None, result: None })
            .expect("reset works");

        let snapshot = app.snapshot();
        assert!(snapshot.game.is_none());
        assert!(snapshot.history.is_empty());
        assert!(snapshot.players.iter().any(|player| !player.registered));
    }

    #[test]
    fn settlement_snapshot_updates_player_ratings() {
        let mut app = LocalWebController::new().expect("controller builds");
        app.handle_action(ActionRequest { action: "register".into(), actor: Some("white".into()), move_label: None, result: None })
            .expect("white registers");
        app.handle_action(ActionRequest { action: "register".into(), actor: Some("black".into()), move_label: None, result: None })
            .expect("black registers");
        app.handle_action(ActionRequest { action: "invite".into(), actor: None, move_label: None, result: None })
            .expect("invite works");
        app.handle_action(ActionRequest { action: "accept_invite".into(), actor: None, move_label: None, result: None })
            .expect("accept works");
        app.handle_action(ActionRequest { action: "start_game".into(), actor: None, move_label: None, result: None })
            .expect("start works");
        app.handle_action(ActionRequest {
            action: "move".into(),
            actor: Some("white".into()),
            move_label: Some("e2e4".into()),
            result: None,
        })
        .expect("white move works");
        app.handle_action(ActionRequest { action: "surrender".into(), actor: Some("black".into()), move_label: None, result: None })
            .expect("surrender works");
        app.handle_action(ActionRequest {
            action: "request_settlement".into(),
            actor: Some("black".into()),
            move_label: None,
            result: Some("white_win".into()),
        })
        .expect("request works");
        app.handle_action(ActionRequest { action: "settle".into(), actor: None, move_label: None, result: Some("white_win".into()) })
            .expect("settle works");

        let snapshot = app.snapshot();
        let white = snapshot.players.iter().find(|player| player.role == "white").expect("white player in snapshot");
        let black = snapshot.players.iter().find(|player| player.role == "black").expect("black player in snapshot");
        assert_eq!(white.rating, 1216);
        assert_eq!(black.rating, 1184);
    }

    #[test]
    fn surrender_can_settle_without_manual_request() {
        let mut app = LocalWebController::new().expect("controller builds");
        app.handle_action(ActionRequest { action: "register".into(), actor: Some("white".into()), move_label: None, result: None })
            .expect("white registers");
        app.handle_action(ActionRequest { action: "register".into(), actor: Some("black".into()), move_label: None, result: None })
            .expect("black registers");
        app.handle_action(ActionRequest { action: "invite".into(), actor: None, move_label: None, result: None })
            .expect("invite works");
        app.handle_action(ActionRequest { action: "accept_invite".into(), actor: None, move_label: None, result: None })
            .expect("accept works");
        app.handle_action(ActionRequest { action: "start_game".into(), actor: None, move_label: None, result: None })
            .expect("start works");
        app.handle_action(ActionRequest {
            action: "move".into(),
            actor: Some("white".into()),
            move_label: Some("e2e4".into()),
            result: None,
        })
        .expect("white move works");
        app.handle_action(ActionRequest { action: "surrender".into(), actor: Some("black".into()), move_label: None, result: None })
            .expect("surrender works");
        app.handle_action(ActionRequest { action: "settle".into(), actor: None, move_label: None, result: Some("white_win".into()) })
            .expect("settle works immediately");

        let snapshot = app.snapshot();
        assert!(snapshot.game.is_none());
        assert!(snapshot.notices.iter().any(|notice| notice.contains("settlement complete")));
    }
}

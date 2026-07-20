use std::fmt::Write as _;
use std::str::FromStr;

use cozy_chess::{Board, Color, Move, Piece, Square};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CozyMoveSpec {
    pub from_x: i64,
    pub from_y: i64,
    pub to_x: i64,
    pub to_y: i64,
    pub promo_piece: i64,
}

impl CozyMoveSpec {
    pub fn new(from_x: i64, from_y: i64, to_x: i64, to_y: i64) -> Self {
        Self { from_x, from_y, to_x, to_y, promo_piece: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CozyState {
    pub board: Vec<u8>,
    pub turn: i64,
    pub castle_rights: [u8; 4],
    pub en_passant_idx: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CozyTransition {
    pub board: Vec<u8>,
    pub turn: i64,
    pub castle_rights: [u8; 4],
    pub en_passant_idx: i64,
    pub recent_castle: i64,
}

pub type StandardMoveSpec = CozyMoveSpec;
pub type StandardState = CozyState;
pub type StandardTransition = CozyTransition;
pub type StandardMoveError = CozyBridgeError;

#[derive(Debug, Error)]
pub enum CozyBridgeError {
    #[error("board must contain exactly 64 squares, got {0}")]
    InvalidBoardLen(usize),
    #[error("unsupported piece encoding 0x{0:02x}")]
    InvalidPiece(u8),
    #[error("move coordinates must stay on board")]
    InvalidCoordinates,
    #[error("promotion piece {0} is not supported")]
    InvalidPromotion(i64),
    #[error("failed to parse cozy-chess board from generated FEN: {0}")]
    InvalidFen(String),
    #[error("move {0} is not legal in the generated cozy-chess position")]
    IllegalMove(String),
    #[error("cozy-chess position is missing a piece on square {0}")]
    MissingPiece(String),
}

pub fn apply_standard_move(state: &StandardState, mv: StandardMoveSpec) -> Result<StandardTransition, StandardMoveError> {
    apply_move_with_cozy(state, mv)
}

pub fn apply_move_with_cozy(state: &CozyState, mv: CozyMoveSpec) -> Result<CozyTransition, CozyBridgeError> {
    let mut board = board_from_state(state)?;
    let chess_move = cozy_move(mv)?;
    let recent_castle = detect_recent_castle(state, mv)?;
    if !board.is_legal(chess_move) {
        return Err(CozyBridgeError::IllegalMove(move_label(mv)));
    }
    board.play(chess_move);
    Ok(CozyTransition {
        board: encode_board(&board)?,
        turn: color_to_turn(board.side_to_move()),
        castle_rights: encode_castle_rights(&board),
        en_passant_idx: encode_en_passant(&board),
        recent_castle,
    })
}

fn board_from_state(state: &CozyState) -> Result<Board, CozyBridgeError> {
    if state.board.len() != 64 {
        return Err(CozyBridgeError::InvalidBoardLen(state.board.len()));
    }
    let fen = state_to_fen(state)?;
    Board::from_str(&fen).map_err(|err| CozyBridgeError::InvalidFen(err.to_string()))
}

fn state_to_fen(state: &CozyState) -> Result<String, CozyBridgeError> {
    let mut fen = String::new();
    for rank in (0..8).rev() {
        let mut empty_run = 0;
        for file in 0..8 {
            let piece = state.board[(rank * 8 + file) as usize];
            if piece == 0 {
                empty_run += 1;
                continue;
            }
            if empty_run > 0 {
                let _ = write!(fen, "{empty_run}");
                empty_run = 0;
            }
            fen.push(piece_to_fen(piece)?);
        }
        if empty_run > 0 {
            let _ = write!(fen, "{empty_run}");
        }
        if rank > 0 {
            fen.push('/');
        }
    }

    fen.push(' ');
    fen.push(if state.turn == 0 { 'w' } else { 'b' });
    fen.push(' ');

    let mut castles = String::new();
    if state.castle_rights[0] == 1 {
        castles.push('K');
    }
    if state.castle_rights[1] == 1 {
        castles.push('Q');
    }
    if state.castle_rights[2] == 1 {
        castles.push('k');
    }
    if state.castle_rights[3] == 1 {
        castles.push('q');
    }
    if castles.is_empty() {
        castles.push('-');
    }
    fen.push_str(&castles);
    fen.push(' ');

    if state.en_passant_idx >= 0 {
        fen.push_str(&square_name(square_from_idx(state.en_passant_idx)?));
    } else {
        fen.push('-');
    }

    // Halfmove/fullmove are not modeled in the covenant state; keep the FEN tail deterministic.
    fen.push_str(" 0 1");
    Ok(fen)
}

fn encode_board(board: &Board) -> Result<Vec<u8>, CozyBridgeError> {
    let mut out = vec![0u8; 64];
    for idx in 0..64 {
        let square = square_from_idx(idx)?;
        let Some(piece) = board.piece_on(square) else {
            continue;
        };
        let Some(color) = board.color_on(square) else {
            return Err(CozyBridgeError::MissingPiece(square_name(square)));
        };
        out[idx as usize] = encode_piece(piece, color);
    }
    Ok(out)
}

fn encode_castle_rights(board: &Board) -> [u8; 4] {
    [
        if board.castle_rights(Color::White).short.is_some() { 1 } else { 0 },
        if board.castle_rights(Color::White).long.is_some() { 1 } else { 0 },
        if board.castle_rights(Color::Black).short.is_some() { 1 } else { 0 },
        if board.castle_rights(Color::Black).long.is_some() { 1 } else { 0 },
    ]
}

fn encode_en_passant(board: &Board) -> i64 {
    board
        .en_passant()
        .map(|file| {
            let file_name = file.to_string();
            let file_idx = i64::from(file_name.as_bytes()[0] - b'a');
            let rank_idx = if board.side_to_move() == Color::Black { 2 } else { 5 };
            square_idx(file_idx, rank_idx)
        })
        .unwrap_or(-1)
}

fn detect_recent_castle(state: &CozyState, mv: CozyMoveSpec) -> Result<i64, CozyBridgeError> {
    let piece = *state.board.get(square_idx(mv.from_x, mv.from_y) as usize).ok_or(CozyBridgeError::InvalidCoordinates)?;
    if piece != 0x06 && piece != 0x0e {
        return Ok(0);
    }
    if mv.from_y != mv.to_y || (mv.to_x - mv.from_x).abs() != 2 {
        return Ok(0);
    }
    Ok(match (piece, mv.to_x > mv.from_x) {
        (0x06, true) => 1,
        (0x06, false) => 2,
        (0x0e, true) => 3,
        (0x0e, false) => 4,
        _ => 0,
    })
}

fn cozy_move(mv: CozyMoveSpec) -> Result<Move, CozyBridgeError> {
    let mut to_x = mv.to_x;
    if (mv.to_x - mv.from_x).abs() == 2 && mv.from_y == mv.to_y {
        to_x = if mv.to_x > mv.from_x { 7 } else { 0 };
    }
    Ok(Move {
        from: square_from_coords(mv.from_x, mv.from_y)?,
        to: square_from_coords(to_x, mv.to_y)?,
        promotion: promotion_piece(mv.promo_piece)?,
    })
}

fn square_from_coords(x: i64, y: i64) -> Result<Square, CozyBridgeError> {
    if !(0..8).contains(&x) || !(0..8).contains(&y) {
        return Err(CozyBridgeError::InvalidCoordinates);
    }
    square_from_idx(square_idx(x, y))
}

fn square_from_idx(idx: i64) -> Result<Square, CozyBridgeError> {
    if !(0..64).contains(&idx) {
        return Err(CozyBridgeError::InvalidCoordinates);
    }
    Square::from_str(&square_name_from_idx(idx)).map_err(|_| CozyBridgeError::InvalidCoordinates)
}

fn square_idx(x: i64, y: i64) -> i64 {
    y * 8 + x
}

fn square_name(square: Square) -> String {
    square.to_string()
}

fn square_name_from_idx(idx: i64) -> String {
    let file = (b'a' + (idx % 8) as u8) as char;
    let rank = (b'1' + (idx / 8) as u8) as char;
    format!("{file}{rank}")
}

fn piece_to_fen(piece: u8) -> Result<char, CozyBridgeError> {
    Ok(match piece {
        0x01 => 'P',
        0x02 => 'N',
        0x03 => 'B',
        0x04 => 'R',
        0x05 => 'Q',
        0x06 => 'K',
        0x09 => 'p',
        0x0a => 'n',
        0x0b => 'b',
        0x0c => 'r',
        0x0d => 'q',
        0x0e => 'k',
        _ => return Err(CozyBridgeError::InvalidPiece(piece)),
    })
}

fn encode_piece(piece: Piece, color: Color) -> u8 {
    match (piece, color) {
        (Piece::Pawn, Color::White) => 0x01,
        (Piece::Knight, Color::White) => 0x02,
        (Piece::Bishop, Color::White) => 0x03,
        (Piece::Rook, Color::White) => 0x04,
        (Piece::Queen, Color::White) => 0x05,
        (Piece::King, Color::White) => 0x06,
        (Piece::Pawn, Color::Black) => 0x09,
        (Piece::Knight, Color::Black) => 0x0a,
        (Piece::Bishop, Color::Black) => 0x0b,
        (Piece::Rook, Color::Black) => 0x0c,
        (Piece::Queen, Color::Black) => 0x0d,
        (Piece::King, Color::Black) => 0x0e,
    }
}

fn promotion_piece(promo_piece: i64) -> Result<Option<Piece>, CozyBridgeError> {
    Ok(match promo_piece {
        0 => None,
        2 => Some(Piece::Knight),
        3 => Some(Piece::Bishop),
        4 => Some(Piece::Rook),
        5 => Some(Piece::Queen),
        _ => return Err(CozyBridgeError::InvalidPromotion(promo_piece)),
    })
}

fn color_to_turn(color: Color) -> i64 {
    match color {
        Color::White => 0,
        Color::Black => 1,
    }
}

fn move_label(mv: CozyMoveSpec) -> String {
    format!(
        "{}{}{}{}{}",
        (b'a' + mv.from_x as u8) as char,
        mv.from_y + 1,
        (b'a' + mv.to_x as u8) as char,
        mv.to_y + 1,
        if mv.promo_piece == 0 { String::new() } else { mv.promo_piece.to_string() }
    )
}

#[cfg(test)]
mod tests {
    use super::{apply_move_with_cozy, CozyMoveSpec, CozyState};

    fn standard_board() -> Vec<u8> {
        vec![
            0x04, 0x02, 0x03, 0x05, 0x06, 0x03, 0x02, 0x04, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x0c, 0x0a, 0x0b, 0x0d,
            0x0e, 0x0b, 0x0a, 0x0c,
        ]
    }

    #[test]
    fn cozy_poc_handles_e2e4() {
        let state = CozyState { board: standard_board(), turn: 0, castle_rights: [1, 1, 1, 1], en_passant_idx: -1 };
        let next = apply_move_with_cozy(&state, CozyMoveSpec::new(4, 1, 4, 3)).expect("e2e4 should be legal");
        assert_eq!(next.turn, 1);
        assert_eq!(next.en_passant_idx, 20);
        assert_eq!(next.recent_castle, 0);
        assert_eq!(next.board[12], 0x00);
        assert_eq!(next.board[28], 0x01);
        assert_eq!(next.castle_rights, [1, 1, 1, 1]);
    }

    #[test]
    fn cozy_poc_handles_white_kingside_castle() {
        let mut board = standard_board();
        board[5] = 0x00;
        board[6] = 0x00;
        let state = CozyState { board, turn: 0, castle_rights: [1, 1, 1, 1], en_passant_idx: -1 };
        let next = apply_move_with_cozy(&state, CozyMoveSpec::new(4, 0, 6, 0)).expect("white kingside castle should be legal");
        assert_eq!(next.turn, 1);
        assert_eq!(next.recent_castle, 1);
        assert_eq!(next.board[4], 0x00);
        assert_eq!(next.board[5], 0x04);
        assert_eq!(next.board[6], 0x06);
        assert_eq!(next.board[7], 0x00);
        assert_eq!(next.castle_rights, [0, 0, 1, 1]);
    }
}

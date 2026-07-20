use thiserror::Error;

use crate::cozy_bridge::{apply_standard_move, StandardMoveError, StandardMoveSpec, StandardState, StandardTransition};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolMoveSpec {
    pub from_x: i64,
    pub from_y: i64,
    pub to_x: i64,
    pub to_y: i64,
    pub promo_piece: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolState {
    pub board: Vec<u8>,
    pub turn: i64,
    pub castle_rights: [u8; 4],
    pub en_passant_idx: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolTransition {
    pub board: Vec<u8>,
    pub turn: i64,
    pub castle_rights: [u8; 4],
    pub en_passant_idx: i64,
    pub recent_castle: i64,
}

#[derive(Debug, Error)]
pub enum ProtocolMoveError {
    #[error("board must contain exactly 64 squares, got {0}")]
    InvalidBoardLen(usize),
    #[error("move coordinates must stay on board")]
    InvalidCoordinates,
    #[error("no piece on source square")]
    MissingSourcePiece,
    #[error("{0}")]
    Standard(#[from] StandardMoveError),
}

pub fn apply_standard_chess_move(state: &ProtocolState, mv: ProtocolMoveSpec) -> Result<ProtocolTransition, ProtocolMoveError> {
    let next = apply_standard_move(
        &StandardState {
            board: state.board.clone(),
            turn: state.turn,
            castle_rights: state.castle_rights,
            en_passant_idx: state.en_passant_idx,
        },
        StandardMoveSpec { from_x: mv.from_x, from_y: mv.from_y, to_x: mv.to_x, to_y: mv.to_y, promo_piece: mv.promo_piece },
    )?;
    Ok(from_standard_transition(next))
}

pub fn apply_protocol_move(state: &ProtocolState, mv: ProtocolMoveSpec) -> Result<ProtocolTransition, ProtocolMoveError> {
    match apply_standard_chess_move(state, mv) {
        Ok(next) => Ok(next),
        Err(ProtocolMoveError::Standard(
            StandardMoveError::InvalidFen(_) | StandardMoveError::IllegalMove(_) | StandardMoveError::MissingPiece(_),
        )) => apply_sil_protocol_move(state, mv),
        Err(err) => Err(err),
    }
}

fn from_standard_transition(next: StandardTransition) -> ProtocolTransition {
    ProtocolTransition {
        board: next.board,
        turn: next.turn,
        castle_rights: next.castle_rights,
        en_passant_idx: next.en_passant_idx,
        recent_castle: next.recent_castle,
    }
}

// Mirrors the covenant's broader protocol semantics for states that are not standard-chess positions.
// This keeps forced illegal moves, king capture modeling, and raw worker-oriented board transitions
// available without making them part of the default standard-chess path.
fn apply_sil_protocol_move(state: &ProtocolState, mv: ProtocolMoveSpec) -> Result<ProtocolTransition, ProtocolMoveError> {
    if state.board.len() != 64 {
        return Err(ProtocolMoveError::InvalidBoardLen(state.board.len()));
    }
    ensure_on_board(mv.from_x, mv.from_y)?;
    ensure_on_board(mv.to_x, mv.to_y)?;

    let mut board = state.board.clone();
    let from_idx = square_idx(mv.from_x, mv.from_y) as usize;
    let to_idx = square_idx(mv.to_x, mv.to_y) as usize;
    let piece = board[from_idx];
    if piece == 0 {
        return Err(ProtocolMoveError::MissingSourcePiece);
    }

    let base_piece = if piece > 8 { piece - 8 } else { piece };
    let is_black = piece > 8;
    let mut castle_rights = state.castle_rights;
    let mut en_passant_idx = -1;
    let mut recent_castle = 0;

    // Any move that lands on an original rook square consumes that side's matching castle right.
    clear_castle_rights_for_square(&mut castle_rights, mv.to_x, mv.to_y);
    // King motion clears both castle rights for the moving side even if the move is otherwise non-standard.
    if base_piece == 6 {
        if is_black {
            castle_rights[2] = 0;
            castle_rights[3] = 0;
        } else {
            castle_rights[0] = 0;
            castle_rights[1] = 0;
        }
    }
    // Rook motion from an original corner clears only the relevant side-specific castle right.
    if base_piece == 4 {
        clear_castle_rights_for_square(&mut castle_rights, mv.from_x, mv.from_y);
    }

    if base_piece == 1 {
        let direction = if is_black { -1 } else { 1 };
        // The protocol preserves SIL-style en passant capture semantics directly from board bytes.
        if mv.from_x != mv.to_x && board[to_idx] == 0 && state.en_passant_idx == square_idx(mv.to_x, mv.to_y) {
            let captured_y = mv.to_y - direction;
            board[square_idx(mv.to_x, captured_y) as usize] = 0;
        }
        board[from_idx] = 0;
        let mut placed_piece = piece;
        // Promotion writes the chosen piece directly without re-checking standard legality.
        if mv.promo_piece != 0 {
            placed_piece = if is_black { (mv.promo_piece as u8) + 8 } else { mv.promo_piece as u8 };
        }
        board[to_idx] = placed_piece;
        // A two-step pawn move exposes the passed-over square as the next en passant target.
        if mv.from_x == mv.to_x && (mv.to_y - mv.from_y).abs() == 2 {
            en_passant_idx = square_idx(mv.from_x, mv.from_y + direction);
        }
    } else if base_piece == 6 && (mv.to_x - mv.from_x).abs() == 2 && mv.from_y == mv.to_y {
        // Castling is materialized explicitly because the protocol also tracks recent_castle for follow-up routes.
        board[from_idx] = 0;
        board[to_idx] = piece;
        if mv.to_x > mv.from_x {
            move_piece(&mut board, 7, mv.from_y as usize, 5, mv.from_y as usize);
            recent_castle = if is_black { 3 } else { 1 };
        } else {
            move_piece(&mut board, 0, mv.from_y as usize, 3, mv.from_y as usize);
            recent_castle = if is_black { 4 } else { 2 };
        }
    } else {
        // All other protocol moves are simple byte-level piece transfers, even for non-standard positions.
        move_piece(&mut board, mv.from_x as usize, mv.from_y as usize, mv.to_x as usize, mv.to_y as usize);
    }

    Ok(ProtocolTransition { board, turn: 1 - state.turn, castle_rights, en_passant_idx, recent_castle })
}

fn ensure_on_board(x: i64, y: i64) -> Result<(), ProtocolMoveError> {
    if !(0..8).contains(&x) || !(0..8).contains(&y) {
        return Err(ProtocolMoveError::InvalidCoordinates);
    }
    Ok(())
}

fn square_idx(x: i64, y: i64) -> i64 {
    y * 8 + x
}

fn move_piece(board: &mut [u8], from_x: usize, from_y: usize, to_x: usize, to_y: usize) {
    let from_idx = from_y * 8 + from_x;
    let to_idx = to_y * 8 + to_x;
    board[to_idx] = board[from_idx];
    board[from_idx] = 0;
}

fn clear_castle_rights_for_square(castle_rights: &mut [u8; 4], x: i64, y: i64) {
    match (x, y) {
        (7, 0) => castle_rights[0] = 0,
        (0, 0) => castle_rights[1] = 0,
        (7, 7) => castle_rights[2] = 0,
        (0, 7) => castle_rights[3] = 0,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_protocol_move, ProtocolMoveSpec, ProtocolState};

    #[test]
    fn protocol_move_uses_standard_engine_for_castle() {
        let mut board = vec![0u8; 64];
        board[4] = 0x06;
        board[7] = 0x04;
        board[60] = 0x0e;
        let next = apply_protocol_move(
            &ProtocolState { board, turn: 0, castle_rights: [1, 0, 0, 0], en_passant_idx: -1 },
            ProtocolMoveSpec { from_x: 4, from_y: 0, to_x: 6, to_y: 0, promo_piece: 0 },
        )
        .expect("castle should work");
        assert_eq!(next.recent_castle, 1);
        assert_eq!(next.castle_rights, [0, 0, 0, 0]);
        assert_eq!(next.board[6], 0x06);
        assert_eq!(next.board[5], 0x04);
    }

    #[test]
    fn protocol_move_keeps_nonstandard_king_capture_available() {
        let mut board = vec![0u8; 64];
        board[4] = 0x06;
        board[60] = 0x0e;
        board[52] = 0x05;
        let next = apply_protocol_move(
            &ProtocolState { board, turn: 0, castle_rights: [0, 0, 0, 0], en_passant_idx: -1 },
            ProtocolMoveSpec { from_x: 4, from_y: 6, to_x: 4, to_y: 7, promo_piece: 0 },
        )
        .expect("protocol move should preserve king-capture modeling");
        assert_eq!(next.board[60], 0x05);
    }
}

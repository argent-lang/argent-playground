# Chess Rules Coverage

This document tracks how the current mux/worker chess protocol relates to
classical chess rules.

It is an audit document, not a promise that every row is already complete.

## Status Legend

- `bounded`: enforced directly by the active mux or worker spend
- `challenge`: enforceable by the opponent through an explicit protocol path
- `partial`: some support exists, but not full classical enforcement yet
- `missing`: no complete on-chain protocol path yet

## Coverage Matrix

| Rule Family | Current Status | Current Mechanism | Evidence |
| --- | --- | --- | --- |
| Side to move authorization | bounded | `ChessMux` authenticates the current signer before routing | `muxed_chess_routes_all_move_families` |
| Piece geometry by move family | bounded | each worker validates only its own move family | `muxed_chess_routes_all_move_families` |
| Slider path emptiness | bounded | vertical, horizontal, and diagonal workers scan the bounded lane | `muxed_chess_routes_all_move_families`, `pawn_double_step_blocked_by_occupied_middle_square_fails` |
| Pawn promotion choice | bounded | pawn worker requires a promotion piece only on the back rank | `pawn_underpromotion_to_knight_succeeds`, `pawn_promotion_requires_choice`, `non_promotion_pawn_move_rejects_promotion_choice` |
| En passant | bounded | pawn worker tracks and consumes `en_passant_idx` | `white_en_passant_capture_succeeds`, `black_en_passant_capture_succeeds`, `expired_en_passant_attempt_fails` |
| Castling structure | bounded | castle worker checks home square, rights bit, corner rook, empty lane | `ordinary_reply_after_castle_clears_recent_castle` |
| Castling through / into attack | challenge | `recent_castle` plus castle-challenge prep rewrites a proof board and forwards into an ordinary worker | `castle_start_square_challenge_by_pawn_succeeds`, `castle_transit_square_challenge_by_rook_succeeds`, `castle_destination_square_challenge_by_rook_succeeds`, `white_queenside_castle_destination_challenge_succeeds`, `black_kingside_castle_start_challenge_by_pawn_succeeds`, `black_queenside_castle_transit_challenge_by_rook_succeeds` |
| Draw negotiation flow | partial | custom two-phase draw dispute reuses ordinary workers and mux timeout | `claim_draw_flips_turn_and_enters_draw_state`, `knight_draw_negotiation_flips_side_control_and_false_claim_loses`, `draw_mode_reuses_ordinary_workers`, `draw_mode_disallows_castle_and_castle_challenge_routes` |
| Draw by agreement | bounded | a move can set `termination_action = 1`; `ChessMux.terminate` accepts the offer with `termination_action = 4`; an ordinary reply rejects it | `argent_mux_executes_claim_surrender_and_draw_acceptance`, `argent_draw_offer_survives_an_ordinary_worker_round_trip` |
| Surrender / resignation | bounded | `ChessMux.terminate` with `termination_action = 3` emits terminal mux state for the conceding side | `argent_mux_executes_claim_surrender_and_draw_acceptance`, `surrender_can_settle_without_manual_request` |
| Timeout / liveness | bounded | mux timeout is opponent-signed, worker timeout is permissionless | `knight_worker_timeout_rescues_invalid_committed_state` |
| Terminal win by king capture | bounded | workers set terminal status when the enemy king is captured | `capturing_enemy_king_sets_terminal_status`, `knight_draw_capture_awards_win_to_the_actor`, `pawn_draw_capture_awards_win_to_the_actor` |
| Ordinary no-self-check / must-answer-check semantics | partial | representative tx tests show that ignored check, pinned-piece exposure, king walks into attack, and illegal double-check replies can collapse into punishable next-ply king capture; no known gap is currently identified in this reduction | `ignoring_single_check_is_punishable_by_next_ply_king_capture`, `moving_a_pinned_piece_is_punishable_by_next_ply_king_capture`, `king_move_into_attack_is_punishable_by_next_ply_king_capture`, `legal_interposition_blocks_the_immediate_king_capture_route`, `illegal_double_check_reply_is_punishable_by_next_ply_king_capture` |
| Checkmate reduction to forced king capture | partial | mate does not need a separate eager terminal predicate here; the intended reduction is surrender or a reply after which the king is capturable, but that reduction is not yet covered end-to-end in theorem-style tx tests | no direct tx coverage yet |
| Stalemate as a direct terminal proof | missing | no dedicated on-chain stalemate path yet | none |
| Threefold repetition | missing | no repetition state or proof path yet | none |
| Fifty-move rule | missing | no half-move clock state or proof path yet | none |
| Insufficient material draw | missing | no dedicated state or proof path yet | none |
| Value settlement on win / draw | bounded | `ChessSettle` pays the stake to the winner or splits a draw, then emits two spendable `Player` outputs | `argent_game_settles_back_into_spendable_players`, `terminal_mux_settles_white_win_back_into_players`, `terminal_mux_settles_black_win_back_into_players`, `terminal_mux_settles_draw_back_into_players` |

## Immediate Gaps

The highest-value unresolved areas are:

- rare draw rules such as repetition and the fifty-move rule
- termination semantics beyond king capture, timeout, draw by agreement, and accepted draw claim
- production hardening of settlement and rating rules

## Expected Maintenance

When rule support changes, this file should move the relevant row from
`partial` or `missing` into `bounded` or `challenge`, and update the evidence
column to point at the concrete transaction tests that prove it.

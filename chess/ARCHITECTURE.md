# Chess Covenant Architecture

This document is about the **inner mux/worker game engine only**.

It describes the `ChessMux` plus move-worker family, their shared game-state
layout, and the bounded-verification philosophy behind that split.

It does **not** try to describe the newer outer durable layer
`League -> Player -> ChessMux -> ChessSettle`. Those notes now live in the
book under `book/`.

This example uses a **multiplexer pattern**.

Chess is too large and too entangled to force through one giant covenant. The
protocol is therefore split into:

- one canonical checkpoint contract: `ChessMux`
- many small worker contracts for bounded move families and challenge flows

The point is not to minimize transaction count. The point is to make the state
machine modular enough to compile, reason about, and extend.

## Core Pattern

`ChessMux` is the durable owner of game state.

Workers are transient validators. A move is routed through mux into the
relevant worker, the worker checks one bounded claim, and control returns to
mux.

In concrete terms:

1. mux authenticates the side to move
2. mux uses the typed `MoveActor` target as the worker choice
3. mux commits the pending move into shared state when a worker route is chosen
4. the worker proves one bounded rule and rewrites the board
5. the worker returns to mux with cleared pending fields

Within the inner game engine, all mux and move-worker contracts share the same
serialized state layout. That is what lets them behave like one split protocol
instead of unrelated scripts.

## Current On-Chain Semantics

The current protocol is not yet “full classical chess legality in one step”.

What it enforces directly:

- side-to-move authorization at mux
- bounded move geometry inside the selected worker
- local path emptiness for sliders
- pawn promotion and en passant mechanics
- castling structure plus a dedicated post-castle challenge path
- timeout-based liveness

How games currently end on chain:

- king capture
- draw by agreement
- surrender
- timeout
- accepted draw claim

That means ordinary play is still adversarial in spirit. A mover proves the
bounded local move now; harder global objections are either deferred into a
challenge flow or left to off-chain policy.

## Shared State

All mux and worker contracts use the same serialized state fields:

- `board`: the 8x8 board as 64 piece codes
- `turn`: `0` white to move, `1` black to move
- `status`: `0` live, `1` white win, `2` black win, `3` draw
- `castle_rights`: four historical eligibility bits in order `white K`, `white Q`, `black K`, `black Q`
- `en_passant_idx`: en passant target square, or `-1`
- `pending_src_idx`, `pending_dst_idx`, `pending_promo`: the move committed by mux for the next worker
- `recent_castle`: `0` none, `1` white king-side, `2` white queen-side, `3` black king-side, `4` black queen-side
- `draw_state`: `3` normal play, `1` draw claimed, `2` counterplay step, `4` white offered draw, `5` black offered draw

`castle_rights` are not a complete proof that castling is legal. They preserve
historical eligibility bits, while the castle worker still checks the live board
for the correct corner rook and an empty lane.

## Why This Resembles Taproot

This pattern simulates something like a stateful version of Taproot branch
selection.

The protocol commits to many possible execution templates, but only one branch
is exercised for a given move. In that sense, mux plus the generated worker
route table acts like a contract-level branch selector.

The spender does not get to invent the next script. State decides which
templates are valid next steps.

## Practical Challenges of the Multiplexer Pattern

The pattern is useful, but it is not free.

The main challenges are:

- **cycles**: workers route back to mux, so the protocol is not a simple linear call tree
- **hash injection into state**: the state must carry the template hashes needed for future routing
- **template handling**: outputs are validated against script templates, not against arbitrary witness-supplied scripts

Those costs are acceptable here because the alternative is worse: a single
chess script would be too large, too entangled, and too expensive per move.

## Why Chess Still Has To Be Kept Small

Even with multiplexing, chess logic can still explode if every rule is enforced
eagerly and globally.

The main danger is broad board analysis:

- full board sweeps
- global attack recomputation
- “search all legal moves” style checks

Those patterns are exactly what make scripts grow out of control.

So the design keeps a second discipline:

- prefer bounded local checks
- avoid large board scans
- move difficult global rules into challenge flows

This is the central tradeoff in the example. Instead of asking the mover to
prove everything immediately, the mover proves only the bounded local rule, and
the opponent is given a way to refute illegality.

## Castling Semantics

Castling is intentionally split into two stages.

First, the castle worker checks the local structural conditions:

- the king starts from the home square
- the selected castle-right bit is present
- the destination square is empty
- the required corner rook is still on its corner
- the lane between king and rook is empty

If that succeeds, the worker records `recent_castle` and returns to mux.

After that, the opponent has two choices:

- make an ordinary reply, which implicitly accepts the castle
- enter the castle challenge path

The castle challenge path rewrites the castle lane into a proof board for one
of three squares:

- the start square
- the transit square
- the destination square

It then forwards into an ordinary move worker so the challenger can prove that
the castling king was capturable on that square.

This keeps ordinary castling local while still giving the opponent a bounded way
to refute “castle through check” style violations.

## Draw Negotiation

Draw claims use the same adversarial mindset.

Instead of trying to prove “no legal move exists” by sweeping the board, the
protocol turns the claim into a short dispute game.

The board is not physically flipped. The protocol flips interpretation:

- the board stays unchanged
- the signer stays their real side
- workers derive an `effective_turn`
- each side temporarily pilots the other side's pieces

The flow is:

1. the claimant calls `ChessMux.terminate` with `termination_action = 2`; this flips `turn` and enters `draw_state = 1`
2. the opponent tries to find a saving move for the claimant side using an ordinary worker
3. if that succeeds, play continues in `draw_state = 2`
4. if phase 2 ends without a decisive king capture, the original claimant loses
5. if phase 1 stalls, mux timeout accepts the draw

This keeps draw negotiation small enough to reuse the normal move workers.

## Draw By Agreement

Draw by agreement is a separate mux-level mechanism from draw claim.

It follows the usual chess rhythm: the offer is attached to an ordinary move,
not sent as a standalone protocol step.

The flow is:

1. the mover routes into an ordinary worker and sets a draw-offer bit in mux state
2. the worker applies the move and returns to mux with `draw_state = 4` or `5`
3. on the opponent's turn, they may accept through `ChessMux.terminate` with `termination_action = 4`
4. any ordinary reply implicitly rejects the offer and clears the draw-offer state

This keeps draw agreement asynchronous without adding a second timeout model or
a dedicated draw-offer-only transaction.

## Timeout

Timeout is the general liveness mechanism for multiplexed state machines.

Once a protocol is split across multiple templates, some states can stall:

- a player may refuse to choose the next route from mux
- a worker state may be committed but never advanced

Timeout is how the protocol guarantees progress.

In chess terms, this is not a weird add-on. It is a necessary part of the game
model, especially if the protocol is expected to behave more like blitz chess
than correspondence chess.

The split used here is:

- mux timeout is opponent-signed, because mux is still a strategic choice state
- worker timeout is permissionless, because a stuck worker state has an objective resolution

Timeouts are measured in DAA score, so the protocol can treat elapsed chain
time as part of the game.

## Design Rules

1. Keep durable state in mux.
2. Keep workers narrow and reusable.
3. Use state-driven template commitments.
4. Use challenges for rules that are easier to refute than to prove eagerly.
5. Use timeout as the universal liveness layer.
6. Avoid broad board sweeps.
7. Prefer semantic reinterpretation over physical board rewrites when building dispute subgames.

# Patterns Worth Keeping

This example is starting to produce reusable design patterns, not just chess
logic.

## Root Allocator Knows The Whole Family

`League` is the root allocator, so it carries the downstream commitments needed
to mint valid `Player` states.

Pattern:

- the root contract knows the whole reachable family, or a commitment to it
- later contracts can often keep only the future-facing subset they actually
  need

Another way to say it:

- the dependency frontier narrows as you move forward through the state machine
- the root is globally aware, later stations are only locally future-aware

## Commit Large Future Data Early, Expand Late

`League` and `Player` keep `routes_commitment`, while `Player.start_game`
expands the full `route_templates` blob only when materializing a `Game`.

Pattern:

- keep large routing tables as commitments in long-lived management layers
- expand them only at the boundary where they become operational

The same idea shows up again at the tail:

- the game keeps a commitment to `blake2b(settle_template || player_template)`
- `ChessMux.settle` expands that commitment only when the terminal route is
  actually taken

## Role Identity Comes From Template, Not Covenant Id Alone

One shared covenant id groups the system, but it does not tell us whether an
input is `League`, `Player`, or `Game`.

Pattern:

- use one shared covenant id for family membership
- use template-hash validation for role identity

## Stateful Identity Versus Auth Identity

The game binds players as `blake2b(owner || player_id)` rather than carrying
owner and id separately in game state.

Pattern:

- keep canonical player identity in the durable shell
- store a compressed auth-ready commitment in the episodic contract

## Durable Lifecycle Counters Beat Global Absence Proofs

`Player` now carries `open_games` and settlement decrements it back down.

Pattern:

- if later policy depends on "nothing is currently open", carry that fact as
  durable state
- do not plan around proving global absence of matching live UTXOs

This is what makes retirement practical:

- `retire` only needs to check `open_games == 0`
- it does not need a global proof that no live game still references the player

## Admin Stewards Funding, Not Protocol Liveness

`League` now has admin-signed `rebalance` and `fork` paths, but no admin path
for shutting down the public registration lineage. Once a game starts, play and
settlement are fully permissionless.

Pattern:

- let an admin manage funding and lane fan-out for a public entry layer
- do not let that admin control user-level liveness or terminal game outcomes

This keeps operational stewardship where it is useful without turning it into a
centralized kill switch.

## Leader / Delegate Split

`Player.start_game` and `ChessSettle.settle` both use a leader-plus-delegates
shape.

Pattern:

- one leader validates outputs and global transition shape
- delegates verify the leader role and their own inclusion in that transition

Settlement adds a useful refinement:

- delegates do not need to sign if the terminal payout is validated
  objectively on chain
- once funds are checked in `ChessSettle`, player delegates can stay fully
  passive

## Shared State Layout Across A Contract Family

The mux and worker contracts share one game-state layout verbatim.

Pattern:

- use one state shape across sibling templates when it simplifies transitions
- tolerate some redundant fields if it keeps routing and validation simple

## Open Questions Worth Preserving

These are not solved yet, but they are productive questions:

- when to compress with commitments versus carry raw bytes in state
- how much output reasoning should delegate paths do
- how to lock durable identity states against overlapping episodes
- where rating policy should live once settlement gets richer

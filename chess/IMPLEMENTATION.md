# Chess application implementation

The Argent files in `ag/` define the complete covenant application. The
generated files in `build/` are pinned. They are the only contract input to the
Rust application.

## Contract graph

The application has twelve actors:

- `League` manages registration lanes.
- `Player` stores one player's account and score.
- `Mux` stores durable game state and selects a move worker.
- `Pawn`, `Knight`, `Vert`, `Horiz`, `Diag`, `King`, and `Castle` validate move
  families.
- `CastleChallengePrep` prepares a bounded castling challenge.
- `Settle` pays the game stake and updates both player accounts.

These actors form one closed covenant domain. The graph includes registration,
game start, moves, challenges, draw actions, timeouts, settlement, retirement,
and contract maintenance.

`Mux` and all move workers use the same authored state layout. Argent generates
one compatible commitment cut for this family. It also generates the route
table and the hidden template fields that the application needs.

## Generated boundary

`build/artifact.json` contains the compiled contracts and their ABI. It also
contains the Argent actor and routing metadata. `build/manifest.json` records
the generated files. `build/sil/` contains the generated SIL for each actor.

The Rust code does not reconstruct compiler metadata. It uses the artifact
directly:

- `orchestrator.rs` uses `TxBuilder` to construct and execute covenant
  transactions in a local UTXO arena.
- `txdecode.rs` decodes calls and state with the generated ABI.
- `observer.rs` converts decoded transactions into typed chess events.
- `indexer.rs` reconstructs league lanes, players, live games, and settlement
  outputs from transaction history.
- `local_web_app.rs` exposes these flows through a local HTTP application.

The runtime keeps transaction IDs, input and output outpoints, signers, and the
full transaction history. The observer and indexer process that history. They
do not receive private state from the orchestrator.

## Entry and route design

Actor enums select worker targets. The source order of the `Move` enum in
`ag/game.ag` defines its integer values. The game logic uses those values for
board piece families and for worker selection.

`Mux.route` handles moves and draw offers. `Mux.terminate` handles
self-transitions such as draw claims, draw acceptance, and surrender. This
separate entry keeps actor-enum targets limited to routed actors.

`CastleChallengePrep` receives a typed worker target. It derives the required
worker from the challenged piece and move geometry. It accepts the route only
when the supplied target matches that result.

## Verification

`chess-covenant/tests/argent_chess_tx_tests.rs` executes all contract scenarios
against the generated artifact. It includes accepted and rejected
transactions. It does not compile source code during a test.

`chess-covenant/tests/generated_contract_tests.rs` verifies artifact integrity
and generated script-size snapshots. It also compares each generated size with
a frozen numeric measurement from the handwritten implementation. It does not
read the handwritten files.

`check.sh` enforces the generated boundary. It fails if `build/argent` exists
or if the Rust workspace restores a handwritten SIL compiler or file loader.

## Current runtime boundary

The local arena validates covenant transactions and is suitable for deterministic
tests and the web demonstration. Network submission, persistent storage, wallet
integration, and Testnet operation are separate future work.

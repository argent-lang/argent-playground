# Argent Chess Port

The Argent source in `ag/` defines the active chess covenant. The build pins its
artifact and generated Sil under `build/argent/`.

The application uses the artifact at both runtime boundaries:

- the orchestrator uses `TxBuilder` to create every covenant transaction
- the observer uses the generated ABI to identify inputs and decode state and calls

The artifact also supplies the canonical contract templates and the generated
worker route table. The Rust code does not compile handwritten Sil to reconstruct
this metadata.

## Contract Graph

The application contains twelve actors: `League`, `Player`, `Mux`, eight
move or challenge workers, and `Settle`. They form one closed application.

The graph covers:

- league registration, lane rebalance, and lane fan-out
- player registration, game start, delegation, rebalance, and retirement
- mux authentication, move commitment, draw actions, timeout, and settlement
- move geometry, board updates, draw challenges, castle challenges, terminal
  status, timeout, and value preservation in every worker
- settlement payout, score counters, open-game counters, and integer Elo updates

The Rust runtime exercises registration, game start, all move families, castle
challenges, termination, timeouts, settlement, league maintenance, player
maintenance, observer decoding, indexing, and the local web controller.

## Intentional Differences

The handwritten mux overloads `route` with `selector == MUX` for claim,
surrender, and draw acceptance. Argent actor enums select foreign route targets.
The Argent graph exposes these self-transitions as a separate `terminate` entry.

The handwritten shared state stores a packed 288-byte route value. It contains
eight worker templates and a settlement/player commitment. Argent generates a
256-byte worker route table. It carries the downstream `Player`, `Mux`, and
`Settle` template information in typed hidden state fields.

`CastleChallengePrep` receives a typed worker target. It derives the
required worker from the moving piece and geometry. It then requires the supplied
target to match before it routes to the generated template.

Generated entry signatures contain hidden template witnesses. The generated
state layouts and contract templates differ from the handwritten baseline even
when the user state is the same.

## Reference Baseline

The handwritten contracts in `build/sil/` remain for independent protocol tests
and script-size reports. These tests help detect semantic drift during the port.
The application runtime does not load these contracts.

`check.sh` regenerates `build/argent/`, rejects fixture drift, and runs the full
Rust test and lint suite.

## Follow-up Opportunity

The generated contracts repeat some shared constants, state declarations, and
helper functions that an actor does not use. This is an output-size opportunity.
It does not block the port.

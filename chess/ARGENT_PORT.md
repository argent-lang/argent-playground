# Argent Port

The handwritten contracts in `build/sil/` remain the executable reference while
the native Argent port is reviewed. `build/argent/` is generated from `ag/app.ag`
and is intentionally not consumed by the Rust orchestration yet.

## Initial Comparison

All twelve contracts have native Argent definitions: `League`, `Player`,
`ChessMux`, the eight move/challenge workers, and `ChessSettle`. Their generated
Sil compiles as one closed application.

The first textual pass compared each generated entry against its handwritten
counterpart. It covers:

- league registration, lane rebalance, and lane fan-out
- player registration state, game start, delegation, rebalance, and retirement
- mux authentication, move commitment, draw actions, timeout, and settlement
- every worker's move geometry, board rewrite, draw challenge, castle challenge,
  terminal status, timeout, and value-preservation rules
- settlement payout, score counters, open-game counters, and integer Elo update

The protocol conditions and state transitions match at this level. Execution of
the generated contracts through the existing Rust game suite remains the next
validation step.

## Intentional Differences

The handwritten mux overloads `route` with `selector == MUX` for claim,
surrender, and draw acceptance. Argent actor enums select foreign route targets,
so the port exposes those self-transitions as a separate `terminate` entry.

The handwritten shared state stores a packed 288-byte route value containing
eight worker templates and a settlement/player commitment. Argent generates a
256-byte worker route table and carries the downstream `Player`, `ChessMux`, and
`ChessSettle` template information as typed hidden state fields.

`ChessCastleChallengePrep` receives a typed worker target. It still derives the
required worker from the moving piece and geometry, then requires the supplied
target to match before routing to its generated template.

Generated entry signatures contain hidden template witnesses and may order
arguments differently from the handwritten ABI. The generated state layouts and
contract templates therefore also differ, even where user-visible chess state is
unchanged.

## Follow-up Observations

The generated contracts currently repeat shared constants, state declarations,
and helper functions even when a particular actor does not use all of them. This
is compiler output-size work, not a semantic port blocker.

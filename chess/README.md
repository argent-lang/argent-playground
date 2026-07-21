# Chess Example

This folder is a standalone Cargo workspace for the mux/worker on-chain chess covenant demo.

## Port Baseline

This workspace was imported from the Silverscript `chess` branch at commit
`abe95d210b24c5d84065722e9e59d46e446f0a85`. It is the executable baseline for
an in-place port to Argent.

The handwritten contracts under `build/sil/` remain the active reference
implementation. The native Argent port lives under `ag/`, and its separately
pinned output is generated under `build/argent/`. The Rust application still
uses `build/sil/`; switching it to the generated contracts is a later validation
step rather than part of the initial source port.

The workspace is independent of the parent playground crate. During the port,
it uses sibling checkouts of Argent and Silverscript through local Cargo paths.
CI pins Silverscript compiler revision
`513c62a04671aa2bba5558b70982dabdb2c18da0` for the executable baseline.
Run its complete checks from this directory with:

```sh
./check.sh
```

Run the local web application with:

```sh
cargo run -p chess-covenant --bin local_web_app
```

Then open `http://127.0.0.1:8080`.

## Core Idea

This example applies the multiplexer pattern to chess.

Instead of one giant contract that tries to enforce the whole game at once, the
protocol is split into:

- `ChessMux`: the durable checkpoint contract that owns the full game state
- worker contracts: narrow validators for bounded move families and challenge flows

All of these contracts share the same serialized state layout.

The key primitive is still the mux pattern itself:

1. mux authenticates the move attempt
2. mux commits the pending move into shared state
3. mux routes into the selected worker template
4. the worker proves one bounded claim
5. the worker returns to mux with updated state

The important design philosophy is bounded verification rather than eager global
analysis. The protocol prefers proving a narrow local claim now and using
challenge paths for rules that are easier to refute than to prove by sweeping
the full board.

Layout:

- `chess-covenant/`: Rust crate with compile-time tests for the covenant source.
- `ag/`: the complete native Argent contract graph.
- `build/sil/`: handwritten Sil reference contracts used by the Rust application.
- `build/argent/`: pinned Sil and artifact output generated from `ag/`.
- `ARGENT_PORT.md`: the initial textual comparison and known port boundaries.
- `ARCHITECTURE.md`: high-level design principles for the mux/worker chess protocol.
- `COVERAGE.md`: audit matrix for current classical-rule coverage vs protocol behavior.

The current prototype already covers the core mux/worker chess engine and a
real outer `League -> Player -> ChessMux -> ChessSettle` flow.

## Tasks

This list is kept up to date. Completed, descoped, or split items are reflected
here in the same change that updates their status.

### Protocol

- [✓] Implement mux-routed worker execution for pawn, knight, vertical, horizontal, diagonal, king, castle, and castle-challenge flows.
- [✓] Keep one shared durable game state layout across mux and all workers.
- [✓] Support pawn promotion and en passant.
- [✓] Support castling with explicit challenge paths for start, transit, and destination-square attack proofs.
- [✓] Support draw negotiation as a bounded dispute flow reusing ordinary workers.
- [✓] Support draw by agreement, offered together with a move and accepted asynchronously on the opponent's turn.
- [✓] Support surrender and timeout-based termination paths.
- [✓] Allow terminal settlement of play states by king capture, so classical chess rules can be enforced through the protocol.
- [✓] Add representative coverage for the ordinary check-evasion reduction.
- [ ] Add rare draw rules such as repetition and the 50-move rule.
- [ ] Make sure all classical game rules can be fully enforced by the opponent through the protocol, and verify that no rule or challenge path is missing.
- [ ] Tighten all draw and termination rules until the full settlement logic is robust enough for production use.
- [ ] Tighten the logic of blitz chess and make sure all supported time-control modes still preserve challenge/timeout liveness, including non-blitz modes where there is no per-turn clock.

### Outer Layer

- [✓] Define the outer covenant and game-entry/settlement protocol needed to represent and update scores on chain.
- [✓] Build an outer durable layer with `League` registration, `Player` accounts, `ChessMux` game start, `ChessSettle` settlement, and `Player` retirement guarded by `open_games`.
- [✓] Keep admin control limited to `League` lane funding and fan-out, while leaving game progression and settlement permissionless once a game is open.
- [ ] Build full chess server logic with persistent player scoring based on game results.

### Funds And Settlement

- [✓] Enforce objective on-chain payout rules at settlement: winner takes all, and draws split with the odd extra unit going to black.
- [✓] Let mutually signed game start define the initial game stake flexibly, with the live game UTXO preserving that value exactly across play.
- [✓] Route terminal game stake objectively into the winner's `Player` output at settlement.
- [✓] Route drawn game stake objectively into both `Player` outputs at settlement, with the odd extra unit going to black.

### Off-Chain Enforcement

- [✓] Add an initial off-chain Rust orchestrator that can register players, exchange invites and settlement requests, and drive real local tx execution for short games.
- [✓] Add a tx-only covenant observer that decodes chess-family inputs and reconstructs typed authored output states and events without prior UTXO knowledge.
- [ ] Implement an off-chain Rust wrapper for the on-chain logic so a player can enforce classical rules by challenging and proving wrong behavior.
- [ ] Base that wrapper on a well-established Rust chess crate as the main rules engine and source of truth.
- [ ] Import a strong chess benchmark or test suite with many complex games and verify them against this protocol.
- [ ] Build an adversarial opponent, or equivalent configurable wrapper modes, that tries to cheat and confirm that an honest player can always enforce their rights.

### Runtime

- [ ] Build a runtime for playing the game over Kaspa Testnet 12, starting with a console interface if that is the fastest path.

### Documentation And Book

- [ ] End this project with a full md book that explains the mux pattern, its subtle points, and the overall philosophy behind this chess system.
- [ ] Use the book to show how complex distributed systems can be built directly on native pure Kaspa L1 after covenant support.
- [ ] Make the chess challenge theme a central teaching device for bounded verification: prove a narrow claim, avoid full sweeps, and push the reader toward NP-like protocol thinking instead of eager global recomputation.
- [ ] Write the book for advanced builders and agents, so it does not just document this project but also transfers the design mindset needed to build similar systems.

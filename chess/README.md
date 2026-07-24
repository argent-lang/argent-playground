# Argent Chess

This directory is a standalone Cargo workspace for the Argent chess
application. It contains the covenant source, generated contracts, local
transaction runtime, observer, indexer, web application, and tests.

## Application layout

- `ag/` contains the Argent source.
- `build/` contains the pinned artifact, manifest, and generated SIL.
- `chess-covenant/` contains the Rust runtime and tests.
- `baseline/sil/` contains the former handwritten SIL contracts. These files
  are passive reference data. Rust code does not load or compile them.

The generated artifact is the application boundary. The transaction builder
uses it to construct contract calls. The observer uses the same artifact to
identify contracts and decode their state, calls, and outputs.

See [IMPLEMENTATION.md](IMPLEMENTATION.md) for the compiler and runtime
boundaries. See [ARCHITECTURE.md](ARCHITECTURE.md) for the mux and worker
protocol. See [COVERAGE.md](COVERAGE.md) for the current chess-rule coverage.

## Build and check

Run all Chess checks from this directory:

```sh
./check.sh
```

The script regenerates `build/` from `ag/app.ag`. It rejects generated-file
drift and legacy SIL execution paths. It then runs formatting, build, test, and
clippy checks.

The contract test suite executes every contract transition from
`build/artifact.json`. It covers the outer account lifecycle, all move
families, castling challenges, draw flows, timeouts, settlement, and contract
maintenance. Other tests cover artifact integrity, script sizes, the
transaction-backed orchestrator, observation, indexing, and the web
controller.

## Run the local web application

Run:

```sh
cargo run -p chess-covenant --bin local_web_app
```

Open `http://127.0.0.1:8080`.

The web application runs a local transaction arena. Each action builds and
executes a real covenant transaction against the generated contracts. It does
not connect to a Kaspa network.

## Protocol summary

The durable flow is:

```text
League -> Player -> Mux <-> move worker -> Mux -> Settle -> Player
```

`League` creates player accounts. Two `Player` contracts start a game. `Mux`
stores the durable game state and routes each move to a bounded worker.
Workers validate one move family and return to `Mux`. `Settle` applies the
result to both player accounts.

The current application supports normal moves, promotion, en passant,
castling challenges, draw offers and claims, surrender, timeouts, stake
settlement, rating updates, account maintenance, and league lane maintenance.
The remaining chess-rule gaps are listed in [COVERAGE.md](COVERAGE.md).

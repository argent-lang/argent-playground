# Argent playground

Small Rust playground for using [Argent](https://github.com/michaelsutton/argent)
as a client app would.

Expected checkout layout:

```text
kaspanet/
  argent/
  argent-playground/
```

## Selected demos

```text
src/lib.rs
  shared playground helpers

ag/<demo>/
  optional file-backed Argent source for demos with imports or multiple apps

src/bin/basic_counter.rs
  inline Counter `.ag` app
  builds `build/counter`
  creates and executes one Counter::bump tx

src/bin/signed_counter.rs
  adds owner authorization to the basic counter
  builds transaction-dependent signature arguments through the fluent builder

src/bin/two_actor_exchange.rs
  inline Ping/Pong `.ag` app
  builds `build/ping_pong`
  creates and executes Ping::send and Pong::reply txs

src/bin/dynamic_actor_choice.rs
  inline Router/Alpha/Beta `.ag` app
  builds `build/dynamic_actor_choice`
  creates Router::choose txs for two runtime-selected targets

src/bin/multiapp_badge.rs
  file-backed Controller/Badge `.ag` apps under `ag/multiapp_badge`
  builds `build/multiapp_badge`
  genesis-launches both covenants and executes an observed co-spend

src/bin/open_icc_agent.rs
  file-backed Cell/Forager `.ag` apps under `ag/open_icc_agent`
  builds `build/open_icc_agent`
  binds a concrete observed actor through explicit runtime context

src/bin/dex_asset.rs
  file-backed Core, Pair and asset apps under `ag/dex`
  registers a Pair, mints a quote lot, funds its KAS reserve and executes a swap
  exercises signed open-ICC co-spends and expanded asset capsule handles

build/
  generated artifacts and Silverscript output
```

Run the first demo:

```bash
cargo run --bin basic_counter
cargo run --bin signed_counter
cargo run --bin two_actor_exchange
cargo run --bin dynamic_actor_choice
cargo run --bin multiapp_badge
cargo run --bin open_icc_agent
cargo run --bin dex_asset
```

Run the complete local check, including every demo binary:

```bash
./check.sh
```

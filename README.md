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

src/bin/counter_reject.rs
  inline Counter `.ag` app (negative-path hardening demo)
  builds `build/counter_reject`
  asserts an honest Counter::bump passes and a tampered one is rejected

src/bin/genesis_counter.rs
  inline Counter `.ag` app
  builds `build/counter`
  launches the Counter covenant, then executes one Counter::bump tx

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

build/
  generated artifacts and Silverscript output
```

Run the first demo:

```bash
cargo run --bin basic_counter
cargo run --bin counter_reject
cargo run --bin genesis_counter
cargo run --bin two_actor_exchange
cargo run --bin dynamic_actor_choice
cargo run --bin multiapp_badge
```

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

src/bin/basic_counter.rs
  inline Counter `.ag` app
  builds `build/counter`
  creates and executes one Counter::bump tx

src/bin/two_actor_exchange.rs
  inline Ping/Pong `.ag` app
  builds `build/ping_pong`
  creates and executes Ping::send and Pong::reply txs

build/
  generated artifacts and Silverscript output
```

Run the first demo:

```bash
cargo run --bin basic_counter
cargo run --bin two_actor_exchange
```

# Settlement Auth Shape

Target transaction:

`(game) -> (settle)` then `(settle, player, player) -> (player, player)`

The intended auth split is:

- `ChessMux` routes terminal state into `ChessSettle`
- generated hidden state carries the `ChessSettle` and `Player` templates
- `ChessSettle` is the settlement leader
- both `Player` inputs are delegates
- `ChessSettle` validates both `Player` outputs and the objective payout split
- player delegates only verify that the leader is the expected terminal settle contract

```mermaid
sequenceDiagram
    participant G as Game input
    participant S as Settle input
    participant PW as Player input A
    participant PB as Player input B
    participant OW as Player output A
    participant OB as Player output B

    G->>S: use generated settle template and route terminal state
    S->>S: verify terminal chess result
    S->>PW: verify input template == player_template
    S->>PB: verify input template == player_template
    S->>S: verify bound player refs match inputs
    S->>OW: decrement open_games
    S->>OB: decrement open_games
    S->>OW: update rating and result counters
    S->>OB: update rating and result counters
    S->>OW: verify winner-takes-all or draw split output value
    S->>OB: verify winner-takes-all or draw split output value
    S->>OW: verify output template == player_template
    S->>OB: verify output template == player_template
    S->>OW: verify stat transition
    S->>OB: verify stat transition

    PW->>S: verify settle leader references this player
    PB->>S: verify settle leader references this player
```

## Current Slice

What is implemented now:

- `ChessMux` routes a terminal mux state into `ChessSettle`
- generated template fields authenticate the `ChessSettle` output and later
  `Player` inputs and outputs
- `ChessSettle` settles into two `Player` outputs
- settlement requires both players to have `open_games > 0`
- settlement decrements `open_games` for both players
- settlement increments `games`
- terminal result updates `wins` / `draws` / `losses`
- settlement applies the bounded Elo-style rating update
- settlement validates objective KAS payouts on chain:
  winner takes all, draw splits, odd extra goes to black
- `Player.delegate_settle` is fully signature-free and only verifies settle/player linkage

This means the durable layer now has one concrete lifecycle invariant:

- a `Player` can retire only when `open_games == 0`

And one concrete funds invariant:

- once a game UTXO has been created, its value is preserved through live play
- terminal settlement routes exactly that value into the two recreated `Player`
  outputs according to the objective result rule

What is still open:

- stronger guarantees around output ordering or template commitments if we want
  delegates to reason more directly about the produced `Player` outputs
- whether `Player.start_game` should also require `open_games == 0` and forbid
  overlapping games entirely

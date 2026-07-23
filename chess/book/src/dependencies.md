# Dependencies And Template Injection

This is the core structural picture.

```mermaid
flowchart TD
    L[League registration lane<br/>immutable self-recreating contract]
    P[Player<br/>persistent score contract]
    G[Game<br/>episodic chess contract]
    S[Settle<br/>terminal settlement worker]

    L -- registers --> P
    P -- starts --> G
    G -- routes terminal state --> S
    S -- settles into --> P

    L -. carries player template .-> P
    L -. carries mux and settle templates .-> P
    L -. carries worker-table digest .-> P

    P -. opens worker table .-> G
    P -. carries role templates .-> G

    S -. validates Player inputs by template .-> P
    P -. delegates to Settle leader .-> S
```

## What each layer needs to know

```mermaid
flowchart LR
    subgraph League["League"]
        LH["Player template"]
        LM["Mux and Settle templates"]
        LR["worker-table digest"]
    end

    subgraph Player["Player"]
        PM["Mux, Settle, and Player templates"]
        PX["worker-table digest"]
        PP["player_id"]
        PO["owner"]
        PR["rating"]
    end

    subgraph Game["Game"]
        GH["256-byte worker table"]
        GT["Mux, Settle, and Player templates"]
        GW["white_player_ref"]
        GB["black_player_ref"]
        GR["result / terminal state"]
    end

    subgraph Settle["Settle"]
        SH["role templates"]
        SX["worker-table digest"]
        SR["terminal result"]
    end

    LH --> Player
    LM --> Player
    LR --> Player

    PM --> Game
    PX --> Game
    PP --> Game

    GT --> Settle

    GW --> Player
    GB --> Player
    SH --> Player
    SX --> Player
```

Today `player_id` does not come from injected League state. It is derived as
`blake2b("LeaguePlayerId" || outpoint_txid || outpoint_index_le32)`, so the
domain is fixed by the contract code itself.

Today the game state binds each side as `blake2b(owner || player_id)`, not as a
raw `player_id`. That keeps the game-side footprint to one field per side while
still letting settlement recover canonical player ids from `Player` inputs.

The Argent compiler derives hidden routing fields from the actor graph. `League`
and `Player` carry the worker-table digest. `Player.start_game` receives the
matching 256-byte worker table through a generated witness and stores it in the
game state.

The game state also carries direct `Mux`, `Settle`, and `Player`
template fields. These fields let the game return to mux or enter settlement
without adding non-worker entries to the worker table.

## Why shared covenant id is not enough by itself

With one shared covenant id:

- `League`, `Player`, and `Game` are all in the same covenant family
- covenant-id grouping is enough to prove they belong to the same system
- covenant-id grouping is **not** enough to prove their role

So settlement needs both:

1. same cov-id group
2. role validation by template hash

That is why the generated design depends on:

- hidden role templates and a worker-table digest
- input-side template validation primitives

The compiler propagates these dependencies through the actor graph. Authored
state contains only the chess data.

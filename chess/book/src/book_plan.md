# Book Plan

This is a lightweight outline for turning the chess notes into a real mdBook.

## Part 1: Why Chess

- why chess is a good stress test for multi-script covenants
- why a single monolithic covenant is the wrong baseline comparison

## Part 2: Inner Game Engine

- mux and workers
- why the move engine is split by piece / route family
- timeout and terminal-state handling inside the game family

## Part 3: Outer Durable Layer

- `League` as root allocator
- `Player` as durable identity and score shell
- `Game` as episodic state machine
- retirement and lifecycle counters such as `open_games`

## Part 4: Hash Injection And Template Identity

- covenant id versus role identity
- injected hashes
- commitments versus expanded witness data

## Part 5: Auth Shapes

- leader / delegate transitions
- start-game auth
- settlement auth
- where signatures matter and where they should disappear

## Part 6: Settlement Policy

- terminal status to durable stat transition
- why rating is policy, not just arithmetic
- candidate places to host rating logic

## Part 7: Design Patterns

- root allocator pattern
- future-facing dependency frontier
- compressed identity references
- commit-early / expand-late

## Part 8: Open Edges

- active-game locking versus merely counting open games
- richer settlement policy
- route commitment schemes
- stronger delegate-side output reasoning

## Near-Term Writing Order

If we only flesh out a few chapters first, the best order is probably:

1. overview / why chess
2. dependencies and template injection
3. start-game flow
4. settlement flow
5. patterns chapter

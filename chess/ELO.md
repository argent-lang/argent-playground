# Approx Elo

This note sketches a bounded, on-chain friendly approximation of an Elo-style
rating update. Canonical formula reference:
[Elo rating system](https://en.wikipedia.org/wiki/Elo_rating_system).

The goal is not exact floating-point Elo. The goal is a rule that:

- uses only cheap integer arithmetic
- fits the covenant philosophy of bounded verification
- stays stable under deterministic rounding

## Core idea

Instead of computing:

`E = 1 / (1 + 10^((R_opp - R_self)/400))`

directly, approximate `E` by rating-difference buckets.

Each bucket stores an expected-score value in fixed-point units, for example
scaled by `1000`.

Example table:

```text
diff bucket                expected_self
(-inf, -800)              990
[-800, -600)              970
[-600, -400)              910
[-400, -250)              820
[-250, -150)              700
[-150,  -75)              600
[ -75,   75)              500
[  75,  150)              400
[ 150,  250)              300
[ 250,  400)              180
[ 400,  600)               90
[ 600,  800)               30
[ 800, +inf)               10
```

The exact buckets are a policy choice.

## Fixed-point conventions

Use only integers.

- `SCALE = 1000`
- `WIN = 1000`
- `DRAW = 500`
- `LOSS = 0`
- `K` is an integer, for example `32`

Then:

`delta = floor(K * (actual - expected) / SCALE)`

and:

`new_rating = old_rating + delta`

## Bounded witness strategy

The spender witnesses:

- `rating_self`
- `rating_opp`
- `result_code`
- `bucket_lo`
- `bucket_hi`
- `expected_self`

The covenant verifies:

1. `diff = rating_opp - rating_self`
2. `bucket_lo <= diff < bucket_hi`
3. `expected_self` is the table value for that bucket
4. `result_code` is one of `LOSS`, `DRAW`, `WIN`
5. `delta = floor(K * (result_code - expected_self) / SCALE)`
6. `new_rating_self = rating_self + delta`

This is bounded because the contract never computes exponentials. It only:

- checks a bucket membership proof
- checks a table value
- applies integer arithmetic

## Pseudo code

```text
constants:
    SCALE = 1000
    LOSS = 0
    DRAW = 500
    WIN = 1000
    K = 32

input:
    rating_self
    rating_opp
    result
    bucket_lo
    bucket_hi
    expected_self

algorithm:
    require(result == LOSS or result == DRAW or result == WIN)

    diff = rating_opp - rating_self

    require(bucket_lo <= diff)
    require(diff < bucket_hi)

    require(expected_self == lookup_expected(bucket_lo, bucket_hi))

    raw = K * (result - expected_self)
    delta = floor_div(raw, SCALE)
    new_rating = rating_self + delta

    write new_rating into the successor score state
    require(the successor covenant state hash matches)
```

`lookup_expected` does not need to be a real dynamic lookup. In a covenant it can
just be a chain of bucket checks:

```text
if diff < -800:
    expected = 990
else if diff < -600:
    expected = 970
...
else:
    expected = 10
```

The witness form is still useful because it makes the intended bucket explicit.

## Two-player settlement

For a game result:

- white win: white gets `WIN`, black gets `LOSS`
- draw: both get `DRAW`
- black win: white gets `LOSS`, black gets `WIN`

Then apply the same algorithm independently to both score UTXOs.

Because the same table is used on both sides, the two updates remain symmetric.

## Why this fits the covenants

- no floating point
- no exponentials
- no logs
- no power opcodes
- only bounded integer checks

## Future refinement

Possible upgrades later:

- denser buckets near zero difference
- different `K` values by rating range
- separate provisional mode
- off-chain higher-fidelity rating engine with on-chain verified bucket proofs
- replacing long range chains with compiler support that lowers bucket checks to
  `OpWithin` where profitable

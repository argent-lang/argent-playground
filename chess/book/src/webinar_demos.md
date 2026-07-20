# Webinar: Demo Order

The demos climb from one simple authorization pattern to a full application.

## 1. Native Assets

Native assets are the ICC warm-up.

- show one covenant family recognizing that another covenant family is also
  authorizing the same transaction
- emphasize that the communication channel is shared transaction approval

## 2. Mux

The small mux example establishes the base MCF pattern.

- one mux state
- two worker templates
- selector-based routing
- immutable template injection
- why timeout is needed

## 3. Chess

Chess is the full system demo.

- `League -> Player -> ChessMux -> ChessSettle`
- zoom into `ChessMux -> workers -> ChessMux`
- point out the timeout edge on every worker
- point out that terminal game value and rating update both settle back into
  `Player`

## The Arc

1. ICC: one covenant family can recognize that another family is co-authorizing the same transaction
2. Mux: one contract can choose among peer templates inside one covenant family
3. Chess: a full application can be assembled from those patterns

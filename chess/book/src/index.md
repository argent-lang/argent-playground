# Chess Covenant Notes

This mdBook is currently a design notebook for the outer league / player / game
layer.

The immediate goal is clarity:

- which contracts depend on which other templates
- which hashes must be injected into state
- how a game is opened from two durable player states
- how settlement auth is intended to flow
- how durable player lifecycle rules like retirement hang off those transitions

All diagrams assume the current preferred outer design:

- one shared covenant id across league, player, and game
- different contracts carry the role identity
- role validation therefore needs input-template primitives plus injected hashes

The next goal is to turn these notes into a real case-study book:

- what the chess example teaches about multi-script covenant design
- which state and hash dependencies belong at each layer
- which parts are "finished pattern" versus "interesting open edge"

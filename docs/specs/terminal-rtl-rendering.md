# Terminal RTL rendering

The terminal emulator and tmux remain authoritative in logical cell order.
Rendering applies UAX #9 independently to each physical grid row, then shapes
each directional run with its logical text. A reversible visual/logical column
map keeps cursor, hit testing, selection, copy, styling, and wide cells tied to
the emulator grid.

Explicit Unicode direction and isolate controls remain in shaping and copied
text. They are never stripped or rewritten.

## Limitation

Each cursor-addressable terminal row is a separate BiDi paragraph. Soft-wrapped
rows are not joined into one Unicode paragraph: terminal applications can
overwrite either row independently, and the grid does not expose durable
paragraph boundaries. This can differ from document-layout UAX #9 behavior
across a wrap, while preserving terminal cursor and damage semantics.

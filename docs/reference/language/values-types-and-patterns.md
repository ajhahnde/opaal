# Values, types, and patterns

OPAAL has ordinary immutable values and nominal records and variants. A constructed record supplies each declared field exactly once; a variant carries the payload declared by its enum. Matching uses the same qualified nominal identity, not a coincidentally similar field shape. Values such as project tool and endpoint identities remain distinct from strings and cannot be forged by spelling their names.

Lists preserve order. A pattern can bind a declaration or parameter, or test an arm of a `match`. Record, variant, list, and list-rest patterns share those positions. A list-rest pattern keeps the tail as a list:

```opaal
match ["build", "test", "review"] {
    [first, ...rest] => { rest }
    [] => { [] }
}
```

Closed, unguarded matches are checked for missing and unreachable arms. Nominal generics use invariant parameters; the available constraints are `Equal` and `Ordered`. A function result annotation and parameter types participate in analysis before evaluation.

A stream is a separate single-consumer carrier, not a list in disguise. Collecting it makes a retained list and consumes a bounded item and byte budget. See [commands, pipelines, and streams](commands-pipelines-and-streams.md). The [`std::outcome` module](../std/outcome.md) defines the compiled `Result` and `Option` variants.

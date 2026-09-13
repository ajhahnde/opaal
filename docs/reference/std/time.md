# `std::time`

`std::time` exports the nominal `Timestamp` type, `wall_now() -> Timestamp`, and `monotonic_now() -> Int`. Both observations need explicit clock authority; neither reads an ambient clock during check.

Wall time renders as UTC RFC 3339 with exactly nine fractional digits, such as `1970-01-01T00:00:00.000000000Z`. A value outside the four-digit RFC 3339 year range refuses. Monotonic time is a separate observation for elapsed time and deadlines. It is not a wall timestamp and must not be serialized or compared as one.

A project plan makes one wall-clock observation for creation and expiry; planning does not run the task's clock operation. Execution checks expiry before allowing the accepted action. A caller may narrow a monotonic deadline but cannot extend it. See [lifecycle](../operational/lifecycle.md) and [resources and lifetimes](../../concepts/resources-and-lifetimes.md).

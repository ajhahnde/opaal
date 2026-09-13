# Actions and effects

An action has typed parameters, an explicit result, an `effects` block, and a body. The declaration states the requests that may be reached; it grants none of them. OPAAL checks the action graph without invoking a host adapter.

The closed effect families are `filesystem.read`, `filesystem.write`, `process.run`, `network.http`, `secret.reveal`, `clock.wall`, and `clock.monotonic`. Arguments are static literals or qualified project identities. An exported task's scoped requests require explicitly imported identities. Repeated non-secret requests collapse semantically; repeated secret reveals remain visible and can make a task unsupported.

Actions may call functions and statically named actions. The graph must be acyclic, at most 64 calls deep, and contain at most 1,024 actions. Every caller declares the requests reachable through its callees. A function cannot call an action; an action is not a first-class value. A `task` declaration appears only in a project's manifest-selected root module and exports one action without overriding its signature or effects.

Ordinary script and interactive evaluation refuse an effectful invocation before platform access. A project check compares declared requests with exact authority and target capabilities without running the task. Execution requires a reviewed plan digest for that task. Read [authority](../operational/authority.md) for grants, denials, and enforcement verdicts.

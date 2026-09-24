# Platform support

OPAAL's qualified standalone host surfaces are Linux x86_64 and macOS arm64.
Explicit foreground `^program` execution in standalone scripts and interactive
sessions is qualified on both. Source parsing, formatting, analysis, project
inspection, check, plan validation, and audit do not probe or start those
programs. Qualified secret-free accepted execution and audit run on both. A
task declaring maintained `process.run` can execute on Linux when exact
authority and the retained executable identity pass.

On macOS, project check reports a controlled process request as unsupported
and plan writes a refused non-executable artifact. No pathname-spawn fallback
occurs in that controlled route. The same task can still be inspected,
checked, planned as refused, and audited from existing journal evidence. An
unrecognized target reports unknown rather than assuming Linux behavior.

Linux process execution holds the opened exact executable and starts through `/proc/self/fd`. Its parent binds argv, environment, process group, output, and deadline; the child's internal filesystem and network behavior remain unenforced and require explicit acknowledgement. POSIX file, HTTP/TLS, secret-sink, and clock boundaries have their own enforcement checks.

This support statement covers the repository's exercised standalone host workflows. It does not claim a downstream image, Redox target, hardware device, or package that has not been qualified. See [capability matrix](operational/capability-matrix.md).

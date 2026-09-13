# Platform support

OPAAL's qualified standalone host surfaces are Linux and macOS arm64. Source parsing, formatting, analysis, project inspection, check, plan validation, and audit are host-free or bounded-read operations on those hosts. Qualified secret-free accepted execution and audit run on both. A task declaring maintained `process.run` can execute on Linux when exact authority and the retained executable identity pass.

On macOS, check reports a process request as unsupported and plan writes a refused non-executable artifact. No pathname-spawn fallback occurs. The same task can still be inspected, checked, planned as refused, and audited from existing journal evidence. An unrecognized target reports unknown rather than assuming Linux behavior.

Linux process execution holds the opened exact executable and starts through `/proc/self/fd`. Its parent binds argv, environment, process group, output, and deadline; the child's internal filesystem and network behavior remain unenforced and require explicit acknowledgement. POSIX file, HTTP/TLS, secret-sink, and clock boundaries have their own enforcement checks.

This support statement covers the repository's exercised standalone host workflows. It does not claim a downstream image, Redox target, hardware device, or package that has not been qualified. See [capability matrix](operational/capability-matrix.md).

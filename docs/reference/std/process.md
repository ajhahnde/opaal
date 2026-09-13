# `std::process`

`std::process` exports `ToolResult { status: Status, stdout: Bytes, stderr: Bytes }` and `run(tool: tools::ToolIdentity, arguments: List[String]) -> ToolResult`. A project tool identity comes from the manifest and exact host lock; it cannot be replaced with a string path.

The lock binds an absolute native executable path, canonical version, SHA-256 digest, platform, and complete ordered child environment. The maintained version probes are fixed `git --version` and `cargo --version --verbose`; project data cannot choose another probe. The runtime revalidates the executable digest before each admitted probe or run.

Maintained execution is Linux-only. Linux retains the opened executable and starts that descriptor through `/proc/self/fd`; a pathname replacement cannot change the selected binary. macOS reports the request unsupported during check and writes a refused, non-executable plan rather than spawning by pathname. One run permits eight attempts, one live child, 8 MiB each of stdout and stderr, and ten minutes per attempt. Cancellation and timeout stop the owned process group and reap it.

The parent controls argv, environment, output, and deadline. The child's internal filesystem and network behavior remain unenforced, so the exact `process.run` grant must explicitly acknowledge that boundary. See [tool locks](../formats/tool-lock.md) and [platform support](../platform-support.md).

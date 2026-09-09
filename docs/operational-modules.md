# Bounded operational modules

OPAAL's maintained operational module layer implements the narrow adapter
contract needed for repository-readiness work. The public embedding APIs are
grouped under `opaal_runtime::operational`; the explicit accepted-project-plan
executor exposes the same operations to these source modules:

- `std::data`: bounded TOML decoding, nested record lookup, and canonical JSON
  encoding;
- `std::path`: byte-preserving lexical normalization, containment, and join;
- `std::filesystem`: no-follow bounded regular-file reads and one atomic
  evidence-file replacement;
- `std::time`: distinct authority-gated wall and monotonic observations;
- `std::version`: canonical SemVer parsing, rendering, and range matching;
- `std::integrity`: canonical SHA-256 hashing and rendering;
- `std::url`: normalized HTTP/HTTPS URL parsing without credentials or
  fragments;
- `std::http`: bounded no-redirect requests with endpoint-bound TLS and one
  opaque `SecretHeader`; and
- `std::process`: on Linux, fixed Git/Cargo version probes plus bounded
  locked-tool execution under an explicit complete child environment; macOS
  reports process requests as unsupported.

These modules add no global built-ins. Ordinary script and interactive
evaluation still refuse an effectful action before any adapter call. The
accepted-plan executor initializes the project source closure without effects,
then supplies a controlled operational host only while invoking the selected
task action.

Operational identities are nominal rather than interchangeable strings.
`std::time::wall_now`, `std::version::parse`, and `std::url::parse` return
`Timestamp`, `Version`, and `Url`; HTTP returns `HttpResponse`, and process
execution returns `ToolResult`. HTTP requests accept only the selected
project's `EndpointIdentity`, process execution accepts only its
`ToolIdentity`, and `secret_header` accepts only its `SecretIdentity`.
`SecretHeader` is a separate opaque sink handle. Its request slot may be
`null`, and the byte-body slot may be `null`, but ordinary values cannot stand
in for any of these nominal identities or handles. Project identities and
secret handles cannot escape an action, including through a list, record,
nominal value, variant, table, or captured callable.

## Data and path behavior

TOML input is UTF-8 and at most 16 MiB. Duplicate keys and malformed types are
parser errors, nesting stops at 64, and TOML datetimes require an explicit time
conversion. JSON output supports only values with an exact JSON representation,
sorts object keys, contains no insignificant whitespace, and refuses the first
byte beyond 16 MiB.

Paths remain native Unix bytes. Normalization is lexical and performs no host
lookup. Join rejects absolute components and parent traversal outside the named
root. File reads and writes repeat containment at the adapter boundary,
traverse and retain the physical root plus descendant directory descriptors,
open every component with no-follow semantics, accept regular files only, and
apply 16-MiB per-file plus 32-MiB aggregate-read limits.
Atomic writes create a private sibling, sync it, replace the exact evidence
name atomically, and sync the retained parent directory.

Wall-clock values render only as UTC RFC 3339 with exactly nine fractional
digits, for example `1970-01-01T00:00:00.000000000Z`. Values outside the
four-digit RFC 3339 year range refuse. Monotonic values remain distinct
non-serializable observations and are never interpreted as wall time.

## HTTP, TLS, and secrets

HTTP permits at most eight requests, 128 headers, 64 KiB of header bytes, 8 MiB
for each body, and 30 seconds per request. It never follows redirects or retries;
all response statuses, including 3xx and non-2xx statuses, are ordinary data.
Plain HTTP is limited by project validation to literal loopback addresses.

HTTPS builds a trust store from only the endpoint's explicit CA bytes. It does
not extend trust with platform roots, proxy settings, ambient credentials, or
environment state. The POSIX adapter requires a literal IP connect host so DNS
resolution cannot escape the request deadline; TLS identity remains the
separate declared server name. The adapter validates the certificate chain,
time, and declared TLS server name. Changing any of those identity inputs makes
an accepted plan stale before task execution.

`SecretHeader` is non-cloneable, non-displayable, and non-serializable. Its
debug form contains only the secret ID, endpoint, header name, and
`[redacted]`. Ordinary headers cannot use a declared secret-header name. The
runtime checks both network and reveal authority before removing the secret
from its store, lends its bytes only to the adapter call, clears the encoded
request buffer, and zeroizes the owned secret immediately after the one call.
Secret bytes that are not a valid HTTP field value refuse before network I/O;
that attempted materialization is still the secret's one permitted use.
Evaluation-owned redaction patterns remain until the context closes, so later
adapter results and failures are still filtered after the consuming buffer is
gone. Response bodies and header values are redacted against the materialized
raw, hex, base64, percent-encoded, and JSON-escaped forms before they can become
ordinary values. Denial or stale preflight never consumes the secret; a second
use or reinjection of the same identity refuses.

Controlled execution syncs a `network.http` before-record before the request.
For a secret-bearing request it then syncs a distinct `secret.reveal`
before-record, performs the one materialization and adapter call, and syncs the
matching reveal after-record before closing the network effect. These records
carry only endpoint, method, secret ID, header name, verdict, attempt, outcome,
and digests; the payload is never journaled.

## Maintained processes

The tool lock supplies an exact native executable path, canonical version,
SHA-256 digest, platform, and an ordered complete child environment.
`inherit = []` is mandatory. Before execution the runtime requires explicit
`HOME`, `TMPDIR`, `PATH`, `CARGO_HOME`, `RUSTC`, `RUSTDOC`, `LC_ALL`, `TZ`,
`CARGO_NET_OFFLINE`, `GIT_CONFIG_NOSYSTEM`, and `GIT_CONFIG_GLOBAL` values and
never reads the ambient environment.

Maintained process execution is currently Linux-only. After accepted-plan
identity verification, the Linux adapter retains the securely opened
executable and starts that exact descriptor through `/proc/self/fd`; replacing
the pathname cannot change what runs. macOS provides no maintained
descriptor-backed execution route, so check records the process request as
`unsupported`, plan emits a refused non-executable artifact, and the adapter
refuses before any pathname spawn. Inspection, check, refused planning, and
audit remain available on macOS.

The only version probes are adapter-owned `git --version` and
`cargo --version --verbose`; project data cannot select another probe. Process
status is returned separately from adapter errors. The executable digest is
revalidated before every admitted probe or run. One run admits at most eight
attempts, one live child, 8 MiB each of stdout and stderr, and ten minutes per
attempt. Cancellation or timeout stops new work, sends TERM to the owned
process group, waits at most five seconds, sends KILL if required, waits at
most five more seconds, drains both pipes, and reaps the leader. A leader that
exits while an owned descendant remains is cleaned up and reported as a
protocol failure. This enforces the parent boundary only: child filesystem and
network behavior remains explicitly acknowledged as unenforced.

[← Documentation index](README.md) · [Authority and resources](authority-and-resources.md)

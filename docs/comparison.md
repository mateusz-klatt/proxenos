# Comparing MCP initialization

The `compare_init` binary measures how long a fresh server process takes to return a valid
MCP initialize response. It starts Proxenos for the Claude client and claude-delegator's Codex
bridge under Node. Both receive the same request for revision `2024-11-05`. Initialization
begins before process spawn and ends when the complete response line reaches the harness.
Response validation, the initialized notification and shutdown occur outside that measurement.
A trial succeeds only if the response has the requested revision, a tools capability and a
server name and version, and the server exits successfully after its input closes. The harness
checks the Proxenos and Codex bridge names and the bridge's version against its package manifest.

This measures startup of one Proxenos server or one Codex bridge. It does not measure a
provider turn, result quality, a complete multi-provider deployment or steady-state throughput.
The older comparison work on `feat/comparison` (harness commit `4e81430`) also investigated
advisory turns, timeouts, cancellation, server death, EOF, provider failures and replies.
This benchmark uses its choice of common protocol revision and isolated Codex stub.

## Run on a comparison host

Use the same quiet machine for both servers. Supply a clean, working local installation of
claude-delegator with its dependencies already installed and a Node executable compatible
with it. D12 specifies version **3.5.0** (tag `v3.5.0`, commit
`cd4eb9ec6b08bc065884be893ae4bf55367ae9fe`). Another installed version is identified in the JSON
report with `d12_baseline_version_matches: false`. Use the report's actual version when sharing
that run. The baseline is launched through `node <installation>/server/codex/index.js`.

From the Proxenos checkout on Linux:

```sh
cargo build --locked --release --bin proxenos
cargo build --locked --release --features test-tools --bin proxenos-stub --bin compare_init
target/release/compare_init \
  target/release/proxenos "$(command -v node)" /path/to/claude-delegator \
  target/release/proxenos-stub 20 > initialize.json
```

On Windows, run the same two Cargo build commands. In PowerShell 7.4 or later, supply
separate executable and installation paths, including the quotes when a path contains spaces:

```powershell
$nodeExe = (Get-Command node.exe -CommandType Application | Select-Object -First 1).Source
$baseline = 'C:\bench installs\claude-delegator'
& .\target\release\compare_init.exe .\target\release\proxenos.exe $nodeExe $baseline .\target\release\proxenos-stub.exe 20 > initialize.json
```

Use the path to the Node executable. The stub's containing
directory must be writable and support hard links. Each trial gets a fresh working directory
and home. Its search path contains only a hard link named `codex` to the supplied stub.
The harness sends no `tools/call`. The timer includes any startup checks the server itself
performs. The initialize response and any preceding notifications share a 1 MiB limit. A
10-second deadline covers the exchange and shutdown after spawn. To exercise the harness
with a fixture peer and the compiled Proxenos server, run
`cargo test --locked --features test-tools --test compare_init`.

## Reading a result

The final argument is an even count of trial pairs between 2 and 1000. Each target runs first
in half of the pairs. Each pair starts two separate processes in sequence. Fingerprinting reads
the executables, bridge entry file and manifest before timing. Filesystem caches can stay warm
between processes. There is no discarded warm-up. The report keeps every trial,
its target and position, the latency and identity returned on success, and the error on
failure. Summary medians use successful trials only. A failed trial also makes the harness
exit unsuccessfully after writing its report. Compare failure counts alongside latencies.

The report identifies the Proxenos binary, stub, Node binary, baseline entry file and package
manifest by SHA-256 and records the operating system and architecture. On Windows, Node receives
the ordinary drive or UNC spelling of the bridge path when it resolves to the fingerprinted file.
The report retains the canonical path. If the ordinary spelling resolves elsewhere or is unavailable,
Node receives the canonical path.

For publication, retain the raw JSON privately and share a copy with machine-specific paths
removed as appropriate.
Record the machine and OS release, the Proxenos build commit and toolchain, the baseline
checkout commit and dependency lock, and the Node version alongside it. A launcher hash alone
does not identify all installed JavaScript dependencies.

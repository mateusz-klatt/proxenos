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

## Results: Windows, 2026-11-27

Measured by the Windows verifier on Windows 11 Pro 10.0.26300 x86_64. The host was shared with
a WSL session that carried a light load, a code review, during the runs. Inputs, by SHA-256:

| input | identity |
|---|---|
| Proxenos | v0.1.0 release `proxenos.exe`, `b798a30e83dca7c878631120142fb3d55a35430bb3e2d78f1462f4e309eba6bf` |
| claude-delegator | 3.5.1, git `56975da` (`d12_baseline_version_matches: false`; D12 names 3.5.0); `server/codex/index.js` `50118f6a298f1918f2eb5834622b4cdeec98b6df3ee8006d0f20c6cc0bb48610`; `package.json` `3feb4f57be7453a2afe9d3c94c95cff60679ed7d8299cd1ae05aa130ccf9d05a` |
| Node | v26.12.1, `1c63f1417e2e26d83ca9a9ec1d548524ab1305bc7209d90ebca56257ff0d84ed` |
| harness and stub | built from main `16bd004`; `proxenos-stub` `ce10b551e197674753dc510a96673ca35a074d20cc4747e92fcc1cde90b3a58b` |

**Summary.**
- Proxenos returns `initialize` about 9× sooner, using one process for six providers where
  the baseline runs one Node process per provider. Each process has a working set about 7×
  smaller and private memory about 4.5× smaller.
- Process-tree cleanup left no survivors in every scenario tried.
- The **first `tools/list` is slower**: 721 ms against 163 ms at p50. The client waits for that
  list before it can call a tool, so this is a regression to fix, tracked in #57.

### Initialize

`compare_init` with 100 pairs at revision `2024-11-05`, as described above. Latency runs from
spawn to the complete initialize response, in milliseconds.

| target | ok / failed | min | p10 | p50 | p90 | max | p50 run first / second |
|---|---|---|---|---|---|---|---|
| Proxenos 0.1.0 | 100 / 0 | 10.2 | 10.5 | 11.1 | 12.1 | 34.7 | 11.1 / 11.2 |
| claude-delegator 3.5.1 (Codex bridge, Node) | 100 / 0 | 94.1 | 96.8 | 101.2 | 112.4 | 157.1 | 101.5 / 100.9 |

### Process cleanup (Proxenos only)

The verifier used a stub `agy` that starts a grandchild process, then counted descendants
before the event and the survivors afterwards. These runs used the verifier's own scripts,
which are not part of this repository, so this table cannot be reproduced from the checkout
alone.

| scenario | descendants before | response | survivors after 2 s | after 10 s |
|---|---|---|---|---|
| `cancel` of a background handle | 3 (agy, conhost, grandchild) | cancel answered in 3 ms | 0 | 0 |
| shutdown by closing standard input | 3 | server exited after 9 ms, code 0 | 0 | 0 |
| `TerminateProcess` of the server alone | 3 | killed after 3 ms | 0 | 0 |

The D6 Job Object kills its whole tree when its last handle closes. That covers a hard kill of
the server too.

### Footprint

The verifier ran 21 sessions per target (1 cold, 20 warm), with the real `PATH` and user profile,
then took 3 samples after 60 s idle. Same caveat as above: the verifier's own scripts.

| target | tools | processes | initialize p50 / p95 | first `tools/list` p50 / p95 | private p50 | working set p50 | idle 60 s private / working set |
|---|---|---|---|---|---|---|---|
| Proxenos 0.1.0 | 13 | 1 | 14.4 / 16.4 ms | **721 / 758 ms** | 5.9 MiB | 8.7 MiB | 5.1 / 8.4 MiB |
| claude-delegator Codex server | 2 | 1 (node) | 162 / 223 ms | 163 / 224 ms | 26.5 MiB | 59.4 MiB | 24.0 / 57.5 MiB |
| claude-delegator Antigravity server | 2 | 1 (node) | 322 / 378 ms | 323 / 378 ms | 26.6 MiB | 59.4 MiB | 23.5 / 57.2 MiB |

claude-delegator registers one server per provider, so seven providers mean seven Node
processes, about 7 × 59 MiB of working set. One Proxenos process serves six providers.

The first `tools/list` in Proxenos waits for D7 discovery of every installed CLI. On the same
host, an earlier measurement on 2026-11-25, with four tools, gave about 223 ms. See #57.

### What these results do not measure

Provider turns, result quality, token cost, steady-state throughput, or the cost of running
several providers at once. The cleanup and footprint tables cover Windows only.

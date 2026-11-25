# Architecture

Proxenos lets one coding agent consult another. This document records what has been decided, why,
what was measured, and what is still unknown. A decision here stands until a record in `docs/adr/`
changes it. [ADR 001](adr/001-decisions-by-the-families-that-can-answer.md) says how decisions are
taken, and [votes.md](votes.md) records who proposed and who supported each one.

**Status: milestone 1 in progress.** `proxenos mcp --client <client>` serves the MCP layer of D4.
Codex and Antigravity run through the D6 supervisor. Behavioural tests install the crate’s stub
in a private search path.
The remaining decisions describe the design.

## 1. Terms

- **Client**: the agent CLI that asks. Claude Code, Codex CLI, Antigravity CLI, Kimi Code, Grok
  CLI, Cursor Agent and GitHub Copilot CLI are the seven supported.
- **Provider**: the agent CLI that is asked. The same seven.
- **Delegation**: one request from a client to a provider, started through an MCP tool and run by
  spawning the provider's CLI once.
- **Role**: a text that tells the provider how to act in a delegation, such as reviewing code or
  judging a plan.

Proxenos replaces claude-delegator, a Node.js project that did the same job with one MCP server
process per provider. What that project measured about the seven CLIs is carried over provider by
provider, as each is ported, and stays the reason for most of the rules below.

## 2. Decisions

### D1. One binary, and one MCP server for each client

`proxenos mcp --client <client>` serves every provider's tools to that client from one process.
The other roles of the binary are subcommands of the same file: `integrate`, `doctor`, `catalog`
and `usage`.

- The server leaves out the client's own provider, so no client delegates to itself.
- It lists a provider only when that provider's CLI is found. A CLI that is found but fails its
  version check stays listed, and a call to it answers with the reason.
- Finding and checking the CLIs never delays the MCP handshake. It runs in parallel, once, when a
  tool list or a call first needs it, and each check has its own time limit. A CLI that failed its
  check is checked again when it is called. Otherwise what was found holds until the server
  restarts.
- "Not found", "found but its check failed" and "found behind a launcher Proxenos cannot follow"
  are three different answers. Calling an installed CLI missing sends its user looking in the
  wrong place.

Why: one registration for each client instead of seven or eight, and nothing to repair when a CLI
is installed or removed. A crash now takes every provider's tools with it for that session, which
the per-provider processes of claude-delegator avoided; a panic in one call is therefore caught
and turned into that call's error (see D4).

### D2. The tool surface uses Proxenos's own names

Each provider has two tools, `<provider>` to start a delegation and `<provider>-reply` to continue
one. Three tools are shared: `await` and `cancel` (D17), and `usage` (D9). Parameters are in
snake_case:

| Parameter | Meaning |
|---|---|
| `prompt` | The task. |
| `expert` | Optional: the name of a role. The server supplies the role's text. |
| `mode` | `advisory` or `implementation`. It adds a sentence of instruction. It is advice to the provider, never a permission. |
| `instructions` | Free text from the caller, added after the role. |
| `access` | `full` or `read-only`. What `read-only` enforces differs by provider and is stated in each tool's schema. |
| `cwd`, `model`, `effort`, `timeout_ms` | As named. Which of them a provider accepts differs. |
| `thread_id` | On a reply: the session to continue. The reply inherits the settings of its start (D19). |
| `wait` | `true` by default. With `false` a call that is admitted returns a handle at once (D17). |
| `coordination` | Optional progress reporting through Iris (D16). |

The answer is a JSON object sent as the text of the result, because MCP clients show a model the
text; whether any of them also shows structured content has not been measured (section 5). It
carries a schema version from the first release, and it keeps three kinds of statement apart:

- **What Proxenos observed**: the state (finished, failed, cancelled, still running), a failure of
  a kind it can see for itself (not found, launch, busy, timeout, cancellation, a memory limit,
  abnormal exit), the provider, the settings requested and the settings actually passed, the
  access mechanism Proxenos selected, the duration, the exit status, how much output there was
  and when the last of it arrived, memory and CPU with the way they were measured (D18), and,
  where it could look, the commit before and after and the changed and untracked paths it saw,
  also after a timeout.
- **What the provider reported**: the `thread_id`, the content, and where it says so the model it
  ran and the tokens it used.
- **What is not claimed**: that `advisory` was obeyed, which instruction files the CLI loaded, or
  what a review concluded. Snapshots of git cannot establish who changed a path, and do not show
  every edit made during the run.

A failure has a kind: not found, launch, busy, authentication, a trust prompt, a refusal, quota,
timeout, cancellation, a memory limit, abnormal exit, or unknown. Busy is Proxenos's own answer to
a delegation above its limit (D17); a refusal is the provider's. Authentication, a trust prompt, a
refusal and quota are read from what the provider printed, so they are a classification and not an
observation. Quota carries the reset time when the provider gives one. A CLI that exits with
success without having run a turn is a failure.

A long answer is returned as an excerpt with the path of a file that holds what was captured, up
to the capture limit. That file is kept apart from the child's temporary files, which D6 removes
once the child is reaped. How long it is kept, where, and who may read it are not decided. A
client that cannot read the path, from inside a sandbox for example, has the excerpt and nothing
more. Voters of vote 6 raised both limits.

The version field was a provisional choice of vote 4, and vote 6 backed it: eight of nine
positions are for it, and one is for additive change without a version. The file for a long answer
is still provisional. Claude (Fable 5.2, max) chose it in vote 4; over votes 4 and 6 six of nine
positions are for it, one of them only if D2 defines a retention that is secure and stable, which
it does not do yet. Two would have the rest read in pages, and one would keep the file only if a
tool of Proxenos can read it back.

In step 3 the server sends schema version 1. Each attempt records its process status, elapsed
time, deadline, last output, total and captured byte counts, including the private CLI output
file. The request's timeout covers discovery, workspace inspection and both attempts of a retry.
Version probes have a ten-second deadline and git commands two seconds each. Their cleanup
budgets also apply, so the response can arrive later. A cancelled MCP request has no response. A connection whose input ends can still
receive the cancellation result during the drain described in D4.

The supervisor keeps the first 4 MiB from each stream and reads up to 4 MiB of the Codex
final-message file. Output beyond the limit can include a terminal record that the parser cannot
see. Answers longer than 16 KiB have a UTF-8 excerpt and an archive path. This step keeps the
eight most recent long answers per server, up to 4 MiB apiece, in a separate temporary directory.
An orderly exit removes its files where permitted. An abrupt exit or a denied deletion can leave
them behind. Unix directories use mode 0700 and answer files 0600. Windows inherits the user's
temporary directory permissions. A caller inside another sandbox may only be able to read the
excerpt. This is the current implementation, pending a durable retention decision.

Git observations use a private configuration with the worktree, index and object database.
This avoids executing repository-configured filters and hooks. The two samples can differ from
the user's own status, including its configured filters, excludes and submodule changes. Paths
are relative to the worktree root. If Git is unavailable, the directory is outside a worktree,
or a bounded, UTF-8 listing cannot be obtained, that sample is absent.

The same object as MCP structured content is left out until it has been measured on the seven
clients: sent twice, a long answer could cost its reader twice.

Why: `workspace-write`, the old name for the default, meant full unattended access and said
otherwise. The registration had to be renamed anyway, so the names change once. The result was
`{thread_id, content}` and could not say why a delegation failed; a provider at its quota answered
only that the turn had failed.

### D3. Tokio, and a supervisor that owns each child

Every delegation has a deadline and a cancellation token. A separate task owns the child process,
its pipes and its temporary files. A background delegation (D17) therefore outlives the request
that started it, and is still stopped and reaped on its deadline, on `cancel`, or when the client
closes the connection.

### D4. MCP through `rmcp`, pinned and kept behind one module

The MCP revision of 2026-07-28 removed the `initialize` handshake: each request now carries its
protocol version, and a server must answer `server/discover`. Clients of the earlier revisions
still open with `initialize`. Proxenos serves both.

It uses the official Rust SDK, `rmcp`, pinned to an exact version and reached only through the
`mcp` module, so that it can be replaced without touching the providers. No `rmcp` type leaves
that module, and the tests of the protocol send raw frames to the compiled binary, so they hold
for whatever is behind it.

This choice is provisional. In vote 4 one of the two reviewers argued for an owned module of about
three hundred lines instead of 65 crates with four gaps to patch; the owner kept `rmcp` for now.

Measured on 2026-11-24 with a throwaway server on Linux, `rmcp` 3.11.0, features `server` and
`transport-io`, 190 scripted checks with hand-written frames:

- One process served `initialize` clients (2024-11-05 to 2025-11-25) and clients of 2026-07-28,
  with a tool list built at run time and schemas passed through unchanged.
- An unsupported version got the error the specification asks for in the new lifecycle, and the
  newest supported version in the old one.
- 65 crates, none of which needs a C compiler. A stripped release binary of that server was 3.4 MB.

What the `mcp` module has to do itself, because the library does not:

- **End of input.** Proxenos reads ahead on standard input and signals running calls as soon as
  it sees the input end. Up to 16 chunks of at most 64 KiB each can wait in the input queue.
  When rmcp falls further behind, the reader waits for space before reading again. Only that
  backpressure delays its observation of the end of input.
- **Read and write failures.** The library ends quietly when it cannot read or write. Proxenos
  takes the library's answers from it frame by frame as they are written, and writes them to
  standard output itself, so that a failure in either direction stops serving and ends it with
  exit status 1 and the first reason recorded. Exit status 0 means the client closed the
  connection. On failure Proxenos stops forwarding bytes into the library. If the library
  dispatches a buffered tool call after the halt, the handler returns an internal error without
  invoking the tool. A failed connection may be unable to deliver that error.
- **A client that does not read.** Up to 64 MiB of output can wait for the client, including the
  frame being collected and the frame being written. The collector passes complete frames to the
  writer in order. Crossing the limit or reaching the end of the library's output partway through
  a frame fails the connection. After a failure, serving can end even if standard output is blocked.
- **Cancellation.** The library signals a token and does not stop the handler. Every handler
  waits on that token.
- **Panics.** A handler that panics is never answered. Proxenos catches the panic and answers
  with an error.
- **Identity.** The server name and version are set; the default is the library's own.
- **Batches.** The revision of 2025-03-26 requires a server to accept JSON-RPC batches, and the
  library refuses them. Proxenos does not offer that revision: a client that asks for it is
  offered 2025-11-25, which it may accept or not.
- **Calls still running when the connection ends.** When rmcp sees the end of its input it gives
  handlers five seconds to finish. A call cancelled by the end of input gets a reply if it
  finishes within that drain. A call that finishes later gets no reply. Proxenos still waits for
  running calls and the workers that reap their children and remove their files. Ten seconds
  after Proxenos sees the end of input, the first failure, or the library stopping, it abandons
  any remaining work and output and exits with a failure. When a failure was already recorded,
  that failure is the one reported.

Known differences from claude-delegator, accepted: a line that is not JSON is ignored instead of
answered with a parse error, and a `tools/call` without a name is "method not found" instead of
"invalid params".

Not measured: Windows, macOS, and any real client.

### D5. Seven providers, each with the same four seams

The set of providers is closed. Each provider module decides four things and nothing else: its
tool schemas, the command line it builds, how it reads the CLI's output, and how it tells failure
from success. It spawns nothing and speaks no MCP.

Each provider also declares, in one place, what it supports: which settings it takes, what a
reply must repeat, and by what means `read-only` is enforced, if at all. The schemas and the
validation are both derived from that declaration. A provider says a delegation succeeded only on
evidence that a turn completed, not on a plausible answer or a zero exit status.

The interface is fixed only after two providers with unlike reply rules, Codex and Antigravity,
have been built against it. Building both in the first milestone was a provisional choice of vote
4, and vote 6 backed it: eight of nine positions are for both, and one would start with Codex
alone.

Where providers differ, each one's tests assert its own behaviour through the compiled binary.
Antigravity's reply must name a model; Cursor's must not. A shared test could assert neither.

For Codex, a completed turn and a final answer are separate evidence. A turn that explicitly
lacks its answer can be asked once for that answer in the same confirmed thread. The server
does not repeat the original task. Antigravity's print command can direct an unknown ID to a
different conversation. The server requires an exact regular `<id>.db` or `<id>.pb` entry,
checks again after discovery and the initial git snapshot, and compares the ID the CLI reports.
These checks cannot make the store lookup atomic with Antigravity's own lookup.
`PROXENOS_AGY_CONVERSATIONS_DIR` takes precedence over `CLAUDE_DELEGATOR_AGY_CONVERSATIONS_DIR`
and the default `~/.gemini/antigravity-cli/conversations`.

### D6. Children are supervised to the end

- **Unix**: the child calls `setsid` before it executes the CLI. It leads its own session and
  process group and starts without a controlling terminal. On a stop, Proxenos sends SIGTERM to
  the group and gives it three seconds before SIGKILL, even when the leader exits first.
  Proxenos observes the leader's exit with `waitid` and keeps its PID reserved until the last
  group signal, so a reused PID cannot take that signal to another group. Launches run on
  blocking workers. Their pipe setup and native spawn are serialized, with cancellation and
  the deadline checked under the lock immediately before spawn. If the OS stalls inside spawn,
  that launch and the launches queued behind it can return late. Other calls keep their timers.
  After the fork, Rust's `Command::spawn` waits for the exec handshake before returning a child
  handle. If the handshake is still stalled when D4's ten-second shutdown limit expires,
  Proxenos exits without that handle. The orphan can run the CLI if exec later succeeds.
- **Windows**: Proxenos creates a job with kill-on-close and starts the CLI suspended with
  `CREATE_NO_WINDOW`. The child has a hidden console. Proxenos assigns the process to the job
  and checks cancellation and the deadline again immediately before resuming its primary thread.
  A stop terminates the job. The job handle belongs to Proxenos alone, so its members also die
  when Proxenos dies. The Windows behavioural test proves containment by making the stub request
  breakaway as it spawns a grandchild. It checks that Windows denies the request and that the
  same grandchild, started inside the job, dies when the job is stopped. It also checks whether
  the child's console window is visible.
- **Everywhere**: the child reads EOF from the null device on standard input. Proxenos captures
  output as bytes up to a limit per stream and removes temporary files only after reaping the
  child. Past the limit it still drains the pipes, or the child would block. Once the child exits,
  a descendant gets up to two more seconds to finish writing. Proxenos then closes its readers
  and kills the remaining group or job members. The result keeps the child's exit status and
  the time of the last read from either stream, including bytes beyond the capture limit.
  After the final group signal or job close, the caller waits up to two seconds for the leader's
  status and file cleanup together. Once reaped, the status can be returned while a worker
  finishes cleanup. Failed and skipped launches also report their outcome before cleanup.
  D4 counts these workers toward its shutdown deadline. A temporary input's pathname stays
  reserved until cleanup ends, even if the CLI unlinks it. A sibling directory reserves the
  name for cooperating launches that use a stable parent directory. Both names must fit the
  filesystem's name limit.
  On Windows volumes that generate 8.3 aliases, this reserves only the requested name. If a
  CLI unlinks an input with a short name and Windows assigns that name as another file's
  short alias, cleanup can delete the other file.

What this cannot do: on Unix, if Proxenos itself is killed outright, its children survive, and a
descendant that leaves the process group is not reached. If the OS denies a group signal,
Proxenos reports the error. On macOS a group containing only its zombie leader also returns
`EPERM`. Once `waitid` confirms the leader has exited, Proxenos accepts that result. A live
descendant that Proxenos lacks permission to signal can go undetected in that case.
The reaper retains the child's files until the leader exits. If the reaper is still running
when D4's shutdown deadline expires, the server exits with a failure. An abrupt exit can leave
the temporary input and its reservation directory behind.
On Windows a CLI that opens `CONIN$` directly can wait on the hidden console until its deadline.
If Proxenos dies between creating the suspended process and assigning it to the job, the process
can remain suspended before it runs any CLI code. Stopping a child does not undo what it wrote.

### D7. A CLI is found by reading `PATH`, and a batch file is never run

Proxenos reads `PATH` as data, directory by directory, and takes the first directory that holds the
CLI. Known install locations are tried only after `PATH`. It never asks `which` or `where`.

On Windows a `.cmd` or `.bat` shim is read and followed to the program it starts: an executable, a
JavaScript loader, or a PowerShell script. The shim itself is never started: Rust would run it
through `cmd.exe` and refuses any argument it cannot escape safely for that shell, a line break
among them, and every real prompt has one. When the loader is JavaScript, `node` is looked for in
this order: an explicit override, beside the shim, on `PATH`, then stable install locations.

### D8. Data ships inside the binary and can be overlaid

The model catalogue, the role texts and the rules for clients are compiled in, so the binary works
alone. Files in the user's configuration directory overlay them. `proxenos catalog refresh` asks
each installed CLI for its models, where that costs nothing, and writes the overlay. A change of
roster therefore needs no release. A user adds a role by adding a file.

- What `refresh` generated and what the user wrote are kept apart, and `refresh` replaces only
  the first. An overlay carries a schema version and the time it was generated, so that an old
  one does not hide a newer catalogue in the binary without `doctor` saying so.
- Proxenos chooses no model. On a start it passes the model that the call names, or else the one
  that the user's own overlay sets for that provider, and otherwise none. A CLI that is given no
  model runs its own default or refuses to start, and Proxenos chooses for it in neither case. A
  reply follows D19. The result shows what was passed and, where the provider reports it, which
  model ran (D2).
- A catalogue entry never widens what a provider is declared to enforce (D5).

The point on the model is [ADR 002](adr/002-proxenos-chooses-no-model.md), which the owner accepted
on 2026-11-25. Until vote 6 it read "the default model of each provider is the strongest one it
offers", a provisional choice of vote 4. In vote 6 seven of nine positions opposed that default.
Five of them said to pass no model unless one is named, and three of those let the user's
configuration name it.

### D9. Usage is read, never spent

`usage` reports each CLI's subscription windows without running a model, where the CLI gives them
that way. Where one does not, `usage` says so for that provider: claude-delegator found only the
plan for Cursor. A voter of vote 6 asked for this limit to be stated. `usage` takes a list of
providers and by default returns a short summary, the windows with their percentages and reset
times; the full report with prices per model is asked for explicitly. It needs a JSON-RPC
client over a child's pipes and one HTTP request to a loopback address. It makes no HTTPS request,
so Proxenos has no TLS stack. The Moonshot API-key balance that claude-delegator read is not
supported: Kimi is used through its subscription.

### D10. Proxenos registers itself with each client

`proxenos integrate <client>` writes the registration and `--remove` takes it out. It changes only
entries it wrote, refuses to take over one it did not, and keeps a record of what it wrote.

- TOML files are edited in place, keeping comments, because other tools, Iris among them, mark
  their own blocks with comments.
- For Claude Code it generates a local marketplace and a plugin that names the binary by absolute
  path, as Iris does.
- `proxenos doctor` reads the registrations back, checks the CLIs, and runs a handshake.

### D11. Releases are built and signed as Iris's are

Six targets: `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (statically linked),
`x86_64-apple-darwin` and `aarch64-apple-darwin`, and `x86_64-pc-windows-gnu` and
`aarch64-pc-windows-gnullvm`. GoReleaser v2.20.0 builds them with its Rust builder through Cargo's
alias `auditablezigbuild` (`cargo auditable zigbuild --release --locked`, without
`--features test-tools`, using pinned Zig 0.17.1, `cargo-zigbuild` 0.24.1 and `cargo-auditable`
0.7.7). `Cargo.toml` strips release symbols and enables thin LTO while keeping `panic = "unwind"`,
so handler panic catching (D4) stays intact. On `aarch64-pc-windows-gnullvm`, `libunwind` is linked
explicitly for that unwind runtime. `cargo-auditable` embeds each target's linked crate graph inside
the binary so that Syft 1.54.0 catalogs target-specific Rust crates into each archive's SPDX JSON
SBOM. Each archive holds `proxenos` (or `proxenos.exe`), `LICENSE` and `NOTICE` with fixed
`root:root` ownership and the commit timestamp. `.goreleaser.yaml` sets `release: disable: true` and
defines no `signs:` block so a local `goreleaser release` with a personal token cannot create a
release or sign outside the workflow.

Compilation and signing are separated across jobs so Cargo build scripts never see signing
credentials or write tokens. There is **no `v*` tag-push trigger**: a pushed tag executes the
workflow YAML from the tagged commit, so an unmerged tagged commit could skip tag checks and request
signing secrets. Instead, a release starts only via `workflow_dispatch` on branch `main` with a
non-empty `version` input (e.g. `0.1.0`). Branch `main` is guarded by GitHub ruleset `28030033`
(pull request, the four required CI checks, no force push, no deletion); tag ruleset `release tags`
(`28073662`) blocks updating and deleting `refs/tags/v*` with no bypass, and tag ruleset
`release tag creation` (`28075912`) blocks creating `refs/tags/v*` with a bypass for the repository
admin role (which the owner's publication of the draft uses to create the tag). All seven Certum and
Apple secrets live in the GitHub `release` environment, whose deployment policy allows **only branch
`main`** and requires review by the owner. Only the four signing jobs (`sign-macos` and
`sign-windows` in `release.yml`, and `sign` in each signing proof) use `environment: release`. One
release therefore needs **two** `release` environment approvals: `sign-macos` and then
`sign-windows`, a few minutes apart (after Apple notarization and `verify-macos`).

Downloaded release tools (`goreleaser` v2.20.0 for Linux x86-64 and macOS universal `Darwin_all`,
`syft` v1.54.0 for Linux amd64 and macOS arm64, and `cosign` v3.1.4 for Linux amd64) are fetched
directly via `curl --proto '=https' --tlsv1.2` without third-party installer actions and verified
against repository-pinned SHA-256 digests for both the downloaded tarball (before unpacking) and the
extracted binary (before execution). `ssign` is built from a pinned commit (`Le-Syl21/ssign`, `--locked`) in a
credential-free job; its SHA-256 is recorded there and re-checked in `sign-windows` as a **transit
integrity** check across the artifact handoff within the same workflow run, not a pre-established
reproducible digest.

The pipeline **never pushes a git tag**. Because no tag exists when the release builds run, the build
jobs set `GORELEASER_CURRENT_TAG=v<version>`, verify a clean working tree explicitly
(`git status --porcelain`), and run `goreleaser build --clean --parallelism=1 --skip=validate`.
GoReleaser reads `GORELEASER_CURRENT_TAG` for its release metadata while `--skip=validate` skips
only the exact-tag `git describe` check without introducing snapshot version suffixes. `publish`
creates a **draft** GitHub release with `tag_name: v<version>` and `target_commitish` set to the
dispatched commit SHA; GitHub creates the git tag only when the owner publishes that draft.

In `.github/workflows/release.yml`, a `workflow_dispatch` on `main` in `mateusz-klatt/proxenos` with
a non-empty `version` input runs:

- **Release check (`check-release`, `ubuntu-latest`, `contents: read`)**: verifies exact
  case-sensitive repository and `refs/heads/main`, validates SemVer `version` against `Cargo.toml`,
  and fails early if tag `v<version>` or any published release visible to `contents: read` already
  exists.
- **Certum signer (`signer`, `ubuntu-latest`, `contents: read`)**: builds pinned `Le-Syl21/ssign`
  from source with `--locked` and no credentials, recording and outputting its SHA-256 for transit
  integrity.
- **Credential-free builds (`build-macos` on `macos-latest`, `build-linux-windows` on
  `ubuntu-latest`, `contents: read`)**: verify the pinned `goreleaser` tarball and binary SHA-256
  digests, pin `GORELEASER_CURRENT_TAG=v<version>`, check tree cleanliness, build the binaries with
  `goreleaser build --clean --parallelism=1 --skip=validate` and no secrets, check Mach-O/ELF/PE
  architectures, static Linux linking and `--version`, upload unsigned binaries as 1-day artifacts,
  and output every binary's SHA-256 digest.
- **macOS signing and notarization (`sign-macos`, `macos-latest`, `environment: release`,
  `contents: read`)**: checks exact repo and `refs/heads/main`, verifies the unsigned macOS binary
  digests from `build-macos` before signing without running Cargo, writes decoded `developer-id.p12`
  and `AuthKey.p8` with mode `0600` at creation time and deletes both files alongside the temporary
  keychain in an `EXIT` trap, signs both binaries with native `codesign` (hardened runtime and
  secure timestamp), submits each to Apple with `xcrun notarytool --wait`, requires JSON
  `status == "Accepted"`, and outputs both signed binary SHA-256 digests.
- **Pre-packaging macOS verification (`verify-macos`, `macos-latest`, no permissions, no secrets)**:
  verifies the signed macOS binary digests from `sign-macos`, checks both bare binaries with
  `codesign` and `spctl` against the pinned Developer ID identity, and runs the arm64 binary with
  `--version` before `sign-windows` runs.
- **Windows signing (`sign-windows`, `ubuntu-latest`, `environment: release`, `contents: read`)**:
  runs only after `verify-macos` passes, checks exact repo and `refs/heads/main`, verifies the
  `ssign` transit digest and both unsigned Windows binary digests from `build-linux-windows`, runs
  `ssign` with `XDG_RUNTIME_DIR` pointing at a step-private directory under `$RUNNER_TEMP` (removed
  by an `EXIT` trap on step exit, including on failure, so `ssign`'s reusable OAuth session cache is
  not left on disk), signs both Windows executables in separate `ssign` processes with a
  five-variable environment allowlist, suppressed stdio and a five-minute timeout without running
  Cargo, and outputs both signed Windows binary SHA-256 digests.
- **Packaging and SBOMs (`package`, `ubuntu-latest`, `contents: read`, no secrets)**: verifies the
  pinned `syft` tarball and binary digests and all six binary SHA-256 digests (two Linux, two signed
  macOS, two signed Windows), packs all six archives deterministically (`root:root`, commit
  timestamp, `LICENSE` and `NOTICE`), generates a Syft SPDX JSON SBOM for each archive asserting
  `rmcp 3.11.0` and target-specific `windows-sys`, writes the 12-subject `checksums.txt`, and
  uploads **one** shared `release-bundle` artifact together with `checksums.txt`'s SHA-256.
- **Final archive verification (`verify-macos-archives` on `macos-latest` and `verify-windows` on
  `windows-latest`, parallel, no permissions, no secrets)**: both jobs download that same shared
  `release-bundle` artifact ID, reject any extra files, subdirectories or symlinks, check
  `checksums.txt`'s SHA-256, verify their platform's final archives against `checksums.txt`, inspect
  archive members and headers, enforce exact tar member metadata on macOS (`0755` for `proxenos`,
  `0644` for `LICENSE` and `NOTICE`, `root:root`, uid/gid `0`, no setuid/setgid/sticky bits, and no
  PAX extended headers or attributes), compare each embedded executable's SHA-256 against its own
  signing job (`sign-macos` or `sign-windows`), check `lipo -archs` (`x86_64` for amd64 and `arm64`
  for arm64) on macOS and PE machine types on Windows, natively verify signatures (`codesign` and
  `spctl` against the pinned Developer ID on macOS; `signtool` and `Get-AuthenticodeSignature`
  against the pinned Certum thumbprint on Windows), run the native binary with `--version`, and
  output the verified `checksums.txt` SHA-256. Both jobs must pass before publication runs.
- **Checksum signing, provenance and draft release (`publish`, `ubuntu-latest`, `contents: write`,
  `id-token: write`, `attestations: write`, no Cargo, no `environment: release`)**: runs isolated
  Python (`python3 -I`) from outside the downloaded bundle directory, downloads the same shared
  `release-bundle` artifact ID, rejects any unexpected entry (including subdirectories and symlinks),
  asserts `checksums.txt`'s SHA-256 equals both `verify-macos-archives` and `verify-windows` outputs
  and matches all 12 subject files, inspects all six final archives before signing to enforce the
  packager's member metadata (on all four `ustar` `.tar.gz` archives: mode `0755` for `proxenos`,
  `0644` for `LICENSE` and `NOTICE`, `root:root`, uid/gid `0`, no setuid/setgid/sticky bits, and no
  PAX extended headers or attributes; on both `.zip` archives: exact `external_attr` mode metadata
  `0755` for `proxenos.exe` and `0644` for `LICENSE` and `NOTICE`, with no special or symlink bits)
  and to confirm each embedded executable's SHA-256 matches its own build or signing job
  (`build-linux-windows`, `sign-macos`, or `sign-windows`) so any cross-architecture or target swap
  is caught before signing, checks **before** `cosign` signing and provenance attestation that
  neither tag `v<version>` nor any release (draft or published) exists, verifies the pinned `cosign`
  SHA-256, signs `checksums.txt` keylessly (`checksums.txt.sigstore.json`), records GitHub build
  provenance (`actions/attest-build-provenance` with `subject-checksums`) for the **12 archive and
  SBOM files listed in `checksums.txt`** (not `checksums.txt` itself), creates a new **draft**
  release (`tag_name: v<version>`, `target_commitish: <dispatched sha>`, `prerelease` when `version`
  contains a SemVer pre-release part), verifies draft state by numeric release ID, uploads all 14
  assets to that numeric draft ID, re-verifies draft state, `prerelease` flag and all 14 uploaded
  asset SHA-256 digests, and prints the expected 14-asset `(name, sha256:<digest>)` list together
  with the copy-ready verification and `PATCH` publication commands (real release ID, tag,
  `prerelease` value and dispatched commit SHA) in the run summary.

Publishing the draft: the owner does **not** click **Publish** in the GitHub UI (a writer could have
changed a pending draft's `target_commitish`, `prerelease` flag, or assets without creating the tag
ref). Before publishing, the owner runs three checks:

1. `git ls-remote origin 'refs/tags/v<version>'` — must return **empty** (do not compare SHAs: an
   annotated tag returns the tag object's SHA rather than the commit SHA).
2. `gh api repos/mateusz-klatt/proxenos/releases/<draft-id>` — must show `"draft": true`, the
   expected `"prerelease"` (`true` iff `<version>` has a SemVer pre-release part), `"tag_name":
   "v<version>"`, and `"target_commitish": "<dispatched-sha>"`.
3. Fetch all remote asset digests via `gh api --paginate
   repos/mateusz-klatt/proxenos/releases/<draft-id>/assets --jq '.[] | "\(.name)  \(.digest)"' |
   LC_ALL=C sort` and diff against the 14 expected `(name, sha256:<digest>)` pairs printed in
   `publish`'s summary (including `checksums.txt` and `checksums.txt.sigstore.json`); the diff must
   be empty.

Publication is then one owner-authorised API request that sets every field explicitly (`<true|false>`
derived from whether the validated SemVer has a pre-release part):

```sh
gh api -X PATCH repos/mateusz-klatt/proxenos/releases/<draft-id> -F draft=false -F prerelease=<true|false> -f tag_name=v<version> -f target_commitish=<dispatched-sha>
```

Consumer verification: verify `checksums.txt` with:

```sh
cosign verify-blob \
  --bundle checksums.txt.sigstore.json \
  --certificate-identity=https://github.com/mateusz-klatt/proxenos/.github/workflows/release.yml@refs/heads/main \
  --certificate-oidc-issuer=https://token.actions.githubusercontent.com \
  checksums.txt
```

Then verify downloaded archives and SBOMs against `checksums.txt`, and verify build provenance of
any of the 12 listed archive or SBOM files with `gh attestation verify <file> --repo
mateusz-klatt/proxenos`.

Partial-draft recovery: if `publish` fails after creating the draft release (leaving fewer than 14
assets), subsequent dispatches for that version fail closed because a draft for `v<version>` already
exists. Recover by finding the draft's exact numeric `<draft-id>` in the failed `publish` log or via
`gh api repos/mateusz-klatt/proxenos/releases`, confirming `"draft": true` and `"tag_name":
"v<version>"`, deleting only that draft with `gh api -X DELETE
repos/mateusz-klatt/proxenos/releases/<draft-id>`, and re-dispatching `release.yml` on `main`.

Pull requests touching the release files and manual `workflow_dispatch` runs of `release.yml` with
an empty `version` input exercise the six-target snapshot path across `macos-latest` and
`ubuntu-latest` with no secrets and read-only permissions, checking pinned tool tarball and binary
digests, static ELF linking, Mach-O and PE architectures, archive members, target-specific SPDX
crate metadata (including `rmcp 3.11.0`) and the 12-subject `checksums.txt`. Two manual-only
signing-proof workflows (`.github/workflows/signing-proof.yml` for Windows/Certum and
`.github/workflows/macos-signing-proof.yml` for macOS) run only on `workflow_dispatch` on `main`,
separate compilation from signing, chain unsigned and signed SHA-256 digests across build, sign and
verify jobs, enforce exact case-sensitive `refs/heads/main` before signing in `environment:
release`, verify the signed binaries natively, retain artifacts for one day, and never publish.
Proxenos never updates itself.

What this cannot do:

- Ruleset `28030033` on `main` requires a pull request and the four CI checks, but
  `required_approving_review_count` is `0`. A session holding the owner's token can open a pull
  request, wait for green checks, merge it into `main` and dispatch `release.yml`. Saying `main` is
  protected therefore means protected by pull requests and CI checks, not by an independent
  approving reviewer.
- The `release` environment restricts deployments to branch `main` (enforced by GitHub) and names
  the owner as required reviewer, but the fleet's agents hold the owner's `gh` token and
  `can_admins_bypass` is enabled on the environment. Deployment approval is therefore an agreement,
  not an enforcement mechanism against a session holding the owner's token.
- Ruleset `release tag creation` (`28075912`) blocks other accounts and the Actions `GITHUB_TOKEN`
  from creating `refs/tags/v*`, but its repository-admin bypass also covers the owner's token, which
  the fleet's agents hold. Against sessions holding the owner's token, tag creation is therefore an
  agreement rather than enforcement.
- Between the owner's pre-publication checks (`git ls-remote`, `gh api .../<draft-id>`, and the
  14-asset digest diff) and the `PATCH` publication request there is no lock. Because the `PATCH`
  request sets `tag_name`, `target_commitish` and `prerelease` explicitly, an edit to those fields
  made in between is overwritten rather than inherited (and `release tag creation` blocks non-admin
  creation of the tag ref in between). However, the `PATCH` has no asset precondition: edits to
  draft assets made after the 14-asset digest comparison and before the `PATCH` executes remain a
  trust assumption.
- If `main` advances while a release is waiting on signing or approval and a newer commit modifies
  workflow files, creating the draft release targeting the earlier dispatched commit SHA with the
  Actions `GITHUB_TOKEN` can fail (operational limit). Recovery is to dispatch `release.yml` on
  `main` again (deleting any partial draft by numeric ID first if one was created).
- Because GitHub hides draft releases from tokens with only `contents: read`, `check-release`
  detects existing tags and published releases before compilation, whereas an existing draft for
  `v<version>` is detected only in `publish` (`contents: write`) — still before `cosign` signing and
  provenance attestation, though after compilation and platform signing.
- Checking `ssign`'s SHA-256 between `signer` and `sign-windows` verifies transit integrity of the
  artifact built in the same workflow run; it does not compare against a pre-established
  reproducible digest.
- Workflow logs, Rekor transparency entries for `checksums.txt`, GitHub build provenance
  attestations for the 12 listed files and one-day handoff artifacts are public before the draft
  release is published (the git tag itself is created only when the owner publishes the draft).
- Restricting `ssign`'s child environment to five variables filters environment variables on a
  shared runner; it is not a process sandbox.
- Native signature verification checks valid signatures from the pinned identities; it does not
  establish Windows SmartScreen reputation, quarantined browser startup under macOS Gatekeeper, or
  execution of the Windows arm64 or macOS x86-64 binaries on runners of the other architecture.

Measured on 2026-11-25:

- CI on commits `1defff9` and `c2ab048` (following `45605931105` on `f98eed2` and CI on `6733350`):
  all seven CI checks passed — Linux, macOS and Windows CI, Sonar, and both `snapshot-darwin`
  (`macos-latest`) and `snapshot-linux-windows` (`ubuntu-latest`) — building all six targets with
  direct verified `goreleaser` and `syft` downloads over `--proto '=https' --tlsv1.2`, verifying
  static ELF output, Mach-O and PE architectures, archive contents, target-specific `cargo-auditable`
  SPDX crate metadata (including `rmcp 3.11.0`), native execution of the Linux x86-64 and macOS
  arm64 binaries, and a 12-subject `checksums.txt`.
- Locally on Linux x86-64 after the round-5 fixes: `actionlint` passed on all three release and
  signing-proof workflows; `goreleaser check` validated `.goreleaser.yaml` across all build-group
  settings; tagless `GORELEASER_CURRENT_TAG=v0.0.0 goreleaser build --clean --single-target
  --skip=validate` succeeded; a 4-target `PROXENOS_BUILD_GROUP=linux-windows` GoReleaser snapshot
  passed with verified static ELF/PE outputs, SPDX SBOMs listing `rmcp 3.11.0` and an 8-subject
  `checksums.txt`; `ustar` tar and ZIP metadata verification passed positively and rejected unsafe
  permissions, non-root ownership and both global and member PAX extended headers/ACLs; and the
  14-asset digest diff and `PATCH` summary commands were syntax- and behavior-checked locally.

Not measured: both signing-proof workflows (`signing-proof.yml` and `macos-signing-proof.yml`), a
real `workflow_dispatch` release on `main` through `release.yml`, quarantined macOS Gatekeeper
startup, and Windows SmartScreen.

### D12. Every line is tested, through the binary

- Line coverage is 100% on each platform for the code that platform compiles, enforced in CI.
- Behavioural tests start the compiled binary and speak MCP to it. Providers run against a stub
  CLI built from this crate, which records what it was started with.
- Tools that exist only to test the MCP layer, such as one that panics, are compiled in only with
  the `test-tools` feature. CI lints with and without it and measures coverage with it; a release
  is built without it.
- During the migration, the same scenarios are replayed against claude-delegator 3.5.0 and the
  results compared, apart from a short list of intended differences.
- No test starts a real agent CLI or spends a model call.

### D13. Migration

The order of work:

1. The contract, proven before it is multiplied. Both MCP lifecycles; the supervisor on Linux,
   macOS and Windows, with a test in which a grandchild tries to outlive its delegation; the
   result of D2; background delegations (D17) and accounting (D18); remembered settings (D19);
   the Codex and Antigravity providers; the Claude Code registration. All of it through the
   compiled binary against stub CLIs, at full coverage on three platforms.
2. The other five providers, and the comparison with claude-delegator.
3. `usage` and the catalogue.
4. Registration for the other six clients, and `doctor`.
5. Releases.
6. The four hosts, one at a time.

The first version of this section ported all seven providers before the contract was proven. Both
reviewers of vote 4 wanted the contract tested before the remaining providers are ported; one
added that otherwise every provider would be written twice.

While both projects run on one host, Proxenos reads and sets claude-delegator's delegation-depth
variables as well as its own, so that a chain passing through both still stops.

### D14. One crate

One crate, edition 2024, a pinned toolchain. Errors are typed at the boundaries and turned into an
exit status or an MCP result in one place. Diagnostics go to standard error. `unsafe` is denied
except in the platform modules of D6.

### D15. Roles are Proxenos's own text

Role texts are written for Proxenos, model-neutral, one per role. A call may also name no role and
carry only `instructions`.

The first three to ship are the code reviewer, the debugger and the claim verifier, as the owner
accepted on 2026-11-25. The rest of the roster is not decided. Three families, in four answers,
agreed in vote 2 on code review, debugging, verification of claims, architecture, security and plan
review, and on a redesigned cross-discipline role. The strongest model of each family would ship the
code reviewer, the debugger and the claim verifier first. They did not agree on a scope analyst, a
challenger or a separate test role.

A role's text says what `implementation` allows it, and tells a role whose worth is its
independence, such as one that verifies a claim, to stay advisory. That is an instruction: under
full access nothing prevents the write.

### D16. Progress is reported through Iris, when the caller asks

A caller may pass `coordination` with its Iris agent name. Proxenos then adds a reporting contract
to the delegation, and the provider reports its start, progress and end to that agent through its
own Iris session. Proxenos holds no Iris session and sends nothing itself. Without `coordination`,
or without Iris on the provider's side, the delegation runs the same and reports nothing. Iris
needs no knowledge of Proxenos.

A provider's message that it has finished is a cue to collect the result, not proof of it: a
provider can send it before it exits, or die without sending it. The result comes from Proxenos
(D17). The reporting contract asks for a line of status without the answer, so that the answer
need not reach the caller twice. A provider may still omit the message, or repeat the answer in
it.

### D17. A delegation can run in the background

With claude-delegator a client waits for a delegation to finish, so asking three providers means
three waits in a row, or a subagent for each that does nothing but wait. Claude Code now moves a
call that runs past two minutes to the background by itself, but as a task that its users found
opaque, that puts the whole answer into the context, and that dies with the session.

- A start or a reply with `wait: false` returns a handle at once, if it is admitted. One that
  waits gives no handle while it runs. `wait` is true by default, so that one consultation can
  still be one call.
- `await` takes handles and a condition, any or all, and waits up to a limit set by the server:
  20 seconds. A client may give up sooner. When a wait runs out, no unfinished delegation is
  cancelled, and waiting longer is another call. It returns the state of every handle: running,
  finished, failed, cancelled, or unknown. Reading a result does not consume it.
- `cancel` asks for one delegation to be stopped. Cancelling an `await` only stops the waiting.
- A handle is not a `thread_id`. It belongs to one server process and means nothing to another.
- On a delegation's deadline, on `cancel`, or when the client closes the connection, the
  supervisor starts the stop that D6 describes, within D6's limits. On Linux and macOS a sampled
  crossing of a memory limit starts it too. On Windows the system refuses an allocation above the
  job's limit instead, and the CLI may carry on (D18).
- Finished results are kept in memory, bounded in number, size and time.
- At most three delegations run at once in a server. A start or a reply that would be a fourth is
  refused at once with a failure of kind busy (D2), whether or not it waits. Nothing is queued:
  a queue would add a state that a handle does not have, and a wait that the caller cannot see.

The refusal follows the count over votes 3 and 6: eight of nine positions chose it, and one a
short queue. The 20 seconds are provisional, taken by Claude (Fable 5.2, max) on the count of vote
6. Four positions chose 20 seconds. Gemini (Hydrogen) proposed 45 by default and at most 60,
Gemini (Argon) 55 by default and at most 120, DeepSeek 60, and Gemini (Carbon) and Zhipu up to
120. The limit starts at the low end because a longer wait can run past the limit that a client
is configured with for one call. The shortest default that a client's documentation gives is 60
seconds.

What this cannot do: when the server exits, its handles and the results it held are gone. Nothing
reliably wakes a client that does not call `await`: an Iris message that a provider has finished
(D16) is voluntary.

If a client times out a `wait: true` start or reply, the caller has no handle for that
delegation. A caller whose `await` times out keeps its handles. Kimi Code documents a
default of 60 seconds, as Kimi pointed out in vote 6. What becomes of the delegation depends on
whether the client also sends a cancellation, which has not been measured for any of the seven
(section 5). Not decided: what to do about it. Kimi's proposals were a handle before a bounded
wait, or a larger limit for the server, set in the client's registration (D10).

MCP defines a task mechanism of its own for long calls. Not every client implements it, so
Proxenos uses plain tool calls, which all of them do.

### D18. What a delegation used is accounted, and may be limited

Memory grows in the delegated CLIs, not in Proxenos, so Proxenos accounts for the memory and CPU
time of each delegation and returns them with the result, saying how they were measured: totals of
the job on Windows, samples of the supervised process group on Linux and macOS.

- **Windows**: the job object of D6 reports the peak of committed memory and the CPU time of the
  job. With a limit set, the system refuses an allocation that would exceed it. The CLI may fail
  on that refusal or carry on.
- **Linux and macOS**: the processes of the supervised process group are sampled. Samples can
  miss a peak, a short-lived child, and a descendant that has left the group. With a limit set, a
  sample above it starts the stop that D6 describes. That is a threshold checked between samples,
  not a ceiling.

No memory limit applies unless the user sets one.

### D19. A session remembers how it was started

A reply is started with those settings of its start that the provider's reply command accepts
(D5): model, effort, directory, access, role and mode, unless the reply says otherwise. Proxenos
reapplies what it passed at the start. Which model actually ran is what the provider reports.
With claude-delegator each provider needed different settings repeated, and a forgotten one
silently changed the model.

Where the start passed no model (D8), there is none to reapply. Not decided: what a reply does
when it must name a model (D5) and its start passed none. It could repeat the model that the
provider reported for the start, where the provider reported one, or ask the caller for one.
ADR 002 opened this question, and neither a voter nor the owner has answered it.

The settings of a session are kept in a file in the user's state directory, without secrets, for a
limited time. When a session is not found there, Proxenos asks for the settings and supplies no
defaults of its own. It cannot check that the settings a caller then gives match the forgotten
start. A reply may change the mode where the CLI accepts it. Codex keeps the start's developer
instructions, including its mode, for the session. Proxenos refuses a change before dispatch.

At milestone 1 step 3 the server remembers starts only in process memory. Restarting it loses
that continuity. A reply to an unremembered session must supply its directory, access, mode and
the optional settings used at the start. Proxenos cannot verify these values against that start.
Pending the owner decision above, an Antigravity reply with no remembered start model must
supply a model explicitly. Proxenos never infers one from provider output.

The file was a provisional choice of vote 4, where one reviewer would keep the settings only in
the server's memory, and vote 6 backed it: eight of nine positions are for a file. How long it is
kept is not fixed: three positions named seven days, and one 24 hours without use.

## 3. Later, and not at all

Later, each when it can be shown to work: a warning before a delegation starts on a provider that
is at its quota; a throwaway worktree for reviews, which is not read-only and will not be called
that; file inputs; checks for a stale daemon of a provider. File inputs stay here provisionally.
Over votes 4 and 6, four families are for adding them later and three for leaving them out
altogether; counted by answers it is four to five.

Not Proxenos's job: choosing another provider when one fails; prompt templates and fan-out, which
a caller composes from starts and `await`; refusing a review by the family that wrote the code; the
scope of Iris claims.

## 4. What Proxenos cannot guarantee

- `mode: advisory` is a sentence in a prompt. Under `access: full` the provider can still write
  and run anything.
- `access: read-only` is only what each CLI enforces, from an operating-system sandbox on one to
  nothing at all on another, where Proxenos refuses the value instead of pretending.
- The delegation-depth guard stops accidental loops. A provider with full access can start
  another CLI by itself.
- Some CLIs load instruction files from the working directory with no way to turn that off.
  Delegating into a repository you do not trust exposes the provider to what is written there.
- On a start, when neither the call nor the user's overlay names a model, Proxenos passes none
  (D8); replies follow D19. The provider's CLI then runs its own default or refuses to start. Proxenos does not choose that
  default, and it can change with a release of the CLI or with the user's configuration of it. A
  model that is named is a request: the result names the one that ran only where the provider
  reports it.
- A call with `wait: true` lasts as long as its delegation. A client that limits the time of one
  call may end it first and leave the caller without a handle (D17).

## 5. Not yet known

- Whether Codex passes Proxenos's environment variables to the MCP servers it starts. If it
  filters them, the depth guard needs them named in its registration.
- How Grok and Cursor treat a Claude Code plugin they load by themselves, and whether that offers
  them their own provider.
- How the seven clients run several tool calls from one message, and how long each lets one call
  last. Three of them described their own CLI, two in vote 3 and one in vote 6, from source and
  documentation: one lets calls overlap only when a tool declares itself read-only or the
  registration opts in, one runs them in parallel, and the documentation of the third does not
  say. The default limits they gave for one call run from 60 to 300 seconds, and each can be
  changed. None of this has been measured.
- Whether a client that ends a call at its own limit also sends a cancellation. What becomes of a
  delegation whose call was waiting depends on it (D17).
- Whether a client shows a model the structured content of a result as well as its text. The
  documentation of one says that it does, and that it leaves the structured copy out when the
  text holds the same value. That has not been measured either.

# Architecture

Proxenos lets one coding agent consult another. This document records what has been decided, why,
what was measured, and what is still unknown. A decision here stands until a record in `docs/adr/`
changes it.

**Status: design.** Apart from the crate skeleton, nothing below is built yet. Sentences in the
present tense describe the design, not a running program.

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
  tool list or a call first needs it.

Why: one registration for each client instead of seven or eight, and nothing to repair when a CLI
is installed or removed. A crash now takes every provider's tools with it for that session, which
the per-provider processes of claude-delegator avoided; a panic in one call is therefore caught
and turned into that call's error (see D4).

### D2. The tool surface uses Proxenos's own names

Each provider has two tools, `<provider>` to start a delegation and `<provider>-reply` to continue
one. `usage` reports subscription windows. Parameters are in snake_case:

| Parameter | Meaning |
|---|---|
| `prompt` | The task. |
| `expert` | The name of a role. The server supplies the role's text. |
| `mode` | `advisory` or `implementation`. It adds a sentence of instruction. It is advice to the provider, never a permission. |
| `instructions` | Free text from the caller, added after the role. |
| `access` | `full` or `read-only`. What `read-only` enforces differs by provider and is stated in each tool's schema. |
| `cwd`, `model`, `effort`, `timeout_ms` | As named. Which of them a provider accepts differs. |
| `thread_id` | On a reply: the session to continue. |
| `coordination` | Optional progress reporting through Iris (D16). |

The answer is a JSON object with `thread_id` and `content`, sent as the text of the result, because
MCP clients show a model only the text. Where the protocol revision allows it, the same object is
also sent as structured content.

Why: `workspace-write`, the old name for the default, meant full unattended access and said
otherwise. The registration had to be renamed anyway, so the names change once.

### D3. Tokio, and a supervisor that owns each child

Every delegation has a deadline and a cancellation token. A separate task owns the child process,
its pipes and its temporary files, so that a request handler that goes away cannot leave a child
running.

### D4. MCP through `rmcp`, pinned and kept behind one module

The MCP revision of 2026-07-28 removed the `initialize` handshake: each request now carries its
protocol version, and a server must answer `server/discover`. Clients of the earlier revisions
still open with `initialize`. Proxenos serves both.

It uses the official Rust SDK, `rmcp`, pinned to an exact version and reached only through the
`mcp` module, so that it can be replaced without touching the providers.

Measured on 2026-11-24 with a throwaway server on Linux, `rmcp` 3.11.0, features `server` and
`transport-io`, 190 scripted checks with hand-written frames:

- One process served `initialize` clients (2024-11-05 to 2025-11-25) and clients of 2026-07-28,
  with a tool list built at run time and schemas passed through unchanged.
- An unsupported version got the error the specification asks for in the new lifecycle, and the
  newest supported version in the old one.
- 65 crates, none of which needs a C compiler. A stripped release binary of that server was 3.4 MB.

What the `mcp` module has to do itself, because the library does not:

- **End of input.** The library waits a fixed five seconds after standard input closes before it
  cancels running calls. Proxenos watches its input and stops its children at once.
- **Cancellation.** The library signals a token and does not stop the handler. Every handler
  waits on that token.
- **Panics.** A handler that panics is never answered. Proxenos catches the panic and answers
  with an error.
- **Identity.** The server name and version are set; the default is the library's own.

Known differences from claude-delegator, accepted: a line that is not JSON is ignored instead of
answered with a parse error, and a `tools/call` without a name is "method not found" instead of
"invalid params".

Not measured: Windows, macOS, and any real client.

### D5. Seven providers, each with the same four seams

The set of providers is closed. Each provider module decides four things and nothing else: its
tool schemas, the command line it builds, how it reads the CLI's output, and how it tells failure
from success. It spawns nothing and speaks no MCP.

Where providers differ, each one's tests assert its own behaviour through the compiled binary.
Antigravity's reply must name a model; Cursor's must not. A shared test could assert neither.

### D6. Children are supervised to the end

- **Unix**: each child starts in a new session, so it has no terminal to prompt on. Stopping it
  means a termination signal to its process group, then a kill signal after three seconds.
- **Windows**: each child runs in a job object that the system closes when Proxenos dies, so its
  descendants die with it. The child is in the job before it runs any code, and it gets no
  console window. There is no polite stop on Windows: the job is terminated.
- **Everywhere**: standard input is closed, output is captured as bytes up to a limit, and
  temporary files are removed only after the child is reaped.

What this cannot do: on Unix, if Proxenos itself is killed outright, its children survive, and a
descendant that leaves the process group is not reached. Stopping a child does not undo what it
wrote.

Not decided: how the Windows child enters its job before it runs. It will be settled by a test on
a Windows runner in which a grandchild tries to escape.

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

### D9. Usage is read, never spent

`usage` reports each CLI's subscription windows without running a model. It needs a JSON-RPC
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

Six targets: Linux, macOS and Windows on x86-64 and arm64, with the Linux binaries linked
statically. GoReleaser builds them with its Rust builder. Windows binaries are signed with the
Certum certificate, macOS binaries with a Developer ID and notarized, the checksums with keyless
cosign, and each archive has an SBOM. A tag produces a draft that the owner publishes. Proxenos
never updates itself.

Not measured: any of this for Proxenos. The cross-builds are the first thing to try.

### D12. Every line is tested, through the binary

- Line coverage is 100% on each platform for the code that platform compiles, enforced in CI.
- Behavioural tests start the compiled binary and speak MCP to it. Providers run against a stub
  CLI built from this crate, which records what it was started with.
- During the migration, the same scenarios are replayed against claude-delegator 3.5.0 and the
  results compared, apart from a short list of intended differences.
- No test starts a real agent CLI or spends a model call.

### D13. Migration

The order of work: the core with the Codex provider and the Claude Code registration; the other
six providers and the comparison with claude-delegator; `usage` and the catalogue; registration for
all seven clients and `doctor`; releases; then the four hosts, one at a time.

While both projects run on one host, Proxenos reads and sets claude-delegator's delegation-depth
variables as well as its own, so that a chain passing through both still stops.

### D14. One crate

One crate, edition 2024, a pinned toolchain. Errors are typed at the boundaries and turned into an
exit status or an MCP result in one place. Diagnostics go to standard error. `unsafe` is denied
except in the platform modules of D6.

### D15. Roles are Proxenos's own text

Role texts are written for Proxenos, model-neutral, one per role. Which roles ship is not decided.

### D16. Progress is reported through Iris, when the caller asks

A caller may pass `coordination` with its Iris agent name. Proxenos then adds a reporting contract
to the delegation, and the provider reports its start, progress and end to that agent through its
own Iris session. Proxenos holds no Iris session and sends nothing itself. Without `coordination`,
or without Iris on the provider's side, the delegation runs the same and reports nothing. Iris
needs no knowledge of Proxenos.

## 3. Proposed, not decided

### P1. Delegations in the background

Today a client waits for a delegation to finish, so asking three providers means three waits in a
row, or a subagent for each that does nothing but wait. The proposal: a start call with
`wait: false` returns a handle at once; an `await` tool returns the finished results of the
handles it is given, waiting up to a limit; `cancel` stops one. Iris (D16) is what wakes the
client when a provider reports its end.

Open: whether results outlive the server process, and how this relates to the task mechanism MCP
itself defines, which not every client may implement.

## 4. What Proxenos cannot guarantee

- `mode: advisory` is a sentence in a prompt. Under `access: full` the provider can still write
  and run anything.
- `access: read-only` is only what each CLI enforces, from an operating-system sandbox on one to
  nothing at all on another, where Proxenos refuses the value instead of pretending.
- The delegation-depth guard stops accidental loops. A provider with full access can start
  another CLI by itself.
- Some CLIs load instruction files from the working directory with no way to turn that off.
  Delegating into a repository you do not trust exposes the provider to what is written there.

## 5. Not yet known

- Whether Codex passes Proxenos's environment variables to the MCP servers it starts. If it
  filters them, the depth guard needs them named in its registration.
- How Grok and Cursor treat a Claude Code plugin they load by themselves, and whether that offers
  them their own provider.
- How the seven clients run several tool calls from one message: in parallel or one by one.

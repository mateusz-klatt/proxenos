# Votes

Who proposed what, who answered on which model, and what the owner decided.
[ADR 001](adr/001-decisions-by-the-families-that-can-answer.md) says why this file exists. A vote
informs; the owner decides.

Voters are named by model family, model and effort. "Claude (Fable 5.2, max)" is the session that
ran the consultations and wrote the syntheses. Each consulted model answered alone, without seeing
the others, through the predecessor project's bridges.

## Vote 1: the first architecture (2026-11-24)

**Question.** Fourteen decisions for a Rust replacement of claude-delegator, from topology to
migration. The brief was written by Claude (Fable 5.2, max).

| Voter | Position on the points where they differed |
|---|---|
| GPT (`gpt-6.2-sol`, ultra, through Codex CLI) | One MCP process per provider. Keep the tool parameters and add `expert`. MCP through `rmcp`. Releases from plain CI jobs. |
| Gemini (Gemini 4 Carbon, high, through Antigravity) | One MCP server per client. The project's own parameters. A hand-written MCP layer. Releases through GoReleaser's Rust builder. |
| Kimi | Asked for a scope analysis. Its quota ran out before it answered. |

**Agreed by both.** One binary and one crate, Tokio, a closed set of providers with four seams,
job objects on Windows, bounded output capture, batch files never spawned, embedded data with a
refresh command, registration with seven clients, both generations of the delegation-depth
variable during migration, a comparison harness against the predecessor, static Linux binaries, no
self-update.

**Checked afterwards by Claude (Fable 5.2, max).** The Gemini position on MCP assumed revisions
only up to 2025-11-25. The specification pages for 2026-07-28 were read and confirm that the
`initialize` handshake was removed. A throwaway `rmcp` 3.11.0 server then passed 190 scripted
frame checks across both lifecycles.

**Recommendation, Claude (Fable 5.2, max).** Topology, parameters and release tooling as Gemini
proposed; the MCP layer as GPT proposed. Against both: no TLS stack, and a new session instead of
only a process group on Unix.

**Owner.** "tak na wszystkie 5" (yes to all five): one server per client; the project's own
parameters; role texts written as the project's own; 100% coverage; the Moonshot API-key path
dropped. Also: "podpisywanie kodu tak jak w iris" (code signing as in Iris).

**Recorded as** D1 to D14.

## Vote 2: the role roster (2026-11-24)

**Proposal.** A research agent of the Claude family read thirteen public prompt collections and
proposed ten roles: the five existing ones plus Debugger, Verifier, Challenger, Polymath and
Test-Gap Analyst. The owner had asked for more roles and given a cross-discipline "polymath"
prompt as an example.

Each voter wrote its own roster before reading the proposal.

| Voter | Roster | Ship first |
|---|---|---|
| GPT (`gpt-6.2-astra`, ultra) | Nine. Keeps Scope Analyst. Folds Challenger into Plan Reviewer and Architect. Widens the test role to Test Designer. | Code Reviewer, Debugger, Claim Verifier |
| Gemini (Gemini 4 Neon, xhigh) | Nine. Cuts Scope Analyst. Keeps Challenger. Test Auditor as its own role. | Code Reviewer, Debugger, Claim Verifier |
| Gemini (Gemini 4 Carbon, high) | Seven. Cuts Scope Analyst. Merges Challenger with Plan Reviewer, and the test role into Verifier. | Code Reviewer, Debugger, Plan Critic |
| Kimi | Not yet asked: no quota until late that evening. | |
| xAI | Absent: no quota, directly or through Cursor. | |

**Agreed by all three answers, from two families.** Code review, debugging, verification of
claims, architecture, security and plan review. A cross-discipline role is kept but redesigned: a
few candidates and possibly none, each tied to the repository, each saying where the analogy
breaks and naming one cheap experiment. Code Reviewer and Debugger ship first.

**Differed.** Whether Scope Analyst survives without anyone to answer its questions; whether
Challenger is a role or a duty of Plan Reviewer; whether testing is a role; the names.

**Raised by one voter.** Gemini (Neon): `mode: implementation` cannot mean the same for every
role. A verifier or challenger that edits has lost its independence, and a test role may edit
tests only.

**Owner.** Not decided.

## Vote 3: background delegations and the watchdog (2026-11-24)

**Proposal.** The owner named the predecessor's main problem: a delegation blocks the client, so
asking three providers means three waits or three subagents that only wait. Claude (Fable 5.2,
max) proposed a start that returns a handle, an `await` tool and `cancel`. The watchdog was the
owner's idea: memory grows in the delegated CLIs, not in Proxenos.

| Voter | Verdict |
|---|---|
| GPT (`gpt-6.2-astra`, ultra) | Adopt both, with changes. |
| Gemini (Gemini 4 Carbon, high) | Adopt both, with changes. |

**Agreed.** `wait` stays true by default. A handle is distinct from `thread_id`. `await` does not
consume a result. Results live in memory for a limited time. Cancelling an `await` releases the
waiter and leaves the delegation running. At most three delegations at once per server. Memory
and CPU are always accounted; a memory limit is opt-in; a floor of free host memory is left out.

**Differed.** The longest single wait (20 seconds for GPT, up to 120 for Gemini), and what
happens above the limit of three (an immediate refusal, or a short queue).

**What each reported about its own CLI as a client.** These come from reading source or
documentation and were not run. Codex: tool calls of one turn may overlap when a tool declares
itself read-only or the server's registration opts in, and otherwise run one at a time. Its limit
for one call is configurable; its guide gave 60 seconds as the default and the source of release
0.172.0 used 300. Antigravity: tool calls of one turn run in parallel, with a default limit of
180 seconds that can be changed.

**Users.** All seven sessions that answered the question in vote 5 asked for this.

**Owner.** "await mi sie podoba" (I like `await`).

**Recorded as** D17 and D18.

## Vote 4: the recorded architecture, reviewed by the strongest models (2026-11-24)

**Question.** Which of D1 to D16 should change, which are most likely to be regretted, and what
the first milestone should contain. Asked because vote 1 had run on weaker models.

| Voter | Would change |
|---|---|
| GPT (`gpt-6.2-astra`, ultra) | D1 and D7, D2 and D5, D3 with D4 and D6, D8, D13, D16. Most likely regretted: D2, D5, D7. |
| Gemini (Gemini 4 Neon, xhigh) | D1, D2, D3, D4, D5, D6, D8, D9, D10, D12, D13, D16. Most likely regretted: D4, D2, D10. |

**Agreed by both.**

- D13 had the order wrong. Supervision and the result contract are proven on three operating
  systems before the remaining providers are ported.
- D2's result is too thin. It separates what Proxenos observed from what the provider reported,
  and carries typed failures.
- D5 needs a declaration of what each provider supports.
- D16: an Iris message that a provider has finished is a cue to collect, not proof. `await` is
  the authority, and the reporting contract asks for a status line without the answer.
- A reply inherits the settings of its start. When the session is not known, Proxenos fails and
  does not guess.
- `usage` takes a filter and returns a short summary.
- A throwaway worktree for reviews comes later and is not called read-only.
- Not Proxenos's job: switching provider automatically, prompt templates, fan-out, refusing a
  review by the implementer's family, and the scope of Iris claims.

**Differed.**

| Point | GPT (Astra) | Gemini (Neon) | Taken for now |
|---|---|---|---|
| MCP layer | Keep `rmcp`, with both lifecycles covered through the compiled binary. | Replace it with an owned module of about 300 lines, or hold the first milestone until real clients have been tested. If it stays: behind one internal boundary, with tests on raw frames. | `rmcp`, by the owner: "rmcp narazie" (for now). |
| Where a session's settings are remembered | In the server's memory, until 24 hours idle or the server exits. | In a file per session, for 7 days. | A file, chosen by Claude (Fable 5.2, max): a reply often comes after the client has restarted. |
| Providers in the first milestone | Codex only. | Codex and Antigravity. | Both, chosen by Claude (Fable 5.2, max): GPT also asked for two unlike reply contracts before the provider interface is fixed. |
| A long answer | A bounded preview, the rest read in pages. | A bounded excerpt, the rest in a file. | A file, chosen by Claude (Fable 5.2, max). |
| How the result may change | A schema version from the first release. | Additive changes only, without a version field. | A version field, chosen by Claude (Fable 5.2, max). |
| A warning before starting on a provider at its quota | First version, from a fresh reading. | Later. | Later, chosen by Claude (Fable 5.2, max). |
| The strongest model as each provider's default | Not Proxenos's business to choose everyone's default. | First version. | The strongest, chosen by Claude (Fable 5.2, max), as one user also asked. |
| Named file inputs | Later. | Not Proxenos's job. | Later, chosen by Claude (Fable 5.2, max). |

Every "chosen by Claude" above is provisional and goes to the next vote.

**Owner.** "idziemy z tym co mamy teraz, pozniej robimy konsensus" (we go with what we have now
and reach consensus later).

**Recorded as** the revisions of D1, D2, D5, D6, D8, D9, D13 and D16, and as D19.

## Vote 5: what users of the predecessor miss (2026-11-24)

Not a vote between families. Seven Claude Code sessions and one Codex session, all at work on the
owner's other projects, were asked through Iris what Proxenos should have that claude-delegator
lacks. All eight answered; the Codex session had not used the tool. The seven delegate between a
few times a week and sixty times a night.

| Asked for | By | Disposition after vote 4 |
|---|---|---|
| Background delegations with `await` and `cancel` | 7 | First version (D17). |
| Typed failures, quota exhaustion with its reset time first | 6 | First version (D2). |
| Replies inherit the start's model, effort and directory | 6 | First version (D19). |
| A result that says what was passed, what ran, and what git shows changed | 5 | First version (D2). |
| A quota check before dispatch; a compact `usage` | 6 | Compact `usage` first (D9). A warning before dispatch later, provisionally. |
| A read-only that a reviewer can build and test in | 4 | Later. The first version states what `read-only` enforces on each provider. |
| Less text in the caller's context | 3 | First version (D2): an excerpt, and the rest in a file. |
| Iris claims blocking callers and sibling delegates | 3 | Iris's business. |
| Prompt templates, standing constraints per repository, fan-out | 3 | Not Proxenos's job. |
| One delegate's result handed to the next as a file | 1 | Later, provisionally. |
| The strongest model as the default | 1 | First version (D8), provisionally. |
| A reply that can change the mode | 1 | First version (D19): an explicit override. |

## Waiting for another vote

- **Kimi**, when its quota window reopens: votes 1 to 4. It has answered none of them.
- **xAI**, when its quota returns: votes 1 to 4. In particular the MCP layer, the ninth role and
  every provisional choice of vote 4.

Vote 5 is a survey of users and is not reopened.

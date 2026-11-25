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
| Kimi (K3.1 Pro; it has no effort setting), late that evening, after its quota reopened | Nine. Keeps Scope Analyst, rewritten to give a recommended reading instead of questions. Folds Challenger into Plan Reviewer and Architect. Test Designer as its own role. Dropped the Performance Analyst of its own first roster. | Code Reviewer, Claim Verifier, Debugger |
| xAI | Absent: no quota, directly or through Cursor. | |

Kimi answered in a checkout that held none of the other answers, and was told not to open them.

**Agreed by all four answers, from three families.** Code review, debugging, verification of
claims, architecture, security and plan review. A cross-discipline role is kept but redesigned: a
few candidates and possibly none, each tied to the repository, each saying where the analogy
breaks and naming one cheap experiment. Code Reviewer and Debugger ship first.

**Agreed by the strongest model of each of the three families.** The first three to ship are
Code Reviewer, Debugger and Claim Verifier.

**Differed.** Whether Scope Analyst survives without anyone to answer its questions (GPT and Kimi
keep it, both Gemini answers cut it); whether Challenger is a role (Gemini Neon) or a duty of Plan
Reviewer and Architect (GPT, Kimi, Gemini Carbon); whether testing is a role (GPT, Kimi and Gemini
Neon) or part of the verifier (Gemini Carbon); the name of the cross-discipline role, for which
four answers gave four names.

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
happens above the limit of three (an immediate refusal, or a short queue). Both went to vote 6.

**What each reported about its own CLI as a client.** These come from reading source or
documentation and were not run. Codex: tool calls of one turn may overlap when a tool declares
itself read-only or the server's registration opts in, and otherwise run one at a time. Its limit
for one call is configurable; its guide gave 60 seconds as the default and the source of release
0.172.0 used 300. Antigravity: tool calls of one turn run in parallel, with a default limit of
180 seconds that can be changed. Kimi Code, as Kimi reported in vote 6: one call is limited to 60
seconds by default, which can be changed, and its documentation does not say whether the calls of
one turn overlap or whether a cancellation is sent.

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
- A reply inherits the start settings that the provider supports. When the record is missing, the
  caller has to supply the required settings before the reply can run; Proxenos does not guess.
- `usage` takes a filter and returns a short summary.
- A throwaway worktree for reviews comes later and is not called read-only.
- Not Proxenos's job: switching provider automatically, prompt templates, fan-out, refusing a
  review by the implementer's family, and the scope of Iris claims.

**Differed.**

| Point | GPT (Astra) | Gemini (Neon) | Taken for now |
|---|---|---|---|
| MCP layer | Keep `rmcp`, with both lifecycles covered through the compiled binary. | Replace it with an owned module of about 300 lines, or hold the first milestone until real clients have been tested. If it stays: behind one internal boundary, with tests on raw frames. | `rmcp`, by the owner: "rmcp narazie" (for now). Vote 6: seven of nine keep it for the first milestone. |
| Where a session's settings are remembered | In the server's memory, until 24 hours idle or the server exits. | In a file per session, for 7 days. | A file, chosen by Claude (Fable 5.2, max): a reply often comes after the client has restarted. Vote 6: eight of nine for a file. |
| Providers in the first milestone | Codex only. | Codex and Antigravity. | Both, chosen by Claude (Fable 5.2, max): GPT also asked for two unlike reply contracts before the provider interface is fixed. Vote 6: eight of nine for both. |
| A long answer | A bounded preview, the rest read in pages. | A bounded excerpt, the rest in a file. | A file, chosen by Claude (Fable 5.2, max). Vote 6: six of nine for it. Still provisional. |
| How the result may change | A schema version from the first release. | Additive changes only, without a version field. | A version field, chosen by Claude (Fable 5.2, max). Vote 6: eight of nine for it. |
| A warning before starting on a provider at its quota | First version, from a fresh reading. | Later. | Later, chosen by Claude (Fable 5.2, max). Vote 6: eight of nine for later. |
| The strongest model as each provider's default | Not Proxenos's business to choose everyone's default. | First version. | The strongest, chosen by Claude (Fable 5.2, max), as one user also asked. Vote 6: seven of nine against it. D8 now has Proxenos choose no model. |
| Named file inputs | Later. | Not Proxenos's job. | Later, chosen by Claude (Fable 5.2, max). Vote 6: four of nine for later, five for not Proxenos's job. Still provisional. |

Every "chosen by Claude" above was provisional and went to the next vote, which was vote 6. The
last column now says what that vote made of it.

**Owner.** "idziemy z tym co mamy teraz, pozniej robimy konsensus" (we go with what we have now
and reach consensus later).

**Recorded as** the revisions of D1, D2, D3, D5, D6, D8, D9, D13 and D16, and as D19.

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

## Vote 6: the architecture, put to the families that had not voted, and to more models of one that had (2026-11-24 and 25, UTC)

**Question.** Judge each of D1 to D19 as sound, to change or wrong, and name the three most likely
to be regretted in a year. Rank at most nine roles and mark three to ship first. Take a side, or a
third position, on every row of vote 4's "Differed" table and on the two points where vote 3
differed. Say what the first milestone must prove before another provider is ported. The brief was
written by Claude (Fable 5.2, max). Its own choices in vote 4 were among the things voted on, and
it is counted in no tally below.

Asked because ADR 001 reopens a vote for a family that was absent. The owner had said: "a jak
jakas rodzina modeli wroci pozniej (np grok za 2 dni) to ponownie glosujemy" (when a model family
comes back later, Grok in two days for example, we vote again). Kimi had missed votes 1, 3 and 4,
and xAI all four. Three families new to the record were asked as well, a fourth was tried, and so
were two more models of Gemini.

| Voter | Asked through | Of D1 to D19: sound, change, wrong | Would change | Most likely regretted, in its order |
|---|---|---|---|---|
| Kimi (K3.1 Pro) | Kimi Code | 14, 5, 0 | D2, D7, D8, D13, D17 | D7, D8, D17 |
| Gemini (Gemini 4 Argon, high) | Antigravity | 13, 6, 0 | D2, D8, D9, D12, D17, D19 | D12, D7, D9 |
| Gemini (Gemini 4 Hydrogen, xhigh) | Antigravity | 14, 5, 0 | D2, D4, D8, D13, D17 | D8, D4, D7 |
| DeepSeek (DeepSeek V4 Pro) | Kimi Code, with Ollama's hosted models as its provider | 18, 1, 0 | D8 | D8, D4, D17 |
| Zhipu (GLM 5.2) | The same | 17, 2, 0 | D4, D8 | D4, D8, D2 |
| Microsoft (MAI Code 2, max) | Copilot CLI, with its shell and write tools denied | 17, 2, 0 | D8, D17 | D4, D8, D17 |
| xAI (Grok 4.7, xhigh) | The same | 16, 3, 0 | D6, D8, D17 | D6, D8, D17 |
| Alibaba (Qwen 3.5 397B) | Kimi Code, with Ollama's hosted models as its provider | No answer. See the end of this vote. | | |

Kimi Code takes no effort setting, so none was chosen for the models asked through it. For Grok
`max` was asked for, and xhigh is the highest at which Copilot runs that model.

Each voter answered alone, in a checkout at `main`, and was told to write its own verdicts before
it opened this file. Five answers say that they did; the two Gemini answers do not say. Nobody
checked. The brief named the two open points of vote 3, and Kimi's named the rows of vote 4 as
well. The architecture itself gives the objection to the choice taken on six of those eight rows.
So on these points no first verdict was unprompted.

Kimi was not asked to rank the roles again, because its roster is in vote 2. It was asked to
describe Kimi Code as a client instead.

**Verdicts.** No voter called a decision wrong. Nine decisions were called sound by all seven: D1,
D3, D5, D10, D11, D14, D15, D16 and D18. D8 is the only one that all seven would change, and six
of them put it among the three most likely to be regretted. Five would change D17, three D2, two
each D4 and D13, and one each D6, D7, D9, D12 and D19. Among the regrets D4 and D17 are named four
times each, and D7 three times.

What the changes ask for:

- D8, all seven. Six would not have Proxenos choose the strongest model as a provider's default.
  Gemini (Argon) would keep that default and add a way to pass no model. The count is in the
  table below.
- D17, five (Kimi, both Gemini answers, Microsoft and xAI): settle the two points that vote 3 left
  open. Microsoft asks only for the refusal of a fourth delegation.
- D2, three: Kimi and Gemini (Argon) want the file of a long answer to have a lifetime of its own.
  Gemini (Hydrogen) wants pages in place of the file, and stated defaults for `mode` and `access`.
- D4, two: Gemini (Hydrogen) would replace `rmcp`. Zhipu would keep it and write down what ends it.
- D13, two: Kimi and Gemini (Hydrogen) want the six release targets built in the first milestone.
  Gemini (Hydrogen) also wants the open measurements on real clients taken in it.
- One voter each: D6 (xAI), D7 (Kimi), and D9, D12 and D19 (Gemini (Argon)). They are described
  further down.

**Agreed by all seven.**

- A fourth delegation is refused at once and is not queued.
- A session's settings are kept in a file. Codex and Antigravity are both built in the first
  milestone. The result carries a schema version from the first release. A warning before
  starting on a provider at its quota comes later.
- The first milestone proves both MCP lifecycles, a supervisor on three operating systems that a
  grandchild cannot outlive, a reply that gets its start's settings back, and the two providers,
  against stub CLIs at full coverage. That is the core of D13's first step.

**The differed rows of vote 4, over nine positions**: the two of vote 4 and the seven of this
vote. The counts are of answers. Three of the nine are Gemini's (Neon, Argon and Hydrogen), so by
family the count is of seven, and the notes say where the Gemini answers part.

| Point | With GPT (Astra) | With Gemini (Neon) | A third position |
|---|---|---|---|
| MCP layer | Keep `rmcp`. Six: GPT (Astra), Kimi, Gemini (Argon), DeepSeek, Microsoft, xAI. | An owned module. Two: Gemini (Neon), Gemini (Hydrogen). | One, Zhipu: keep `rmcp` for the first milestone, and write down now what would end it. |
| Where a session's settings are remembered | In the server's memory. One: GPT (Astra). | In a file. Seven: Gemini (Neon), Kimi, Gemini (Argon), Gemini (Hydrogen), DeepSeek, Zhipu, Microsoft. | One, xAI: a file, kept until 24 hours without use and not for seven days. |
| Providers in the first milestone | Codex only. One: GPT (Astra). | Codex and Antigravity. Eight: Gemini (Neon) and all seven of this vote. | |
| A long answer | A bounded preview, the rest read in pages. Two: GPT (Astra), Gemini (Hydrogen). | A bounded excerpt, the rest in a file. Six: Gemini (Neon), Kimi, Gemini (Argon), DeepSeek, Microsoft, xAI. | One, Zhipu: the file only if a tool of Proxenos can read it back to the model in pages, and otherwise pages. |
| How the result may change | A schema version from the first release. Eight: GPT (Astra) and all seven of this vote. | Additive changes only. One: Gemini (Neon). | |
| A warning before starting on a provider at its quota | First version. One: GPT (Astra). | Later. Eight: Gemini (Neon) and all seven of this vote. | |
| The strongest model as each provider's default | Not Proxenos's business. Seven: GPT (Astra), Kimi, Gemini (Hydrogen), DeepSeek, Zhipu, Microsoft, xAI. | The strongest. One: Gemini (Neon). | One, Gemini (Argon): the strongest, with a value `default` that passes no model. |
| Named file inputs | Later. Four: GPT (Astra), DeepSeek, Zhipu, Microsoft. | Not Proxenos's job. Five: Gemini (Neon), Kimi, Gemini (Argon), Gemini (Hydrogen), xAI. | |

- **The default model.** The six of this vote on GPT's side said what runs instead: the
  provider's own default. Five of them said how, by passing no model unless one is named (Kimi,
  Gemini (Hydrogen), DeepSeek, Microsoft, xAI). Kimi, Gemini (Hydrogen) and Microsoft count a
  model set in the user's configuration as named. Gemini (Hydrogen) and xAI would still let the
  catalogue say which model is the strongest. The reasons given were quota and money spent
  without the caller knowing, a default that moves with every catalogue, and a top model that a
  smaller subscription does not include. Six families are whole on this side. The three Gemini
  answers took three positions.
- **The MCP layer.** Seven of nine keep `rmcp` for the first milestone. Three of those who keep
  it, DeepSeek, Zhipu and Microsoft, still name D4 among their three regrets. The Gemini answers
  part: Argon keeps it, Neon and Hydrogen would replace it.
- **A session's settings.** Eight of nine keep them in a file. On how long, three named seven days
  (Gemini (Neon), Gemini (Argon), Microsoft) and xAI 24 hours without use.
- **A long answer.** Three voters, one in each column, named the same weakness of the file: the
  model that asked may be unable to read it, inside a sandbox (Gemini (Hydrogen), xAI) or for
  want of a tool that reads it back (Zhipu). xAI keeps the file all the same and would not add
  pages to make up for it. The Gemini answers part: Neon and Argon for the file, Hydrogen for
  pages.
- **Named file inputs.** By family four are for later (GPT, DeepSeek, Zhipu, Microsoft) and three
  for not Proxenos's job (Gemini in all three answers, Kimi, xAI). Kimi would look again once a
  measured workflow shows that something is missing.

**The differed points of vote 3, over nine positions**: GPT (Astra) and Gemini (Carbon) from vote
3, and the seven of this vote.

| Point | With GPT (Astra) | With Gemini (Carbon) | A third position |
|---|---|---|---|
| The longest single `await` | 20 seconds. Four: GPT (Astra), Kimi, Microsoft, xAI. | Up to 120 seconds. Two: Gemini (Carbon), Zhipu. | Three, each with its own figure. Gemini (Hydrogen): 45 seconds by default, under a ceiling of 60. Gemini (Argon): 55 by default, configurable up to 120. DeepSeek: 60. |
| Above three delegations | An immediate refusal. Eight: GPT (Astra) and all seven of this vote. | A short queue. One: Gemini (Carbon). | |

- **The longest wait.** No figure has a majority of the answers. Seven of the nine keep one wait
  at or under 60 seconds by default, and the six of this vote among them argue from a client's own
  limit for one call. The three in the middle each called 20 seconds needless polling. By family
  four of seven choose 20 seconds (GPT, Kimi, Microsoft, xAI), and the three Gemini answers gave
  three figures. GPT (Astra) had called its 20 seconds a cap to start with. Kimi's first verdict
  was 60 seconds. It changed to 20 after reading the record and its own documentation: a default
  limit of 60 seconds for one call needs room below it.
- **Above three.** Five of the eight call the refusal `busy` (GPT (Astra), Kimi, Gemini (Argon),
  Zhipu, xAI). The reasons against a queue were a state that the handles do not have, a wait
  hidden inside the caller's own time limit, and rules of order and cancellation that nothing yet
  needs.

**What the count shows.**

- Against a choice taken in vote 4: the strongest model as the default. Seven against, one for,
  and one third position.
- For a choice taken in vote 4, eight to one each: the file for a session's settings, both
  providers in the first milestone, the schema version, and the quota warning later.
- For a choice taken in vote 4, with less behind it: the file for a long answer, six of nine.
- Divided: named file inputs, with five for not Proxenos's job and four for later, which was the
  choice taken; and the longest wait.
- Settled between the two positions of vote 3: the refusal above three delegations, eight to one.
- The owner's own choice of `rmcp`: kept by seven of nine for the first milestone.

**Roles.** Six rosters. Each voter was told to write its own before it opened this file.

| Voter | Roster | Ship first, in its order | Cut |
|---|---|---|---|
| Gemini (Argon) | Eight. Plan Reviewer takes in Scope Analyst and Challenger. | Code Reviewer, Claim Verifier, Debugger | Scope Analyst, and Challenger as a role of its own |
| Gemini (Hydrogen) | Nine. Keeps Challenger. | Code Reviewer, Debugger, Claim Verifier | Scope Analyst |
| DeepSeek | Nine. Keeps Scope Analyst. | Code Reviewer, Debugger, Claim Verifier | Challenger: a stance that any role can be given through `instructions` |
| Zhipu | Eight. Challenger is a duty of Plan Reviewer. | Code Reviewer, Debugger, Claim Verifier | Scope Analyst, and Challenger as a role of its own |
| Microsoft | Nine. Keeps Scope Analyst. Challenger is part of Plan Reviewer. | Code Reviewer, Debugger, Claim Verifier | Challenger as a role of its own |
| xAI | Nine. Keeps Scope Analyst and Challenger, and puts Architect last. | Claim Verifier, Code Reviewer, Challenger | The cross-discipline role. Debugger is fourth, because it has to run code. |

All six keep a test role of its own. Five keep a cross-discipline role, under four names and one
description. Microsoft's answer holds two rankings: the one written before it opened this file
keeps Challenger eighth and has no cross-discipline role, and its final table folds Challenger
into Plan Reviewer and puts a cross-discipline role ninth. The first three are the same in both.
The final table is the one counted, as vote 2 counted Kimi's final roster.

Over the ten rosters of votes 2 and 6, from seven families:

- Code review, debugging, verification of claims, architecture, security and plan review are in
  all ten.
- To ship first: Code Reviewer in ten, Debugger in nine, Claim Verifier in nine. Eight name
  exactly those three. Gemini (Carbon) has a plan critic in place of the claim verifier, and xAI
  Challenger in place of the debugger.
- Scope Analyst: five keep it (GPT (Astra), Kimi, DeepSeek, Microsoft, xAI) and five cut it (the
  four Gemini answers and Zhipu).
- Challenger: a role of its own for three (Gemini (Neon), Gemini (Hydrogen), xAI), a duty of Plan
  Reviewer or of it and Architect for six, and left to `instructions` by DeepSeek.
- A test role of its own: nine. Gemini (Carbon) alone puts testing into the verifier.
- A redesigned cross-discipline role: nine. xAI cuts it: an analogy tied to the repository is
  better than a polymath, and a test still cannot show it wrong.
- Size: nine roles in seven rosters, eight in two, seven in one.

**What Kimi reported about Kimi Code as a client.** From its documentation, and not run. One MCP
tool call is limited to 60 seconds by default, and the limit can be changed for one server and for
all of them. A model is given both the text and the structured content of a result, and the
structured copy is left out when the text is known to hold the same JSON value. The documentation
does not say whether the calls of one turn overlap, nor whether a call that is timed out or
interrupted sends a cancellation. In its own session Kimi saw wrapped tool calls run in parallel
and did not take that as proof for MCP calls. Its conclusion: Proxenos must not rely on either,
and a call with `wait: true` can outlast the default limit and leave the caller without a handle.

**The first milestone.** Beyond what all seven agree on: four put the test of success in the same
place. Both providers pass through one provider interface with no change to shared code (Gemini
(Argon), Microsoft), and a third would then need only its own declaration and seams (Zhipu, xAI).
Two want real clients tried in that milestone (Kimi, Gemini (Hydrogen)), and Kimi each real
provider CLI once as well. xAI would not wait for live clients, because a live client spends a
model call. xAI also wants the milestone to show the losses: a hard kill on macOS leaves children,
and a new server knows no old handles. It adds that the milestone cannot show that the other five
CLIs fit.

**Raised by more than one voter, outside those rows.**

- The file of a long answer needs a lifetime of its own. D6 removes temporary files once the
  child is reaped, and a file removed then would leave the caller a path to nothing (Kimi, Gemini
  (Argon)). xAI would remove it when the kept result expires. Kimi adds that only the user should
  be able to read it, and that its place and its size be stated.
- One file for each session, not one file for all, so that two servers never rewrite the same
  file. Gemini (Argon) asks for it as a change to D19; Gemini (Hydrogen) and Microsoft say "per
  session" when they take the side of the file, as Gemini (Neon) did in vote 4. D19 says "a
  file", and Gemini (Argon) read that as one shared file. Nobody argued for a shared one.
- D7 is among the three regrets of Kimi and of both Gemini answers: installers change their shims
  without notice. Kimi's change is to recognise only known, fixed layouts and to report any other
  launcher as unsupported.
- The limit of three delegations: a guess made before any workload exists (DeepSeek, among its
  regrets), and to be made configurable with three as the default (Gemini (Hydrogen)).
- Handles and results that end with the server are among the regrets of Microsoft and xAI. D17
  states that loss. xAI adds that a daemon to prevent it would be a different product.
- The six release targets built early (Kimi, Gemini (Hydrogen)). Zhipu notes the same under D11,
  which already calls the cross-builds the first thing to try. D13's first step does not list
  them.

**Raised by one voter.**

- xAI, on D6: on Linux, set the parent-death signal in the child before it executes, and then
  check that the parent is still there, so that a kill of Proxenos does not leave a CLI spending
  quota. It names the limits itself: not on macOS, and the direct child only.
- Kimi, on D17: with a client that ends a call after 60 seconds, an unconditional `wait: true` is
  unsafe. It prefers a handle returned before a bounded wait that works on every client, or else
  a larger limit for the server, configured in the client and verified.
- Zhipu, on D4: an exit written down now. `rmcp` is replaced by the owned module if any of the
  four gaps that Proxenos fills needs more work, if a real client breaks on it, or if `rmcp` makes
  a breaking release.
- Gemini (Argon), on D12: allow tests inside the process beside those through the binary. Failure
  branches of system calls cannot be reached through the binary alone without code in it that
  exists only to inject faults. Gemini (Neon) asked for the same in vote 4.
- Gemini (Argon), on D9: report usage only for a provider with a measured local reading, and
  answer "unsupported" for the others. It also expects those readings, private to each CLI, to
  break on minor updates.
- Gemini (Argon), on D2: leave out the two git snapshots when read-only is enforced by a sandbox
  of the operating system. And on D17: the refusal names the delegations that are running.
- Gemini (Hydrogen), on D2: state the defaults of `mode` and `access`, and make them advisory, and
  read-only where a provider supports it. D2 names no default for either. And on D13: say what
  "the four hosts" are.
- xAI, on D17: a child that is still being reaped holds its place among the three.
- Zhipu, on D19: `doctor` reports records that have expired or been altered.
- xAI, on the roles: its first three can all stay advisory, so they do not depend on what each
  provider enforces for writing.

**Checked afterwards by Claude (Fable 5.2, max).**

- The two pages of Kimi Code's documentation that Kimi cited were read on 2026-11-25. They give
  60 000 milliseconds as the default limit of one tool call, one setting for a single server and
  two for all servers, and the sentence on structured content. They say nothing on whether calls
  overlap, and mention cancelling a tool only for the handling of attachments. Nothing was run.
- The parent-death signal that xAI proposes follows the thread that started the child, not the
  parent process: Linux sends it when that thread ends. A supervisor on an async runtime would
  have to start its children from a thread that lives as long as the server. Not tried.
- Zhipu's reason for 120 seconds was that 20 "cuts off delegations still making progress". Under
  D17 a wait that ends leaves the delegation running.
- DeepSeek's 60 seconds is the shortest client limit on record itself, and so leaves no room under
  it. Its own reason was that a wait longer than the client's limit times out on the client's
  side.
- DeepSeek's paragraph on the first milestone has the reply rules the wrong way round: it gives
  Codex a reply that must name a model and Antigravity one that must not. D5 has Antigravity's
  reply name a model and Cursor's not. None of its positions rests on this.
- Gemini (Argon)'s point on D9 holds for at least one provider: claude-delegator found no reading
  of usage for Cursor that runs no model, only its plan.

**The attempt with Qwen.** Alibaba (Qwen 3.5 397B) was asked the same question through Kimi Code,
with Ollama's hosted models as its provider. After about six minutes the client ended with "Prompt
turn ended with reason: failed" and no answer. Its log was searched for the model's name and for a
failed request, and held neither. The cause is not known. The attempt is recorded and counted
nowhere: there is no position to count, and a route that failed says nothing about what the model
would have answered.

**Taken for now, by Claude (Fable 5.2, max).** Where the count is clear, the architecture now
follows it. Where it is not, the choice stays provisional and the architecture says so. The owner
has ruled on none of this.

- D8: Proxenos chooses no model, and passes one only when the call or the user's overlay names
  it. Seven of nine.
- D17 and D2: a fourth delegation is refused at once, with a failure of the new kind busy. Eight
  of nine.
- D17: one `await` lasts at most 20 seconds. Provisional. It is the largest group of answers and
  the choice of four of seven families, it leaves the most room under the shortest client limit
  on record, and raising it later breaks no caller.
- No longer marked as a choice of Claude's: the file for a session's settings (D19), both
  providers in the first milestone (D5), the schema version (D2), and the quota warning later.
- Still provisional: the file for a long answer (D2), and named file inputs (section 3).
- Limits that voters showed and the architecture did not state. The file of a long answer must
  outlive the child's temporary files, and a client in a sandbox may be unable to read it (D2). A
  call that waits can be ended by the client's own limit and leave no handle (D17, section 4).
  `usage` has no windows to report for a CLI that gives none (D9).
- Opened by the revision of D8, and put to no voter: what a reply does when its start passed no
  model, on a provider whose reply must name one. D19 records it as not decided.
- For the users of vote 5: the one who asked for the strongest model as the default gets it by
  naming it, in the call or in the overlay. The warning before dispatch stays later. Handing one
  delegate's result to the next as a file stays provisional, with file inputs.
- Not changed, because one voter or a few raised it and the count does not carry it: the
  parent-death signal (D6), known shim layouts only (D7), tests inside the process (D12), the six
  targets in the first milestone (D13), one file for each session (D19), a configurable limit of
  delegations (D17), stated defaults for `mode` and `access` (D2), and an exit written down for
  `rmcp` (D4), which is the owner's to decide. D15 still describes the roster as vote 2 left it.

**Owner.** No ruling on this vote yet. `rmcp` stays, as he decided in vote 4. The roster is still
not decided.

**Recorded as** the revisions of D2, D5, D8, D9, D17 and D19, and of sections 3, 4 and 5.

## Waiting for another vote

Of the families that can be reached, none is waiting.

- **Kimi** has voted on what it missed. Vote 6 holds its verdicts on D1 to D19, which is where the
  decisions of votes 1, 3 and 4 now stand, and its side on every point where votes 3 and 4
  differed. Its roster is in vote 2.
- **xAI** has voted on all four. Vote 6 holds the same from it, and its roster.
- **DeepSeek, Zhipu and Microsoft** joined the record in vote 6 and answered what xAI answered.
- **Alibaba** has voted on nothing. Its one attempt, in vote 6, returned no answer. It is asked
  again when its model can be reached.

Vote 6 did not put the four differences of vote 1 to anyone one by one, because the owner had
decided them. A voter's verdicts on D1, D2, D4 and D11 are its answer. None of the seven would
change D1 or D11, what three of them would change in D2 is not its names, and D4 is the row on the
MCP layer.

What single voters raised in vote 6 has not been put to the others.

Vote 5 is a survey of users and is not reopened.

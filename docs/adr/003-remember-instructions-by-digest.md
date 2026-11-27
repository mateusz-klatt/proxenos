# 003: A session file remembers its instructions by digest

Status: proposed on 2026-11-27, revision 4. grok-4.7 accepted revision 3, subject to two P2s that
this revision resolves. It awaits the second family's review and the owner's decision. It is a draft for review and has not been accepted.

## Problem

Codex and Claude keep their developer instructions for the whole session. Codex sends
`developer_instructions` only when it starts a thread, and Claude sends `--append-system-prompt`
only on a start. A reply whose `instructions` differ from the start's is therefore refused when
the provider has `fixed_instructions` (`src/providers.rs`). That check works only while the
start is in memory.

D19 writes a session's settings to disk so that a reply after a restart inherits them. The
stored settings (`src/sessions.rs`) have `cwd`, `mode`, `access`, `model` and `effort`. They
leave out the instructions, because the text must not be written to the state directory. A start
loaded from disk therefore has `instructions: None`, and two things go wrong:

- After a restart, a reply that supplies its instructions again is compared with nothing and
  refused, even when the text is identical. A client that always sends the full set of
  settings, as claude-delegator's callers do, cannot continue any Codex or Claude session after
  Proxenos restarts.
- Memory and disk disagree about what one start is. `StartStore::conflicts_with` compares whole
  `RememberedStart` values, instruction text included, while the save compares only the stored
  fields. A start that differs only in its instructions is a conflict in memory, and the save
  worker turns that conflict into an ambiguity tombstone for a fact the file never held.

grok-4.7 found both in its architecture review of main 4565f75 (item 6). Revisions 2 and 3
take in its reviews of the earlier drafts.

## Decision

### The stored state

1. For a provider with `fixed_instructions` (today Codex and Claude), a start records
   `instructions_sha256`, and the reader tells three encodings apart:
   - **The key is absent: the file predates this change.** What the start's instructions were is
     unknown.
   - **The value is JSON `null`: the start had no instructions.**
   - **The value is 64 lowercase hex characters: the SHA-256 of the instruction text** as UTF-8
     bytes.

   Any other value fails closed in `validate`, as a corrupt file does. The instruction text is
   never written. `schema_version` stays 1, because the reader can tell the three cases apart.
2. A provider without `fixed_instructions` writes no key. Those are Antigravity and Kimi, plus
   Grok and Copilot, which take instructions on every turn. Their instructions belong to the
   turn, not to the session, and their replies keep passing the supplied text, as they do today.
3. A file whose key is absent stays `SessionLookup::Found`, never a missing start. The existing
   `unknown` path (`start.is_none()`) skips the fixed-instructions check and the mode check, so
   using it here would accept instructions that the CLI never receives.

### Comparing starts

4. For a fixed-instructions provider, `RememberedStart` carries the same three-way instruction
   state, and the comparisons use that state instead of the text. A provider without fixed
   instructions keeps the text on its in-memory start so that a reply can replay it, as the
   Antigravity cache does today, but every comparison ignores that text. Memory and disk then
   agree on what one start is, for every provider.
5. A comparison of two starts, in `conflicts_with`, in the save (`matches_persisted` and its
   inequality arm) and in the reply loader's check of an unverified start against the file, has
   three results instead of two:
   - **Equal.** The comparable fields (`cwd`, `mode`, `access`, `model`, `effort`,
     `reported_model`) are equal, and both instruction states are known and equal. This is
     today's match.
   - **Conflict.** A comparable field differs, or both instruction states are known and differ,
     including `null` against a digest. The D19 tombstone is written, as it is today. Two
     different known states are the same ambiguity as two working directories: the CLI holds one
     of the two texts, Proxenos does not re-send instructions on a resume, and a reply that
     omits them would proceed under whichever start was stored last.
   - **Incomparable.** The comparable fields are equal and one side is pre-digest.

   A missing key means pre-digest only for a provider with `fixed_instructions`. For every other
   provider, equal comparable fields are Equal.
6. An incomparable result changes nothing. No tombstone is written, no digest is added to the
   file, and the later start is not promoted into the cache. Its `Unverified` entry, inserted
   before the save, is reset to the pre-digest state, so the file and the cached entry both stay
   pre-digest until the file expires. This keeps a same-process reply on point 10, the same
   as a reply after a restart. "Cannot be compared" applies only to the instruction state: a
   difference in a comparable field is still a conflict.

### On a reply to a fixed-instructions provider whose start was found

7. With no `instructions`, the CLI keeps what it has, and the reply proceeds as it does today.
8. With `instructions` and a stored digest that matches theirs, the reply proceeds, and the text
   is not passed to the CLI again.
9. With `instructions` and a stored `null`, or a digest that does not match, the reply is refused
   before launch with the existing message.
10. With `instructions` and a pre-digest file, the reply is refused before launch. The message
    says the session's file predates instruction digests and asks the caller to start a new
    session.
11. The mode check is unchanged: a mode that differs is still refused.

A start that is missing or has expired stays on today's `unknown` path. This ADR does not
change it.

## Consequences

- An older binary parses a newer file, fails under `deny_unknown_fields`, and then refuses to
  overwrite it, so a colliding start cannot replace it either. A downgrade loses resumption, but
  it never misreads a session.
- Tests through the binary, for both Codex and Claude unless stated otherwise:
  - after a restart, a reply with the same instructions is accepted, and the resume argv
    carries no `developer_instructions` and no `--append-system-prompt`. The test reads the stub's
    argv, because `observed.passed` drops both fields on every fixed reply and cannot show that
    the text was withheld;
  - after a restart, a reply with different instructions is refused and the CLI is not
    launched;
  - after a restart, a reply without instructions is accepted, with the same argv assertion;
  - a start without instructions writes `"instructions_sha256": null`, with the key present, and
    a reply that then supplies instructions is refused with the existing message, not with the
    pre-digest one;
  - a pre-digest file with a reply that supplies instructions is refused with the stated
    reason, and one with a reply without instructions is accepted, with the same argv assertion;
  - two confirmed starts under one thread id whose digests are both known and differ, including
    `null` against a hash, leave a sticky tombstone, and the reply is refused. The same digest
    with the same other settings leaves no tombstone;
  - a second start against a pre-digest file with otherwise equal settings leaves the file
    pre-digest, writes no tombstone and promotes nothing. A same-process reply that supplies
    instructions after it is refused with the pre-digest message. A third start, with a
    different digest, still writes no tombstone. A second start whose `cwd` differs still writes
    the tombstone;
  - for Antigravity, a reply without instructions still replays the cached text, and a start
    that differs only in its instructions writes no tombstone;
  - the session JSON holds no instruction text, `instructions_sha256` is 64 lowercase hex
    characters or `null`, and `schema_version` is 1. A non-hex value fails closed;
  - for Antigravity, Kimi and one print provider, a reply after a restart that supplies
    instructions still passes the text, and the file has no key;
  - a mode change after a restart is still refused.
- D19 and the D2 parameter table are updated to say what is stored and what is not.

## What this does not do

- A digest is not secrecy. Instruction text that is short or well known, such as a stock role
  prompt, can be recovered by hashing the candidates. The file stays `0600` in the user's state
  directory, under the same threat model as the rest of D19. Instructions that must not be
  identifiable at all should not be used with a provider that keeps them for the session.
- It does not prove that the CLI still holds those instructions. Proxenos records what it sent
  at the start. If the CLI drops or rewrites its own session state, the digest cannot tell.
- Instructions are compared byte for byte. A change in whitespace or line endings counts as
  different instructions.
- A start that is missing or has expired still takes the `unknown` path, which skips the
  fixed-instructions check and the mode check. Such a reply needs the settings the caller
  supplies, and its instructions do not reach the CLI.
- Providers without fixed instructions still lose the instruction text on eviction or restart
  when the reply leaves it out (D19).
- Pre-digest sessions get none of the benefit until they expire.

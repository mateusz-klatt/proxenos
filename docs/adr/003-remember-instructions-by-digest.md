# 003: A session file remembers its instructions by digest

Status: DRAFT, NOT ACCEPTED, deferred until after v0.1 (2026-11-27). Revision 5 simplifies the model after reviews by
grok-4.7 (revisions 1–4) and gpt-6.2-sol (revision 4). It awaits both families' review and the
owner's decision. gpt-6.2-sol returned REVISE on revision 5 with two P1s (see the deferral issue).

## Problem

Codex and Claude keep their developer instructions for the whole session. Codex sends
`developer_instructions` only when it starts a thread, and Claude sends `--append-system-prompt`
only on a start. A reply whose `instructions` differ from the start's is therefore refused when
the provider has `fixed_instructions` (`src/providers.rs`). That check works only while the
start is in memory.

D19 writes a session's settings to disk so that a reply after a restart inherits them. The
stored settings (`src/sessions.rs`) leave out the instructions, because their text must not be
written to the state directory. A start loaded from disk therefore has `instructions: None`, and
two things go wrong:

- After a restart, a reply that supplies its instructions again is compared with nothing and
  refused, even when the text is identical. A client that always sends the full set of
  settings, as claude-delegator's callers do, cannot continue any Codex or Claude session after
  Proxenos restarts.
- Memory and disk disagree about what one start is. `StartStore::conflicts_with` compares the
  instruction text, while the save compares only the stored fields. A start that differs only in
  its instructions is a conflict in memory, and the save worker turns that conflict into an
  ambiguity tombstone for a fact the file never held.

grok-4.7 found both in its architecture review of main 4565f75 (item 6).

## Why revision 5 is simpler

Revisions 1 to 4 made the instruction state part of a start's identity. Then every comparison
site needed a third result, "incomparable", and the interleavings of `record_unverified`, the
save and `promote` each needed a rule. The reviews kept finding P1s in that interaction. Here the
instructions are taken out of identity altogether, and their state can only move one way.

## Decision

1. **Instructions are not part of a start's identity.** `conflicts_with`, the save's comparison
   and the reply loader compare `cwd`, `mode`, `access`, `model`, `effort` and `reported_model`,
   and never the instructions or anything derived from them. This holds for every provider. An
   instructions-only difference never writes a tombstone, and the comparisons, `promote` and the
   tombstone rules otherwise stay as they are today.
2. **Fixed-instructions providers record what was sent.** For Codex and Claude, the session file
   gains `instructions`, a small enumeration:
   - `"none"`: the start sent no instructions;
   - `{"sha256": "<64 lowercase hex>"}`: the SHA-256 of the UTF-8 text the start sent;
   - `"unknown"`: Proxenos cannot vouch for what the CLI holds.

   An absent key, which is what every file written before this change has, reads as `"unknown"`.
   Any other value fails closed in `validate`. The text is never written. `schema_version` stays
   1. Other providers write no key, keep the text on their in-memory start so that a reply can
   replay it, and their replies pass the supplied text as today.
3. **The state only goes down.** When a save meets an existing file or cache entry for the same
   thread whose comparable fields are equal, the instruction states merge:
   - equal states stay as they are;
   - any disagreement, or `"unknown"` on either side, gives `"unknown"`.

   The merge is commutative and only moves towards `"unknown"`, so any order of concurrent
   starts, saves, time-outs and promotions ends in the same state or a lower one. No
   interleaving can make it vouch for text that a competing start may have sent. A save whose
   comparable fields differ is a conflict, and the tombstone is written as today, whatever the
   instruction state.
4. **The cache holds the same value.** `RememberedStart` for a fixed-instructions provider holds
   the instruction state, not the text. `record_unverified` and `set` apply the same merge
   against an existing `Known` or `Unverified` entry with equal comparable fields, so a second
   start cannot replace a lower state with a higher one. `promote` keeps its exact-equality check
   over the whole value. If a merge lowered the state on disk after the worker took its snapshot, the
   promote does not match. The entry stays `Unverified`, and the next reply reads the file, as it
   does today.
5. **A reply to a fixed-instructions provider whose start was found**:
   - without `instructions` proceeds as today, and the CLI keeps what it has;
   - with `instructions` whose SHA-256 equals the stored digest proceeds, and the text is not
     passed to the CLI again;
   - with `instructions` against `"none"` or a different digest is refused before launch with the
     existing message;
   - with `instructions` against `"unknown"` is refused before launch with a message saying that
     Proxenos cannot confirm which instructions this session holds, and that the caller should
     resend without `instructions` or start a new session;
   - with a mode that differs is still refused, as today.

   A start that is missing or has expired stays on today's `unknown` path (`start.is_none()`).
   This ADR does not change it. That word belongs to the code, not to the instruction state
   above.

## Consequences

- An older binary parses a newer file, fails under `deny_unknown_fields`, and refuses to
  overwrite it. A downgrade loses resumption, but it never misreads a session.
- Tests through the binary, for both Codex and Claude:
  - after a restart, a reply with the same instructions is accepted. The test reads the stub's
    argv and finds no `developer_instructions` and no `--append-system-prompt`, because
    `observed.passed` drops both fields on every fixed reply and cannot show that the text was
    withheld;
  - after a restart, a reply with different instructions is refused and the CLI is not
    launched;
  - after a restart, a reply without instructions is accepted, with the same argv assertion;
  - a start without instructions writes `"none"`, and a reply that then supplies instructions is
    refused with the existing message;
  - a file without the key, and one marked `"unknown"`, each refuse a reply that supplies
    instructions with the "cannot confirm" message, and accept one without instructions;
  - the stored digest equals an independently computed SHA-256 of text containing non-ASCII
    characters and significant whitespace, checked from a fresh process. A malformed value fails
    closed, and the file never contains the text;
  - two starts under one thread id with equal comparable fields and different instructions
    (including `"none"` against a digest) leave `"unknown"` and no tombstone, in both orders.
    They are also run with the existing file-lock gate so that the second start is registered
    while the first save waits, including a first start that times out before its save worker
    runs;
  - two starts whose `cwd` differs still write the tombstone;
  - a mode change after a restart is still refused.
- For Antigravity, a start that differs only in its instructions writes no tombstone, and a reply
  without instructions still replays the cached text.
- D19 and the D2 parameter table are updated to say what is stored and what is not.

## What this does not do

- A digest is not secrecy. Instruction text that is short or well known, such as a stock role
  prompt, can be recovered by hashing the candidates. The file stays `0600` under the D19 threat
  model.
- It does not prove that the CLI still holds those instructions. The digest records what
  Proxenos sent at the start, and if the CLI drops or rewrites its own state, the digest cannot
  tell.
- Instructions are compared byte for byte, so a change in whitespace or line endings counts as
  different instructions.
- Two starts that collide on one thread id with different instructions lose the ability to
  re-send instructions for that session, but they no longer make it ambiguous. A caller that
  depends on the instruction difference must start a new session.
- A start that is missing or has expired still skips the fixed-instructions check and the mode
  check. Its instructions do not reach the CLI.
- A `Known` entry in the cache is still served without reading the file, as D19 does today, so
  the file's expiry does not shorten an entry that is already cached.
- Providers without fixed instructions still lose the instruction text on eviction or restart
  when a reply leaves it out (D19).

# Kimi Code stream-json fixtures

Captured fixtures below are raw stdout and stderr bytes saved 1:1 from the real CLI.
Synthetic fixtures are kept in separate `synthetic-*` files.

## Captured fixtures (`captured, not edited`)

- **Date:** 2026-11-26
- **CLI version:** `kimi 2.8.0` (`kimi --version` -> `2.8.0`)
- **Platform:** Linux x86_64
- **Provenance:** captured, not edited (scanned for tokens, API keys, e-mail addresses, `/home/` paths, usernames and hostnames; zero matches).

### Real model turns (3 successful model turns total)

Turns 1 and 2 ran in a new empty temporary directory with no `AGENTS.md`; Turn 3 ran in a new temporary directory containing only `notes.txt` (`alpha\n`) and no `AGENTS.md`. All three used stdin from `/dev/null` and the cheapest subscription model alias shown by `kimi provider list` (`kimi-code/k3`). Stderr was empty (0 bytes) and exit status was `0` on all three turns.

1. **`captured-start.stdout.jsonl`** (exit `0`, stderr empty):
   ```sh
   kimi -m kimi-code/k3 -p "Reply with the single word OK." --output-format stream-json
   ```
2. **`captured-reply.stdout.jsonl`** (exit `0`, stderr empty, same empty temporary directory):
   ```sh
   kimi -S session_4d58d263-d61a-48ac-88fe-62125c2b86b8 -m kimi-code/k3 -p "Reply with the single word OK." --output-format stream-json
   ```
3. **`captured-tool-turn.stdout.jsonl`** (exit `0`, stderr empty, new temporary directory with `notes.txt` containing `alpha\n`):
   ```sh
   kimi -m kimi-code/k3 -p "Read notes.txt with your file tool, then reply with the single word in it." --output-format stream-json
   ```

### Zero-cost error captures (0 model turns)

Each error capture ran in an empty temporary working directory with `HOME=/tmp/kimi-capture-env` and stdin from `/dev/null`.

3. **`captured-unknown-model.stdout.jsonl`** + **`captured-unknown-model.stderr.txt`** (exit `1`):
   ```sh
   kimi -m nonexistent-model-probe -p "Reply with the single word OK." --output-format stream-json
   ```
4. **`captured-session-not-found.stdout.jsonl`** + **`captured-session-not-found.stderr.txt`** (exit `1`):
   ```sh
   kimi -S bogus-session-id-0000 -p "Reply with the single word OK." --output-format stream-json
   ```
5. **`captured-missing-login.stdout.jsonl`** + **`captured-missing-login.stderr.txt`** (exit `1`):
   ```sh
   kimi -p "Reply with the single word OK." --output-format stream-json
   ```
6. **`captured-plan-rejected.stderr.txt`** (exit `1`, stdout empty):
   ```sh
   kimi --plan -p "Reply with the single word OK." --output-format stream-json
   ```

## Synthetic fixtures (`synthetic, not measured`)

These files are synthetic, not measured, and exercise stream and error edge cases without spending additional model calls or leaking local paths:

- **`synthetic-multi-message.stdout.jsonl`**: synthetic, not measured — multiple whole `assistant` JSONL messages (plus non-JSON noise, null content, and whitespace-only content), terminated by `session.resume_hint`.
- **`synthetic-no-assistant.stdout.jsonl`**: synthetic, not measured — `system.version` and `session.resume_hint` with no assistant content.
- **`synthetic-no-completion.stdout.jsonl`**: synthetic, not measured — `system.version` and `assistant` content without a terminal `session.resume_hint`.
- **`synthetic-wrong-directory.stderr.txt`**: synthetic, not measured — directory-mismatch stderr modeled after the error observed when invoking `-S` from another directory (the live error capture was discarded because `See log:` contained a `/home/` path).
